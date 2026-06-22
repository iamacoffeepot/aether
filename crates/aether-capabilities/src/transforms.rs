//! First-party native transforms (ADR-0048, issue 1464). A
//! `#[transform]` here links into both `aether-substrate-bundle` (the
//! headless binary's `TransformRegistry::from_inventory`) and
//! `aether-mcp` (`describe_transforms`), so the link-time inventory
//! submission populates both surfaces with no extra wiring.
//!
//! These ship in the production binaries — they are not `#[cfg(test)]`
//! like the `aether.fs` fetch fixtures' `double_fs` / `seed_fs` transforms.
//!
//! `mat4_apply` is ADR-0048's first first-party transform — a generic
//! linear-algebra node, unrelated to reachability. The space-time
//! reachability certifier transforms (`solve`, `build_corridor_graph`,
//! `aggregate_traffic`, …) moved to `aether-labyrinth` (issue 1908);
//! `mat4_apply` stays here.

use aether_data::transform;
use aether_kinds::Mat4Apply;
use aether_math::Vec4;

/// Apply a 4×4 matrix to a 4-vector, `M · v` (ADR-0048's first
/// first-party transform). `Mat4Apply` bundles both operands so the
/// transform stays a unary `Kind → Kind` node.
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
    use aether_data::{Kind, transforms};
    use aether_kinds::{Mat4Apply, descriptors};
    use aether_math::{Mat4, Vec4};

    #[test]
    fn identity_returns_the_input_vector() {
        let out = mat4_apply(Mat4Apply {
            matrix: Mat4::IDENTITY,
            vector: Vec4::new(1.0, 2.0, 3.0, 4.0),
        });
        assert_eq!(out, Vec4::new(1.0, 2.0, 3.0, 4.0));
    }

    #[test]
    fn scale_then_translate_applies_column_major() {
        // Column-major scale(2,3,4) + translate(5,6,7): the scale runs
        // down the diagonal, the translation in the LAST column (index
        // 12..16). Applied to the point (1,1,1,1) this is
        // (2·1+5, 3·1+6, 4·1+7, 1) = (7,9,11,1). A row-major / transposed
        // apply would read the translation from the bottom ROW instead
        // and miss it, so this pins the apply against that regression.
        let matrix = Mat4::from_cols_array([
            2.0, 0.0, 0.0, 0.0, //
            0.0, 3.0, 0.0, 0.0, //
            0.0, 0.0, 4.0, 0.0, //
            5.0, 6.0, 7.0, 1.0, //
        ]);
        let out = mat4_apply(Mat4Apply {
            matrix,
            vector: Vec4::new(1.0, 1.0, 1.0, 1.0),
        });
        assert_eq!(out, Vec4::new(7.0, 9.0, 11.0, 1.0));
    }

    #[test]
    fn registered_in_link_time_inventory() {
        // The contract `TransformRegistry::from_inventory` (headless)
        // and `describe_transforms` (aether-mcp) both read: the
        // transform is in the inventory, declares `Mat4Apply` as its one
        // input slot, and produces `Vec4`.
        let entry = transforms()
            .find(|t| t.name.ends_with("::mat4_apply"))
            .expect("mat4_apply not registered in link-time inventory");
        assert_eq!(entry.input_kind_ids, [Mat4Apply::ID]);
        assert_eq!(entry.output_kind_id, Vec4::ID);
    }

    #[test]
    fn input_and_output_kinds_resolve_distinctly() {
        // The input bundle and the output vector are separate kinds: a
        // shared kind id would alias the transform's input and output.
        // Both also surface through the substrate descriptor inventory
        // (the hub encodes `Mat4Apply` params; the transform produces the
        // `Vec4` output).
        assert_ne!(Mat4Apply::ID, Vec4::ID);
        let names: Vec<String> = descriptors::all().into_iter().map(|d| d.name).collect();
        assert!(names.iter().any(|n| n == Mat4Apply::NAME));
        assert!(names.iter().any(|n| n == Vec4::NAME));
    }
}
