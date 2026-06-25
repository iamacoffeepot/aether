//! Spike consumer — the identity/runtime split the macro is meant to enable.
//!
//! Everything here is the always-on *identity* surface: the marker trait,
//! the kind vocabulary, the cap ZST. The dispatcher + state live in
//! `runtime.rs`, gated behind `feature = "runtime"`. `#[pull_up]` reads
//! `runtime.rs` off disk and lifts the per-kind markers up to here, so the
//! typed-send compile gate works even in a `--no-default-features` build
//! where `mod runtime` is stripped entirely.

/// Always-on addressing marker — stand-in for `aether_actor::HandlesKind<K>`.
pub trait Handles<K> {}

/// Always-on kind vocabulary — stand-in for a cap's mail kinds.
pub struct Tick;
pub struct Resize;

/// The cap identity ZST — always-on, names no runtime/substrate types.
pub struct RenderCapability;

// Plain, hand-gated module declaration — no macro touches it (an attribute
// macro on a file module is unstable, rust#54727; see README).
#[cfg(feature = "runtime")]
mod runtime;

// The function-like macro reads `runtime.rs` off disk, harvests the
// `#[handler]` kinds, and emits `impl Handles<Tick> for RenderCapability {}`
// + `impl Handles<Resize> for RenderCapability {}` here — always-on, so the
// markers survive a build where `mod runtime` above is `#[cfg]`-stripped.
pull_up_macro::lift_markers!(runtime => RenderCapability);

// Compile-time proof that the markers exist regardless of the `runtime`
// feature. The inner turbofish call forces rustc to *prove* the bounds at
// this site (a bare `where` clause on an uncalled fn would only be assumed,
// not proven). This body is type-checked even though it is never invoked, so
// a `--no-default-features` build that strips `mod runtime` still has to find
// the lifted `impl Handles<_> for RenderCapability` impls — exactly the
// property the split needs.
#[allow(dead_code)]
fn _assert_markers_present() {
    fn requires<T: Handles<Tick> + Handles<Resize>>() {}
    requires::<RenderCapability>();
}
