use crate::buffer::{BufferLayout, Color, ColorChannel, Descriptor, Texel};
use crate::program::{CompileError, Program};
use crate::pool::PoolImage;

/// A reference to one particular value.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Register(pub(crate) usize);

/// One linear sequence of instructions.
///
/// The machine model is a single basic block in SSA where registers are strongly typed with their
/// buffer descriptors.
///
/// *Why not a … stack machine*: The author believes that stack machines are a poor fit for image
/// editing in general. Their typical core assumption is that a) registers have the same size b)
/// copying them is cheap. Neither is true for images.
///
/// *Why not a … mutable model*: Because this would complicate the tracking of types. Due to the
/// lack of loops there is little reason for mutability anyways. If you wrap the program in a loop
/// to simulate branches yourself then each `launch` provides the opportunity to rebind the images
/// or bind an image to an output, where it can be mutated.
///
/// The strict typing and SSA-liveness analysis allows for a clean analysis of required temporary
/// resources, re-usage of those as an internal optimization, and in general simple analysis and
/// verification.
#[derive(Default)]
pub struct CommandBuffer {
    ops: Vec<Op>,
}

enum Op {
    /// i := in()
    Input {
        desc: Descriptor,
    },
    /// out(src)
    ///
    /// WIP: and is_cpu_type(desc)
    /// for the eventuality of gpu-only buffer layouts.
    Output { 
        src: Register,
    },
    /// i := op()
    /// where type(i) = desc
    Construct {
        desc: Descriptor,
        op: ConstructOp,
    },
    /// i := unary(src)
    /// where type(i) =? Op[type(src)]
    Unary {
        src: Register,
        op: UnaryOp,
        desc: Descriptor,
    },
    /// i := binary(lhs, rhs)
    /// where type(i) =? Op[type(lhs), type(rhs)]
    Binary {
        lhs: Register,
        rhs: Register,
        op: BinaryOp,
        desc: Descriptor,
    },
}

pub(crate) enum ConstructOp {
    // TODO: can optimize this repr for the common case.
    Solid(Vec<u8>),
}

/// Identifies one resources in the render pipeline, by an index.
#[derive(Clone, Copy)]
pub(crate) struct Texture(usize);

/// A high-level, device independent, translation of ops.
/// The main difference to Op is that this is no longer in SSA-form, and it may reinterpret and
/// reuse resources. In particular it will ran after the initial liveness analysis.
/// This will also return the _available_ strategies for one operation. For example, some texels
/// can not be represented on the GPU directly, depending on available formats, and need to be
/// either processed on the CPU (with SIMD hopefully) or they must be converted first, potentially
/// in a compute shader.
pub(crate) enum High {
    /// Assign a texture id to an input with given descriptor.
    Input(Texture, Descriptor),
    /// Designate the ith textures as output n, according to the position in sequence of outputs.
    Output(Texture),
    /// Instruct the machine to allocate the texture now.
    Allocate(Texture),
    /// Mark a texture as unneeded.
    Discard(Texture),
    Construct {
        dst: Texture,
        op: ConstructOp,
    },
    Unary {
        src: Texture,
        dst: Texture,
        op: UnaryOp,
    },
    Binary {
        lhs: Texture,
        rhs: Texture,
        dst: Texture,
        op: BinaryOp,
    },
}

pub(crate) enum UnaryOp {
    /// Op = id
    Affine(Affine),
    /// Op = id
    Crop(Rectangle),
    /// Op(color)[T] = T[.color=color]
    /// And color needs to be 'color compatible' with the prior T (see module).
    ColorConvert(Color),
    /// Op(T) = T[.color=select(channel, color)]
    Extract { channel: ColorChannel },
}

pub(crate) enum BinaryOp {
    /// Op[T, U] = T
    /// where T = U
    Inscribe { placement: Rectangle },
    /// Replace a channel T with U itself.
    /// Op[T, U] = T
    /// where select(channel, T.color) = U.color
    Inject { channel: ColorChannel }
}

/// A rectangle in `u32` space.
/// It's describe by minimum and maximum coordinates, inclusive and exclusive respectively. Any
/// rectangle where the order is not correct is interpreted as empty. This has the advantage of
/// simplifying certain operations that would otherwise need to check for correctness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rectangle {
    pub x: u32,
    pub y: u32,
    pub max_x: u32,
    pub max_y: u32,
}

#[non_exhaustive]
pub enum Blend {
    Alpha,
}

pub struct Affine {
    transformation: [f32; 9],
}

#[derive(Debug)]
pub struct CommandError {
    type_err: bool,
}

impl CommandBuffer {
    /// Declare an input.
    ///
    /// Inputs MUST later be bound from the pool during launch.
    pub fn input(&mut self, desc: Descriptor) -> Result<Register, CommandError> {
        if !desc.is_coherent() {
            return Err(CommandError::TYPE_ERR);
        }

        Ok(self.push(Op::Input { desc }))
    }

    /// Declare an image as input.
    ///
    /// Returns its register if the image has a valid descriptor, otherwise returns an error.
    pub fn input_from(&mut self, img: PoolImage)
        -> Result<Register, CommandError>
    {
        let descriptor = img.descriptor()
            .ok_or(CommandError::OTHER)?;
        self.input(descriptor)
    }

    /// Select a rectangular part of an image.
    pub fn crop(&mut self, src: Register, rect: Rectangle)
        -> Result<Register, CommandError>
    {
        let desc = self.describe_reg(src)?.clone();
        Ok(self.push(Op::Unary {
            src,
            op: UnaryOp::Crop(rect),
            desc,
        }))
    }

    /// Create an image with different color encoding.
    pub fn color_convert(&mut self, src: Register, texel: Texel)
        -> Result<Register, CommandError>
    {
        let desc_src = self.describe_reg(src)?;

        // Pretend that all colors with the same whitepoint will be mapped from encoded to
        // linear RGB when loading, and re-encoded in target format when storing them. This is
        // almost correct, but not all GPUs will support all texel kinds. In particular
        // some channel orders or bit-field channels are likely to be unsupported. In these
        // cases, we will later add some temporary conversion.
        match (&desc_src.texel.color, &texel.color) {
            (
                Color::Xyz { whitepoint: wp_src, .. },
                Color::Xyz { whitepoint: wp_dst, .. },
            ) if wp_src == wp_dst => {},
            _ => return Err(CommandError::TYPE_ERR),
        }

        // FIXME: validate memory condition.
        let layout = BufferLayout {
            width: desc_src.layout.width,
            height: desc_src.layout.height,
            bytes_per_texel: texel.samples.bits.bytes(),
        };

        let op = Op::Unary {
            src,
            op: UnaryOp::ColorConvert(texel.color.clone()),
            desc: Descriptor {
                layout,
                texel,
            },
        };

        Ok(self.push(op))
    }

    /// Embed this image as part of a larger one.
    pub fn inscribe(&mut self, below: Register, rect: Rectangle, above: Register)
        -> Result<Register, CommandError>
    {
        let desc_below = self.describe_reg(below)?;
        let desc_above = self.describe_reg(above)?;

        if desc_above.texel != desc_below.texel {
            return Err(CommandError::TYPE_ERR);
        }

        if Rectangle::with_layout(&desc_above.layout) != rect {
            return Err(CommandError::OTHER);
        }

        if !Rectangle::with_layout(&desc_below.layout).contains(rect) {
            return Err(CommandError::OTHER);
        }

        let op = Op::Binary {
            lhs: below,
            rhs: above,
            op: BinaryOp::Inscribe {
                placement: rect.normalize(),
            },
            desc: desc_below.clone(),
        };

        Ok(self.push(op))
    }

    /// Extract some channels from an image data into a new view.
    pub fn extract(&mut self, src: Register, channel: ColorChannel)
        -> Result<Register, CommandError>
    {
        let desc = self.describe_reg(src)?;
        let texel = desc.channel_texel(channel)
            .ok_or_else(|| CommandError::OTHER)?;
        let op = Op::Unary {
            src,
            op: UnaryOp::Extract { channel },
            desc: Descriptor {
                layout: desc.layout.clone(),
                texel,
            },
        };

        Ok(self.push(op))
    }

    /// Overwrite some channels with overlaid data.
    pub fn inject(&mut self, below: Register, channel: ColorChannel, above: Register)
        -> Result<Register, CommandError>
    {
        let desc_below = self.describe_reg(below)?;
        let expected_texel = desc_below.channel_texel(channel)
            .ok_or_else(|| CommandError::OTHER)?;
        let desc_above = self.describe_reg(above)?;

        if expected_texel != desc_above.texel {
            return Err(CommandError::TYPE_ERR);
        }

        let op = Op::Binary {
            lhs: below,
            rhs: above,
            op: BinaryOp::Inject { channel },
            desc: desc_below.clone(),
        };

        Ok(self.push(op))
    }

    /// Overlay this image as part of a larger one, performing blending.
    pub fn blend(&mut self, _below: Register, _rect: Rectangle, _above: Register, _blend: Blend)
        -> Result<Register, CommandError>
    {
        // TODO: What blending should we support
        Err(CommandError::OTHER)
    }

    /// A solid color image, from a descriptor and a single texel.
    pub fn solid(&mut self, describe: Descriptor, data: &[u8])
        -> Result<Register, CommandError>
    {
        if data.len() != describe.layout.bytes_per_texel {
            return Err(CommandError::TYPE_ERR);
        }

        Ok(self.push(Op::Construct {
            desc: describe,
            op: ConstructOp::Solid(data.to_owned()),
        }))
    }

    /// An affine transformation of the image.
    pub fn affine(&mut self, src: Register, affine: Affine)
        -> Result<Register, CommandError>
    {
        // TODO: should we check affine here?
        let desc = self.describe_reg(src)?.clone();
        Ok(self.push(Op::Unary {
            src,
            op: UnaryOp::Affine(affine),
            desc,
        }))
    }

    /// Declare an output.
    ///
    /// Outputs MUST later be bound from the pool during launch.
    pub fn output(&mut self, src: Register)
        -> Result<Descriptor, CommandError>
    {
        let outformat = self.describe_reg(src)?.clone();
        // Ignore this, it doesn't really produce a register.
        let _ = self.push(Op::Output {
            src,
        });
        Ok(outformat)
    }

    pub fn compile(&self) -> Result<Program, CompileError> {
        let steps = self.ops.len();

        let mut last_use = vec![0; steps];
        let mut first_use = vec![steps; steps];

        let mut high_ops = vec![];

        // Liveness analysis.
        for (back_idx, op) in self.ops.iter().rev().enumerate() {
            let idx = self.ops.len() - 1 - back_idx;
            match op {
                Op::Input { .. } | Op::Construct { .. } => {},
                &Op::Output { src: Register(src) } => {
                    last_use[src] = last_use[src].max(idx);
                    first_use[src] = first_use[src].min(idx);
                },
                &Op::Unary { src: Register(src), .. } => {
                    last_use[src] = last_use[src].max(idx);
                    first_use[src] = first_use[src].min(idx);
                },
                &Op::Binary { lhs: Register(lhs), rhs: Register(rhs), .. } => {
                    last_use[rhs] = last_use[rhs].max(idx);
                    first_use[rhs] = first_use[rhs].min(idx);
                    last_use[lhs] = last_use[lhs].max(idx);
                    first_use[lhs] = first_use[lhs].min(idx);
                },
            }
        }

        Ok(Program {
            ops: high_ops,
        })
    }

    fn describe_reg(&self, Register(reg): Register)
        -> Result<&Descriptor, CommandError>
    {
        match self.ops.get(reg) {
            None | Some(Op::Output { .. }) => {
                Err(CommandError::BAD_REGISTER)
            }
            Some(Op::Input { desc })
            | Some(Op::Construct { desc, .. })
            | Some(Op::Unary { desc, .. })
            | Some(Op::Binary { desc, .. }) => {
                Ok(desc)
            }
        }
    }

    fn push(&mut self, op: Op) -> Register {
        let reg = Register(self.ops.len());
        self.ops.push(op);
        reg
    }
}

impl Rectangle {
    /// A rectangle at the origin with given width (x) and height (y).
    pub fn with_width_height(width: u32, height: u32) -> Self {
        Rectangle { x: 0, y: 0, max_x: width, max_y: height }
    }

    /// A rectangle describing a complete buffer.
    pub fn with_layout(buffer: &BufferLayout) -> Self {
        Self::with_width_height(buffer.width, buffer.height)
    }

    /// The apparent width.
    pub fn width(self) -> u32 {
        self.max_x.saturating_sub(self.x)
    }

    /// The apparent height.
    pub fn height(self) -> u32 {
        self.max_y.saturating_sub(self.y)
    }

    /// Return true if this rectangle fully contains `other`.
    pub fn contains(self, other: Self) -> bool {
        self.x <= other.x && self.y <= other.y && {
            // Offsets are surely non-wrapping.
            let offset_x = other.x - self.x;
            let offset_y = other.y - self.y;
            let rel_width = self.width().checked_sub(offset_x);
            let rel_height = self.height().checked_sub(offset_y);
            rel_width >= Some(other.width()) && rel_height >= Some(other.height())
        }
    }

    /// Bring the rectangle into normalized form where minimum and maximum form a true interval.
    #[must_use]
    pub fn normalize(self) -> Rectangle {
        Rectangle {
            x: self.x,
            y: self.y,
            max_x: self.x + self.width(),
            max_y: self.y + self.width(),
        }
    }

    /// A rectangle that the overlap of the two.
    #[must_use]
    pub fn meet(self, other: Self) -> Rectangle {
        Rectangle {
            x: self.x.max(other.x),
            y: self.y.max(other.y),
            max_x: self.max_x.min(other.max_x),
            max_y: self.max_y.min(other.max_y),
        }
    }

    /// A rectangle that contains both.
    #[must_use]
    pub fn join(self, other: Self) -> Rectangle {
        Rectangle {
            x: self.x.min(other.x),
            y: self.y.min(other.y),
            max_x: self.max_x.max(other.max_x),
            max_y: self.max_y.max(other.max_y),
        }
    }

    /// Remove border from all sides.
    /// When the image is smaller than `border` in some dimension then the result is empty and
    /// contained in the original image but otherwise unspecified.
    #[must_use]
    pub fn inset(self, border: u32) -> Self {
        Rectangle {
            x: self.x.saturating_add(border),
            y: self.y.saturating_add(border),
            max_x: self.max_x.saturating_sub(border),
            max_y: self.max_y.saturating_sub(border),
        }
    }
}

impl CommandError {
    /// Indicates a very generic type error.
    const TYPE_ERR: Self = CommandError {
        type_err: true,
    };

    /// Indicates a very generic other error.
    /// E.g. the usage of a command requires an extension? Not quite sure yet.
    const OTHER: Self = CommandError {
        type_err: false,
    };

    /// Specifies that a register reference was invalid.
    const BAD_REGISTER: Self = Self::OTHER;

    pub fn is_type_err(&self) -> bool {
        self.type_err
    }
}
