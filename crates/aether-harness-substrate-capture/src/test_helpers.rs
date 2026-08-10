//! GPU-gated test helpers for visual harness scenarios (issue #3765).
//! The wasm-locating half lives in
//! `aether_harness_substrate::test_helpers` (`require_wasm` and friends);
//! this module adds the wgpu adapter probe in front of it for scenarios
//! whose harness composes render.

use std::env;
use std::path::PathBuf;

// Union surface: re-export the core (GPU-free) helpers so a visual test
// imports its whole helper set from one module.
pub use aether_harness_substrate::test_helpers::{
    envelope, init_save_sandbox, locate_component_wasm, require_wasm, test_namespace_roots, write_fixture,
};
use aether_render::{
    InputSlot, OutputSlot, PassStage, ProgramPass, ProgramRegister, SlotExtent, SlotSpec, TextureFormat,
};

use crate::visual::Image;

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

/// The RGBA quadruple at `(x, y)` in a decoded capture.
///
/// The three helpers below are the pixel-assertion vocabulary every visual
/// scenario reaches for first, and had been carried as byte-identical copies
/// in `aether-render` and `aether-text` (issue 4131). They live here rather
/// than in the base harness because `Image` does: the capture crate is
/// downstream of it.
///
/// # Panics
/// Panics if `(x, y)` is outside the image.
#[must_use]
pub fn rgba_at(img: &Image, x: u32, y: u32) -> [u8; 4] {
    let start = ((y * img.width + x) * 4) as usize;
    [img.rgba[start], img.rgba[start + 1], img.rgba[start + 2], img.rgba[start + 3]]
}

/// Whether `actual`'s colour channels are all within `tolerance` of
/// `expected`. Alpha is ignored — a capture's background is opaque and the
/// comparison is about colour.
#[must_use]
pub fn rgb_close(actual: [u8; 4], expected: [u8; 3], tolerance: u8) -> bool {
    actual[..3].iter().zip(expected).all(|(actual, expected)| actual.abs_diff(expected) <= tolerance)
}

/// Whether the pixel at `(x, y)` differs from the background beyond
/// `tolerance` — i.e. something was drawn there.
///
/// # Panics
/// Panics if `(x, y)` is outside the image.
#[must_use]
pub fn pixel_is_lit(img: &Image, x: u32, y: u32, bg: [u8; 3], tolerance: u8) -> bool {
    !rgb_close(rgba_at(img, x, y), bg, tolerance)
}

/// Decode one sRGB capture byte into its linear channel value.
#[must_use]
pub fn srgb_byte_to_linear(byte: u8) -> f32 {
    let encoded = f32::from(byte) / 255.0;
    if encoded <= 0.04045 {
        encoded / 12.92
    } else {
        ((encoded + 0.055) / 1.055).powf(2.4)
    }
}

/// Append a scenario-owned capture probe to an authored render program.
///
/// The helper owns only the repeated registration choreography. The probe
/// shader and its inputs remain with the scenario that defines what is being
/// observed.
///
/// # Panics
///
/// Panics if the register contains more bindings than can be addressed by a
/// `u32` program slot.
#[must_use]
pub fn append_capture_probe(
    register: &mut ProgramRegister,
    probe_wgsl: &str,
    inputs: Vec<InputSlot>,
    uniform_length: u32,
) -> u32 {
    let output = u32::try_from(register.bindings.len()).expect("program binding count exceeds u32");

    register.wgsl.push('\n');
    register.wgsl.push_str(probe_wgsl);
    register.bindings.push(SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full });
    register.passes.push(ProgramPass {
        stage: PassStage::Fragment,
        entry_point: "fs_probe".to_owned(),
        inputs,
        output: OutputSlot::Binding { index: output },
        uniform_offset: 0,
        uniform_length,
        repeat: None,
    });

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_capture_probe_adds_the_declared_binding_and_pass() {
        let initial = SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full };
        let inputs = vec![InputSlot::Binding { index: 0 }];
        let mut register = ProgramRegister {
            wgsl: "base".to_owned(),
            bindings: vec![initial],
            transients: Vec::new(),
            geometries: Vec::new(),
            depth_transients: Vec::new(),
            passes: Vec::new(),
        };

        let output = append_capture_probe(&mut register, "probe", inputs.clone(), 24);

        assert_eq!(output, 1);
        assert_eq!(register.wgsl, "base\nprobe");
        assert_eq!(register.bindings, vec![initial, initial]);
        assert_eq!(
            register.passes,
            vec![ProgramPass {
                stage: PassStage::Fragment,
                entry_point: "fs_probe".to_owned(),
                inputs,
                output: OutputSlot::Binding { index: output },
                uniform_offset: 0,
                uniform_length: 24,
                repeat: None,
            }]
        );
    }
}
