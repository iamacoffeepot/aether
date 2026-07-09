//! Math transform input kind vocabulary.

use aether_math::{Mat4, Vec4};
use bytemuck::{Pod, Zeroable};

/// Input to the `mat4_apply` native transform (ADR-0048, issue 1464):
/// apply a 4×4 matrix to a 4-vector, `M · v`. Both operands ride in
/// one kind so the transform stays a unary `Kind → Kind` node — a
/// two-operand transform would need multi-input slot wiring.
///
/// `matrix` is the `aether_math::Mat4` operand (column-major, the same
/// layout as the substrate's `view_proj` uniform). `vector` is the
/// homogeneous `aether_math::Vec4` — the apply is a raw left-multiply
/// with the `w` weight carried and no perspective divide, so a point
/// (`w = 1`) picks up the translation column and a direction (`w = 0`)
/// does not.
///
/// Cast-shaped (`#[repr(C)]` + `Pod`, like `Vec4` and `ViewProjection`),
/// composing the math primitives directly rather than flattening them
/// to raw `[f32; N]` arrays. The `Kind` canonical encode/decode keeps
/// the transform boundary consistent: a source encodes its output and
/// the transform decodes its input through the same shape-agnostic
/// `Kind` path, so cast bytes agree on both sides.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.math.mat4_apply")]
pub struct Mat4Apply {
    pub matrix: Mat4,
    pub vector: Vec4,
}
