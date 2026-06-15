//! First-party native transforms (ADR-0048, issue 1464). A
//! `#[transform]` here links into both `aether-substrate-bundle` (the
//! headless binary's `TransformRegistry::from_inventory`) and
//! `aether-mcp` (`describe_transforms`), so the link-time inventory
//! submission populates both surfaces with no extra wiring.
//!
//! These ship in the production binaries — they are not `#[cfg(test)]`
//! like the DAG executor's `double` / `seed` fixtures.

use aether_data::transform;
use aether_kinds::{BudgetQuery, Mat4Apply, ReachabilityMargin, ReachabilityProblem, ScalarField};
use aether_math::Vec4;

use crate::reachability::{UNREACHABLE, solve_cost_to_reach};

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

/// Solve minimum-cost reachability over a time-varying scalar cost field
/// (ADR-0047/0048/0049, issue 1857). `ReachabilityProblem` bundles the
/// cost field, the movement stencil, and the start seed so the transform
/// stays a unary `Kind → Kind` node, the same shape `Mat4Apply` gives
/// `mat4_apply`. The output is the cost-to-reach field `V` — a
/// `ScalarField` of the same shape — so it is the single currency every
/// follow-on field pass consumes.
///
/// The body delegates to the pure [`solve_cost_to_reach`] core (the
/// reusable internal API the corridor-graph / windowed-replan / agent-
/// population passes call directly); the transform fn itself only
/// unbundles the operands and rewraps the result, so it clears the
/// `#[transform]` purity deny-list.
#[transform]
fn solve(problem: ReachabilityProblem) -> ScalarField {
    let ScalarField {
        width,
        height,
        ticks,
        values: costs,
    } = problem.cost;
    let values = solve_cost_to_reach(
        width as usize,
        height as usize,
        ticks as usize,
        &costs,
        &problem.stencil.offsets,
        &problem.start,
    );
    ScalarField {
        width,
        height,
        ticks,
        values,
    }
}

/// Threshold a solved cost-to-reach field against a budget (issue 1857).
/// Reads the minimum cost-to-reach over the field's final tick: the
/// cheapest cell reachable under the full horizon. `reachable` is that
/// minimum `< budget`; `margin` is `budget - minimum` as a signed value
/// (positive slack under budget, negative when even the cheapest reachable
/// cell exceeds it, or when no cell is reachable). Kept off `ScalarField`
/// itself so every downstream consumer is free of a readout it does not
/// use — a separate cached query transform composes better.
#[transform]
fn reachability_margin(field: ScalarField, query: BudgetQuery) -> ReachabilityMargin {
    let ScalarField {
        width,
        height,
        ticks,
        values,
    } = field;
    let plane = (width as usize).saturating_mul(height as usize);
    let min_cost = if ticks == 0 || plane == 0 {
        UNREACHABLE
    } else {
        let final_base = (ticks as usize - 1).saturating_mul(plane);
        values
            .get(final_base..final_base.saturating_add(plane))
            .and_then(|layer| layer.iter().copied().min())
            .unwrap_or(UNREACHABLE)
    };
    let reachable = min_cost < query.budget;
    let margin = i64::from(query.budget) - i64::from(min_cost);
    ReachabilityMargin {
        reachable,
        min_cost,
        margin,
    }
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
        // shared kind id would collide in the Ref-slot resolver. Both
        // also surface through the substrate descriptor inventory (the
        // hub encodes `Mat4Apply` params; the observer resolves the
        // `Ref<Vec4>` output).
        assert_ne!(Mat4Apply::ID, Vec4::ID);
        let names: Vec<String> = descriptors::all().into_iter().map(|d| d.name).collect();
        assert!(names.iter().any(|n| n == Mat4Apply::NAME));
        assert!(names.iter().any(|n| n == Vec4::NAME));
    }
}

#[cfg(test)]
mod reachability_transform_tests {
    use super::{reachability_margin, solve};
    use aether_data::{Kind, transforms};
    use aether_kinds::{
        BudgetQuery, MovementStencil, ReachabilityMargin, ReachabilityProblem, ScalarField,
        StencilOffset,
    };

    const UNREACHABLE: u32 = u32::MAX;

    fn stencil_4way() -> MovementStencil {
        MovementStencil {
            offsets: vec![
                StencilOffset { dx: 0, dy: 0 },
                StencilOffset { dx: 1, dy: 0 },
                StencilOffset { dx: -1, dy: 0 },
                StencilOffset { dx: 0, dy: 1 },
                StencilOffset { dx: 0, dy: -1 },
            ],
        }
    }

    /// 3×1 uniform-cost field, start at cell 0 — the same hand-checked
    /// field the solver-core tests pin, here driven through the transform.
    fn small_problem() -> ReachabilityProblem {
        ReachabilityProblem {
            cost: ScalarField {
                width: 3,
                height: 1,
                ticks: 3,
                values: vec![1u32; 9],
            },
            stencil: stencil_4way(),
            start: vec![0, UNREACHABLE, UNREACHABLE],
        }
    }

    #[test]
    fn solve_transform_produces_the_cost_to_reach_field() {
        let v = solve(small_problem());
        assert_eq!(v.width, 3);
        assert_eq!(v.height, 1);
        assert_eq!(v.ticks, 3);
        assert_eq!(
            v.values,
            vec![0, UNREACHABLE, UNREACHABLE, 1, 1, UNREACHABLE, 2, 2, 2]
        );
    }

    #[test]
    fn solve_registered_in_link_time_inventory() {
        // Same contract as `mat4_apply`: registered, one `ReachabilityProblem`
        // input slot, `ScalarField` output.
        let entry = transforms()
            .find(|t| t.name.ends_with("::solve"))
            .expect("solve not registered in link-time inventory");
        assert_eq!(entry.input_kind_ids, [ReachabilityProblem::ID]);
        assert_eq!(entry.output_kind_id, ScalarField::ID);
    }

    #[test]
    fn reachability_margin_registered_in_link_time_inventory() {
        // A two-input transform: `(ScalarField, BudgetQuery)` in slot order,
        // `ReachabilityMargin` out.
        let entry = transforms()
            .find(|t| t.name.ends_with("::reachability_margin"))
            .expect("reachability_margin not registered in link-time inventory");
        assert_eq!(entry.input_kind_ids, [ScalarField::ID, BudgetQuery::ID]);
        assert_eq!(entry.output_kind_id, ReachabilityMargin::ID);
    }

    #[test]
    fn reachability_kinds_resolve_distinctly() {
        let ids = [
            ReachabilityProblem::ID,
            ScalarField::ID,
            BudgetQuery::ID,
            ReachabilityMargin::ID,
            MovementStencil::ID,
        ];
        for (i, a) in ids.iter().enumerate() {
            for b in &ids[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn margin_threshold_is_reachable_under_budget() {
        // Final-tick minimum is 2 (every cell reaches cost 2 at t = 2).
        let field = solve(small_problem());
        let under = reachability_margin(field.clone(), BudgetQuery { budget: 5 });
        assert_eq!(
            under,
            ReachabilityMargin {
                reachable: true,
                min_cost: 2,
                margin: 3,
            }
        );
        let exact = reachability_margin(field.clone(), BudgetQuery { budget: 2 });
        // `< budget` is strict: a budget equal to the minimum is not under.
        assert_eq!(
            exact,
            ReachabilityMargin {
                reachable: false,
                min_cost: 2,
                margin: 0,
            }
        );
        let over = reachability_margin(field, BudgetQuery { budget: 1 });
        assert_eq!(
            over,
            ReachabilityMargin {
                reachable: false,
                min_cost: 2,
                margin: -1,
            }
        );
    }

    #[test]
    fn margin_on_all_unreachable_final_tick_is_not_reachable() {
        // A stay-only stencil leaves every non-start cell unreachable; with
        // no start cell at all, the whole final tick is the sentinel.
        let field = solve(ReachabilityProblem {
            cost: ScalarField {
                width: 2,
                height: 1,
                ticks: 2,
                values: vec![1u32; 4],
            },
            stencil: MovementStencil {
                offsets: vec![StencilOffset { dx: 0, dy: 0 }],
            },
            start: vec![UNREACHABLE, UNREACHABLE],
        });
        let margin = reachability_margin(field, BudgetQuery { budget: 100 });
        assert!(!margin.reachable);
        assert_eq!(margin.min_cost, UNREACHABLE);
    }

    #[test]
    fn solve_is_deterministic_and_content_addressable() {
        // Same input -> identical output, and the content-addressing path
        // (the encoded output bytes the executor keys on) replays
        // byte-for-byte.
        let a = solve(small_problem());
        let b = solve(small_problem());
        assert_eq!(a, b);
        assert_eq!(a.encode_into_bytes(), b.encode_into_bytes());
    }

    #[test]
    fn canonical_field_encodes_under_output_cap_and_round_trips() {
        // The canonical 64×64 grid over 1800 ticks (~7.4M cells): the
        // solved `ScalarField` must fit the 64MB transform output cap
        // (ADR-0048 §6) and round-trip byte-stable through the kind codec.
        const CAP: usize = 64 * 1024 * 1024;
        let width = 64u32;
        let height = 64u32;
        let ticks = 1800u32;
        let plane = (width * height) as usize;
        let problem = ReachabilityProblem {
            cost: ScalarField {
                width,
                height,
                ticks,
                values: vec![1u32; plane * ticks as usize],
            },
            stencil: stencil_4way(),
            start: {
                let mut s = vec![UNREACHABLE; plane];
                s[0] = 0;
                s
            },
        };
        let field = solve(problem);
        assert_eq!(field.values.len(), plane * ticks as usize);

        let bytes = field.encode_into_bytes();
        assert!(
            bytes.len() < CAP,
            "encoded canonical field is {} bytes, over the {CAP}-byte cap",
            bytes.len()
        );

        let back = ScalarField::decode_from_bytes(&bytes).expect("canonical field round-trips");
        assert_eq!(field, back);
    }
}
