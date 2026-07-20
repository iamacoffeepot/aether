//! GPU-gated test helpers for visual bench scenarios (issue #3765).
//! The wasm-locating half lives in
//! `aether_substrate_bench::test_helpers` (`require_wasm` and friends);
//! this module adds the wgpu adapter probe in front of it for scenarios
//! whose bench composes render.

use std::env;
use std::path::PathBuf;

// Union surface: re-export the core (GPU-free) helpers so a visual test
// imports its whole helper set from one module.
pub use aether_substrate_bench::test_helpers::{
    init_save_sandbox, locate_component_wasm, require_wasm, test_namespace_roots, write_fixture,
};

/// Probe for any usable wgpu adapter. Used by [`require_runtime`] and
/// by visual tests that need wgpu but no wasm component.
#[must_use]
pub fn has_wgpu_adapter() -> bool {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::default(),
        compatible_surface: None,
        force_fallback_adapter: false,
    }))
    .is_ok()
}

/// Skip-or-panic gate for visual scenarios: probes wgpu, then locates
/// the wasm via `require_wasm`. Returns the wasm path on success;
/// `None` when the test should skip.
///
/// `AETHER_REQUIRE_RUNTIME=1` flips both skip points into a panic so CI
/// catches a forgotten pre-build entry instead of passing a 30 ms
/// vacuous test. CI sets this; local devs leave it unset and keep the
/// skip behavior.
///
/// # Panics
/// Panics in strict (`AETHER_REQUIRE_RUNTIME=1`) mode if either no wgpu
/// adapter is available or the named crate's wasm artifact is not
/// pre-built — fail-fast per ADR-0063.
#[must_use]
// Test-only skip diagnostic — emitted from `cargo test` runners so a
// skipped test is visible alongside `test ... ok` lines (issue 891).
#[allow(clippy::print_stderr)]
// Test-only: AETHER_REQUIRE_RUNTIME is the CI strict-mode toggle, a test
// harness knob, not cap config.
#[allow(clippy::disallowed_methods)]
pub fn require_runtime(crate_name: &str) -> Option<PathBuf> {
    let strict = env::var("AETHER_REQUIRE_RUNTIME").is_ok();
    if !has_wgpu_adapter() {
        assert!(!strict, "AETHER_REQUIRE_RUNTIME set but no wgpu adapter available");
        eprintln!("skipping: no wgpu adapter available");
        return None;
    }
    require_wasm(crate_name)
}
