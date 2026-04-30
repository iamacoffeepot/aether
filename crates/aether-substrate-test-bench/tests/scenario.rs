//! Phase 3 substrate-feature scenarios (issue 430). Each test boots
//! a `TestBench`, loads `aether-test-fixture-probe`'s wasm, and
//! exercises one substrate primitive (input subscription, drop, etc.)
//! by counting fixture-emitted broadcasts on the bench loopback.
//!
//! Skipped when:
//! - No wgpu adapter is available (driverless Linux runners without
//!   `mesa-vulkan-drivers`).
//! - The fixture's wasm hasn't been built — tests read
//!   `target/wasm32-unknown-unknown/{debug,release}/aether_test_fixture_probe.wasm`
//!   and skip with an `eprintln!` when it's absent. CI builds the
//!   fixture wasm before invoking `cargo test`. Setting
//!   `AETHER_REQUIRE_RUNTIME=1` (CI does) flips both skip points
//!   into hard panics so a missing pre-build is loud.

use std::path::{Path, PathBuf};

use aether_kinds::{DropComponent, LoadComponent};
use aether_mail::{MailboxId, mailbox_id_from_name};
use aether_substrate_test_bench::TestBench;

// Pin the fixture rlib so its `inventory::submit!` `KindDescriptor`
// entries are present in this test binary. Without the reference, the
// host-target rlib's descriptor symbols can be stripped by the linker
// and `aether_kinds::descriptors::all()` won't see fixture kinds.
use aether_test_fixture_probe as _;

/// Probe for any usable wgpu adapter.
fn has_wgpu_adapter() -> bool {
    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::default(),
        compatible_surface: None,
        force_fallback_adapter: false,
    }))
    .is_ok()
}

/// Locate the fixture's wasm artifact under the workspace target dir.
/// Tries `release` first, then `debug` so either build profile works.
fn locate_fixture_wasm() -> Option<PathBuf> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root reachable from CARGO_MANIFEST_DIR");
    for profile in ["release", "debug"] {
        let path = workspace
            .join("target")
            .join("wasm32-unknown-unknown")
            .join(profile)
            .join("aether_test_fixture_probe.wasm");
        if path.exists() {
            return Some(path);
        }
    }
    None
}

/// Common boot path: probe wgpu, locate the fixture wasm, return
/// both. `AETHER_REQUIRE_RUNTIME=1` turns either missing requirement
/// into a panic so CI failures are loud.
fn require_runtime() -> Option<PathBuf> {
    let strict = std::env::var("AETHER_REQUIRE_RUNTIME").is_ok();
    if !has_wgpu_adapter() {
        assert!(
            !strict,
            "AETHER_REQUIRE_RUNTIME set but no wgpu adapter available",
        );
        eprintln!("skipping: no wgpu adapter available");
        return None;
    }
    match locate_fixture_wasm() {
        Some(path) => Some(path),
        None => {
            assert!(
                !strict,
                "AETHER_REQUIRE_RUNTIME set but aether_test_fixture_probe.wasm not pre-built; \
                 CI's `Pre-build component wasm for scenario tests` step is missing this crate",
            );
            eprintln!(
                "skipping: aether_test_fixture_probe.wasm not built; run \
                 `cargo build --target wasm32-unknown-unknown -p aether-test-fixture-probe`",
            );
            None
        }
    }
}

const PROBE_NAME: &str = "probe";
const TICK_OBSERVED: &str = "aether.test_fixture.tick_observed";

/// Loads the probe into the bench. The load mail is queued and
/// processed during the next `advance` (the bench's queue is FIFO
/// and drains ahead of the chassis Advance event, so the freshly
/// instantiated probe is fully subscribed before any tick fans out).
fn load_probe(bench: &TestBench, wasm_path: &Path) {
    let wasm = std::fs::read(wasm_path).expect("read fixture wasm");
    bench
        .send_mail(
            "aether.control",
            &LoadComponent {
                wasm,
                name: Some(PROBE_NAME.to_owned()),
            },
        )
        .expect("dispatch load_component");
}

/// Subscribing the fixture to Tick yields exactly one
/// `tick_observed` broadcast per advance tick. Validates the
/// subscribe_input → tick fanout path end-to-end.
#[test]
fn input_subscription_yields_one_tick_observed_per_advance() {
    let Some(wasm_path) = require_runtime() else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    load_probe(&bench, &wasm_path);

    bench.advance(5).expect("advance 5");
    assert_eq!(
        bench.count_observed(TICK_OBSERVED),
        5,
        "expected exactly 5 tick_observed broadcasts after advance(5); \
         observed kinds: {:?}",
        bench.observed_kinds(),
    );
}

/// Dropping the probe stops further tick_observed broadcasts.
/// Validates that `aether.control.drop_component` removes the
/// mailbox from the input subscriber set so subsequent ticks don't
/// reach it (ADR-0021 + ADR-0038 actor lifecycle).
#[test]
fn drop_component_silences_tick_echoes() {
    let Some(wasm_path) = require_runtime() else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    load_probe(&bench, &wasm_path);

    bench.advance(3).expect("pre-drop advance");
    assert_eq!(
        bench.count_observed(TICK_OBSERVED),
        3,
        "expected 3 tick_observed before drop; observed kinds: {:?}",
        bench.observed_kinds(),
    );

    // Queue the drop. The same FIFO ordering that lets the load mail
    // beat the first tick fanout means the drop mail beats the next
    // tick fanout — by the time `Advance{1}`'s `run_frame` queries
    // subscribers, the probe's mailbox is already gone.
    let probe_mbox = MailboxId(mailbox_id_from_name(PROBE_NAME));
    bench
        .send_mail(
            "aether.control",
            &DropComponent {
                mailbox_id: probe_mbox,
            },
        )
        .expect("dispatch drop_component");
    bench.advance(1).expect("drop drain advance");

    let post_drop = bench.count_observed(TICK_OBSERVED);

    bench.advance(10).expect("post-drop advance");
    assert_eq!(
        bench.count_observed(TICK_OBSERVED),
        post_drop,
        "tick_observed count climbed after drop_component; observed kinds: {:?}",
        bench.observed_kinds(),
    );
}

/// `aether.observation.frame_stats` is broadcast every 120 frames
/// (ADR-0023). Advancing exactly 120 ticks should yield one such
/// broadcast on the loopback. The bench emits this from its own
/// frame loop — no fixture component needed.
#[test]
fn frame_stats_broadcast_at_120_tick_cadence() {
    if !has_wgpu_adapter() {
        let strict = std::env::var("AETHER_REQUIRE_RUNTIME").is_ok();
        assert!(
            !strict,
            "AETHER_REQUIRE_RUNTIME set but no wgpu adapter available",
        );
        eprintln!("skipping: no wgpu adapter available");
        return;
    }
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    bench.advance(120).expect("advance 120");
    let stats_count = bench.count_observed("aether.observation.frame_stats");
    assert_eq!(
        stats_count,
        1,
        "expected exactly one frame_stats broadcast at 120 ticks; observed kinds: {:?}",
        bench.observed_kinds(),
    );
}
