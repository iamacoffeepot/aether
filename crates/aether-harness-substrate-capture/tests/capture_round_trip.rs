//! Capture-path round trips over a render-composed harness — relocated
//! from the core harness's unit tests by the crate split (issue #3765):
//! the core no longer links wgpu, so the tests that capture live with
//! the GPU crate.

// Test-only skip diagnostics emit `eprintln!` so `cargo test` runners
// surface a visible "skipping: ..." line alongside `test ... ok`
// (issue 891).
#![allow(clippy::print_stderr)]

use aether_actor::Addressable;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_kinds::{CaptureFrame, CaptureFrameResult};
use aether_render::RenderCapability;

/// Boot, advance one tick, capture, sanity-check the PNG. The default
/// scene is empty so the captured frame is the background-clear color
/// uniformly. The test asserts the PNG is well-formed; deeper visual
/// assertions live in the scenario suites.
///
/// The test lets the boot fail naturally on driverless runners and
/// skips on any boot error rather than pulling in the wgpu probe —
/// same skip semantics, keyed off the boot result.
#[test]
fn boot_advance_capture_round_trip() {
    let mut tb = match SubstrateHarness::builder().size(64, 48).with_render().build() {
        Ok(tb) => tb,
        Err(e) => {
            eprintln!("skipping: SubstrateHarness boot failed (likely no wgpu adapter): {e}");
            return;
        }
    };
    let result =
        tb.execute(vec![("tick", HarnessOp::advance(1)), ("snap", HarnessOp::capture())]).expect("advance + capture");
    let png = result.captured("snap").expect("snap step ran");
    assert!(
        png.starts_with(&[0x89, 0x50, 0x4E, 0x47]),
        "captured bytes are not a PNG: first 8 bytes={:?}",
        &png.iter().take(8).copied().collect::<Vec<u8>>(),
    );
}

/// iamacoffeepot/aether#1273: `on_capture_frame` parks the request on
/// the capture queue and returns immediately — the reply happens later
/// on the chassis main thread. ADR-0086 §12 says deferred replies MUST
/// hold-open against the trace root; without that hold `Settled{root}`
/// fires before the reply lands and the wire `Call` driving the MCP
/// tool ends with zero collected reply events.
///
/// Sends `CaptureFrame` via `HarnessOp::send_and_await` (the shape the
/// issue's regression test calls for) and asserts the reply decodes to
/// `CaptureFrameResult::Ok { png: <non-empty> }`. The PNG comes back
/// through the loopback's `EgressEvent::ToSession` — the same
/// correlation-id round-trip the MCP harness uses, but in-process.
#[test]
fn capture_frame_send_and_await_returns_png() {
    let mut tb = match SubstrateHarness::builder().size(64, 48).with_render().build() {
        Ok(tb) => tb,
        Err(e) => {
            eprintln!("skipping: SubstrateHarness boot failed (likely no wgpu adapter): {e}");
            return;
        }
    };
    let result = tb
        .execute(vec![
            ("tick", HarnessOp::advance(1)),
            (
                "capture",
                HarnessOp::send_and_await(
                    RenderCapability::NAMESPACE,
                    &CaptureFrame {
                        window: None,
                        mails: Vec::new(),
                        after_mails: Vec::new(),
                        checks: Vec::new(),
                        similarity: None,
                    },
                ),
            ),
        ])
        .expect("advance + send_and_await(CaptureFrame)");
    let reply: CaptureFrameResult = result.reply("capture").expect("capture step replied with CaptureFrameResult");
    match reply {
        CaptureFrameResult::Ok { png, verdict, .. } => {
            assert!(verdict.is_none(), "no checks were requested, so the verdict must be absent");
            assert!(
                png.starts_with(&[0x89, 0x50, 0x4E, 0x47]),
                "captured bytes are not a PNG: first 8 bytes={:?}",
                &png.iter().take(8).copied().collect::<Vec<u8>>(),
            );
        }
        CaptureFrameResult::Err { error } => {
            panic!("capture_frame replied Err: {error}");
        }
    }
}
