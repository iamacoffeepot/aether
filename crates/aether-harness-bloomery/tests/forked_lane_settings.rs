#![cfg(unix)]

//! Forked coordinators must honour the builder's landing gate and poll cadence
//! (#5599 / F0048). In-process boots already consumed both knobs; the child
//! used to pin poll to 1s and drop `cas_land` entirely.

use std::env;
use std::path::Path;

use aether_bloomery::BloomStatus;
use aether_chassis_bloomery::bloomery::mock_lane::LaneScript;
use aether_harness_bloomery::HarnessBuilder;

/// A cadence far enough out that no reactor timer fires inside this scenario.
/// There is no "never" (`poll_interval_secs.max(1)`), so a day stands in.
const QUIET_POLL_SECS: u64 = 86_400;

#[test]
fn a_forked_cas_land_false_resolves_and_never_lands() {
    ensure_bloomery_bin();

    let mut harness = HarnessBuilder::lane(&LaneScript::all_passing())
        .cas_land(false)
        .poll_interval_secs(QUIET_POLL_SECS)
        .start("forked-cas-land-off");

    let (bloom, sealed_on) = {
        let view = harness.view();
        let bloom = view.blooms.first().expect("auto-seal produced a bloom").id;
        (bloom, view.mainline)
    };

    harness.pump_until("the bloom resolves with landing gated off", |harness| {
        harness.bloom(bloom).status == BloomStatus::Resolved
    });

    assert_eq!(harness.bloom(bloom).status, BloomStatus::Resolved);
    assert_eq!(harness.view().mainline, sealed_on, "resolve must not move mainline");

    for _ in 0..8 {
        harness.land_tick();
        assert_eq!(harness.bloom(bloom).status, BloomStatus::Resolved, "cas_land(false) must refuse every land wake",);
        assert_eq!(harness.view().mainline, sealed_on, "a gated land must not move mainline");
    }
}

/// This package does not own the `bloomery` bin, so cargo does not inject
/// `CARGO_BIN_EXE_bloomery` here. The forked cell still execs that production
/// binary; when a workspace build already produced it next to this crate's
/// mock-lane, point the spawn at that sibling.
fn ensure_bloomery_bin() {
    if env::vars().any(|(key, _)| key == "CARGO_BIN_EXE_bloomery") {
        return;
    }
    let bloomery = Path::new(&aether_harness_bloomery::mock_lane_program()).with_file_name("bloomery");
    assert!(
        bloomery.is_file(),
        "the forked cell execs the production bloomery binary; expected it at {}",
        bloomery.display(),
    );
    #[allow(
        clippy::disallowed_methods,
        reason = "test process injects CARGO_BIN_EXE_bloomery for a sibling workspace binary this package does not own"
    )]
    unsafe {
        env::set_var("CARGO_BIN_EXE_bloomery", bloomery);
    }
}
