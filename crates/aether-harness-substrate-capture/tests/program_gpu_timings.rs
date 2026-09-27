//! Public harness GPU-timing surface (iamacoffeepot/aether#4422).
//!
//! These tests stop before recording a frame, so they need no adapter and
//! assert the reply each timing configuration gives deterministically rather
//! than applying a wall-clock threshold to a GPU.

use aether_harness_substrate::SubstrateHarness;
use aether_harness_substrate_capture::{ProgramTimingsResult, RenderHarnessBuilderExt, RenderHarnessExt};

#[test]
fn disabled_timing_surface_reports_why_it_is_absent() {
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot harness");

    let reply = harness.program_gpu_timings(0).expect("query timing surface");

    match reply {
        ProgramTimingsResult::Absent { reason } => {
            assert_eq!(reason, "per-pass gpu timings are disabled by configuration");
        }
        other => panic!("a disabled timing instrument must report why it is absent, got {other:?}"),
    }
}

#[test]
fn enabled_timing_surface_is_absent_until_a_frame_meets_the_device() {
    let mut harness =
        SubstrateHarness::builder().size(64, 48).with_render_pass_timings().build().expect("boot harness");

    let reply = harness.program_gpu_timings(0).expect("query timing surface");

    // "No frame has recorded yet" is itself the proof that no capture ran:
    // a capture records a frame, and the instrument would then have met it.
    match reply {
        ProgramTimingsResult::Absent { reason } => {
            assert_eq!(reason, "no frame has recorded yet, so the timing instrument has not met the render device");
        }
        other => panic!("an instrument that has not met a device cannot invent measurements, got {other:?}"),
    }
}
