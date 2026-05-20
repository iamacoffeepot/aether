//! `#[transform]` macro tests (ADR-0048 §1, iamacoffeepot/aether#979).
//!
//! The trybuild fixtures under `tests/ui/` exercise the macro's
//! accept/reject surface: a pure single-input body and a multi-input
//! body compile; a `Ctx` reference, a host-fn call, a `std::time` read,
//! and a 9th parameter are rejected with the deny-list / cap
//! `compile_error!`. The `transform_determinism` test calls a real
//! generated `invoke` thunk twice on identical bytes and asserts
//! byte-identical output (the content-addressing precondition).

#![allow(clippy::unwrap_used)]

use aether_data::transform;

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/accepts_pure_body.rs");
    t.pass("tests/ui/accepts_multi_input.rs");
    t.compile_fail("tests/ui/rejects_ctx_param.rs");
    t.compile_fail("tests/ui/rejects_host_fn.rs");
    t.compile_fail("tests/ui/rejects_std_time.rs");
    t.compile_fail("tests/ui/rejects_nine_inputs.rs");
}

#[repr(C)]
#[derive(
    Copy, Clone, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "test.det_scalar")]
struct DetScalar {
    value: u32,
}

/// A pure transform whose generated `invoke` the determinism test
/// drives. Body is deterministic — same input bytes always produce the
/// same output bytes.
#[transform]
fn det_double(x: DetScalar) -> DetScalar {
    DetScalar {
        value: x.value.wrapping_mul(2),
    }
}

/// ADR-0048 §"Determinism": calling the generated `invoke` twice on
/// identical input bytes yields byte-identical output. This is the CI
/// determinism check the content-addressing scheme depends on.
#[test]
fn transform_determinism() {
    use aether_data::Kind;

    // Find the `det_double` entry in the link-time inventory.
    let entry = aether_data::transforms()
        .find(|e| e.name.ends_with("det_double"))
        .expect("det_double transform registered in link-time inventory");

    let input = DetScalar { value: 21 };
    let input_bytes = input.encode_into_bytes();
    let slices: [&[u8]; 1] = [input_bytes.as_slice()];

    let out_a = (entry.invoke)(&slices).expect("first invoke succeeds");
    let out_b = (entry.invoke)(&slices).expect("second invoke succeeds");
    assert_eq!(out_a, out_b, "transform output must be byte-deterministic");

    // Sanity: the decoded output is the doubled value.
    let decoded = DetScalar::decode_from_bytes(&out_a).expect("output decodes");
    assert_eq!(decoded, DetScalar { value: 42 });
}
