//! First-party native transforms (ADR-0048, issue 1464). A
//! `#[transform]` here links into both `aether-substrate-bundle` (the
//! headless binary's `TransformRegistry::from_inventory`) and
//! `aether-mcp` (`describe_transforms`), so the link-time inventory
//! submission populates both surfaces with no extra wiring. Co-located
//! with the `Mat4Apply` kind it consumes.
//!
//! Native-only: the `aether.fs` `fetch` verb that runs transforms is
//! non-wasm, and no wasm consumer runs transforms, so the whole module
//! is `#[cfg(not(target_family = "wasm"))]`-gated at its `mod`
//! declaration rather than carried dead on the wasm-header-only build.
//!
//! `mat4_apply` is ADR-0048's first first-party transform — a generic
//! linear-algebra node.

use crate::Mat4Apply;
use aether_data::transform;
use aether_math::Vec4;

/// Apply a 4×4 matrix to a 4-vector, `M · v` (ADR-0048's first
/// first-party transform). `Mat4Apply` bundles both operands into one
/// input so the transform stays a unary `Kind → Kind` node.
///
/// Column-major + homogeneous: `matrix` is column-major (matching
/// `aether_math::Mat4` and the substrate's `view_proj` uniform), and
/// the multiply carries `w` with no perspective divide — a raw
/// left-multiply. `Mat4Apply` composes the math primitives directly,
/// so the body is the `Mat4 * Vec4` operator with no array rebuild.
///
/// Pure arithmetic, so it clears the `#[transform]` purity deny-list:
/// no host fn, no `Ctx`, no `std::time` / `std::env`.
#[transform]
fn mat4_apply(input: Mat4Apply) -> Vec4 {
    input.matrix * input.vector
}

#[cfg(test)]
mod tests {
    use super::mat4_apply;
    use crate::Mat4Apply;
    use aether_data::Kind;
    use aether_math::{Mat4, Vec4};

    #[test]
    fn scale_then_translate_applies_column_major() {
        // Column-major scale(2,3,4) + translate(5,6,7): the scale runs
        // down the diagonal, the translation in the LAST column (index
        // 12..16). Applied to the point (1,1,1,1) this is
        // (2·1+5, 3·1+6, 4·1+7, 1) = (7,9,11,1). A row-major / transposed
        // apply would read the translation from the bottom ROW instead
        // and silently drop it, so this pins the apply against that
        // regression.
        let matrix = Mat4::from_cols_array([
            2.0, 0.0, 0.0, 0.0, //
            0.0, 3.0, 0.0, 0.0, //
            0.0, 0.0, 4.0, 0.0, //
            5.0, 6.0, 7.0, 1.0, //
        ]);
        let applied = mat4_apply(Mat4Apply { matrix, vector: Vec4::new(1.0, 1.0, 1.0, 1.0) });
        assert_eq!(applied, Vec4::new(7.0, 9.0, 11.0, 1.0));
    }

    /// `aether.math.mat4_apply` is the wire name the hub encodes
    /// agent-supplied params against and `describe_transforms` surfaces
    /// for this transform's input (ADR-0048); pin it so an accidental
    /// rename is caught here rather than as a routing miss at the first
    /// live send.
    #[test]
    fn mat4_apply_kind_name_is_stable() {
        assert_eq!(Mat4Apply::NAME, "aether.math.mat4_apply");
    }
}
