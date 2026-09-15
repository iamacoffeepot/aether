//! Public harness GPU-timing surface (iamacoffeepot/aether#4422).
//!
//! These tests stop before recording a frame, so they need no adapter and
//! assert protocol/measurement boundaries deterministically rather than
//! applying a wall-clock threshold to a GPU.

use std::thread::sleep;
use std::time::{Duration, Instant};

use aether_data::Kind;
use aether_harness_substrate::{DEFAULT_POLL_BUDGET, SubstrateHarness};
use aether_harness_substrate_capture::{ProgramTimingsResult, RenderHarnessBuilderExt, RenderHarnessExt};
use aether_kinds::CaptureFrame;
use aether_render::ProgramTimings;

/// How many times the harness observer has witnessed `kind`, read to a
/// wall-clock budget rather than the instant the reply lands.
///
/// The cap witnesses each dispatched kind by mail on the dispatched mail's own
/// chain, while `program_gpu_timings` waits for the reply's correlation id and
/// nothing else — the weakest of the harness's three waits, which orders none
/// of what the handler sent beside answering. No barrier the caller can raise
/// here is stronger, because every inbound mail the cap dispatches is itself
/// witnessed, so a settling send would move the count it is meant to order.
/// Reading to a budget is what `HarnessOp::poll_until` does with an effect the
/// caller's chain does not carry, and it leaves the assertion exact.
fn witnessed(harness: &SubstrateHarness, kind: &str) -> usize {
    let deadline = Instant::now() + DEFAULT_POLL_BUDGET;
    while harness.count_observed(kind) == 0 && Instant::now() < deadline {
        sleep(Duration::from_millis(10));
    }

    harness.count_observed(kind)
}

#[test]
fn gpu_timing_query_never_routes_through_capture() {
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot harness");

    let reply = harness.program_gpu_timings(0).expect("query timing surface");

    match reply {
        ProgramTimingsResult::Absent { reason } => {
            assert_eq!(reason, "per-pass gpu timings are disabled by configuration");
        }
        other => panic!("a disabled timing instrument must report why it is absent, got {other:?}"),
    }
    assert_eq!(witnessed(&harness, ProgramTimings::NAME), 1, "the helper must send exactly one timing query");
    assert_eq!(
        harness.count_observed(CaptureFrame::NAME),
        0,
        "a GPU timing query must not capture pixels or pay PNG deflate",
    );
}

#[test]
fn enabled_timing_surface_is_absent_until_a_frame_meets_the_device() {
    let mut harness =
        SubstrateHarness::builder().size(64, 48).with_render_pass_timings().build().expect("boot harness");

    let reply = harness.program_gpu_timings(0).expect("query timing surface");

    match reply {
        ProgramTimingsResult::Absent { reason } => {
            assert_eq!(reason, "no frame has recorded yet, so the timing instrument has not met the render device");
        }
        other => panic!("an instrument that has not met a device cannot invent measurements, got {other:?}"),
    }
    assert_eq!(harness.count_observed(CaptureFrame::NAME), 0, "enabling timing must not request a capture");
}
