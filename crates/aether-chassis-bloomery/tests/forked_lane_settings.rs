#![cfg(all(unix, feature = "github"))]

//! Forked coordinators must honour the builder's landing gate and poll cadence
//! (#5599 / F0048). In-process boots already consumed both knobs; the child
//! used to pin poll to 1s and drop `cas_land` entirely.

use std::env;
use std::fs;
use std::process::{Command, Stdio};
use std::thread;

use aether_bloomery::BloomStatus;
use aether_chassis_bloomery::bloomery::BloomeryEnv;
use aether_chassis_bloomery::bloomery::mock_lane::LaneScript;
use aether_harness_bloomery::HarnessBuilder;
use aether_harness_bloomery::harness::ForkedLaneSettings;

/// A cadence far enough out that no reactor timer fires inside this scenario.
/// There is no "never" (`poll_interval_secs.max(1)`), so a day stands in.
const QUIET_POLL_SECS: u64 = 86_400;

/// Handshake env that tells this test binary it is the isolated resolver child,
/// not the parent scenario. Named outside `AETHER_*` so `BloomeryEnv::from_env`
/// does not treat it as an unknown chassis knob.
const RESOLVE_REPORT: &str = "BLOOMERY_TEST_FORKED_LANE_RESOLVE_REPORT";

#[test]
fn a_forked_cas_land_false_resolves_and_never_lands() {
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
        assert_eq!(harness.bloom(bloom).status, BloomStatus::Resolved, "cas_land(false) must refuse every land wake");
        assert_eq!(harness.view().mainline, sealed_on, "a gated land must not move mainline");
    }
}

#[test]
fn forked_lane_settings_reach_the_production_resolver() {
    if write_resolve_report_if_child() {
        return;
    }

    let report = tempfile::NamedTempFile::new().expect("a resolve-report file");
    let settings = ForkedLaneSettings {
        store_path: "/tmp/forked-lane-store",
        artifacts_root: "/tmp/forked-lane-artifacts",
        lane_program: "/tmp/mock-lane",
        worktree_base: "/tmp/forked-lane-worktrees",
        poll_interval_secs: QUIET_POLL_SECS,
        cas_land_enabled: false,
        fixture_base_sha: "abc123",
        heartbeat_silence_secs: None,
        authorized_instructions: "",
        retrospect_reader_enabled: false,
        host_class: "harness-fleet",
    };
    let exe = env::current_exe().expect("the test executable");
    let test_thread = thread::current();
    let test_name = test_thread.name().expect("libtest names the test thread");
    let mut command = Command::new(exe);
    command.arg(test_name).arg("--exact").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    isolate_resolve_child(&mut command);
    for (key, value) in settings.env() {
        command.env(key, value);
    }
    let output = command.env(RESOLVE_REPORT, report.path()).output().expect("the isolated resolver child forks");
    assert!(output.status.success(), "resolver child failed: {}", String::from_utf8_lossy(&output.stderr));

    let body = fs::read_to_string(report.path()).expect("the child wrote a resolve report");
    assert_eq!(
        body,
        format!("poll={QUIET_POLL_SECS}\ncas_land=false\nhost_class=harness-fleet\n"),
        "BloomeryEnv::from_env must consume the helper env, not the derive defaults",
    );
}

/// When the parent re-execs this test with [`RESOLVE_REPORT`], resolve the
/// helper-produced env through the production consumer and write the typed
/// knobs. Returns `true` when this process was the child.
fn write_resolve_report_if_child() -> bool {
    let Some(path) = env::vars().find(|(key, _)| key == RESOLVE_REPORT).map(|(_, value)| value) else {
        return false;
    };
    let resolved = BloomeryEnv::from_env().expect("helper env resolves through BloomeryEnv::from_env");
    fs::write(
        &path,
        format!(
            "poll={}\ncas_land={}\nhost_class={}\n",
            resolved.coordinator.poll_interval_secs, resolved.github.cas_land_enabled, resolved.coordinator.host_class,
        ),
    )
    .expect("the resolve report writes");
    true
}

/// Constructed child environment: platform surface only, then the helper's
/// addressed knobs. No parent `AETHER_*` rides across, so the child's
/// `from_env` cannot see this process's ambient config.
fn isolate_resolve_child(command: &mut Command) {
    command.env_clear();
    for (key, value) in env::vars() {
        if matches!(
            key.as_str(),
            "PATH" | "HOME" | "TMPDIR" | "USER" | "LOGNAME" | "SHELL" | "LANG" | "TZ" | "RUST_BACKTRACE"
        ) || key.starts_with("LC_")
            || key.starts_with("DYLD_")
            || key.starts_with("XDG_")
        {
            command.env(key, value);
        }
    }
}
