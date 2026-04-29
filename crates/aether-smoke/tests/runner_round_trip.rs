//! End-to-end smoke runner test: parse YAML, boot a TestBench,
//! run the script, assert the report. Skipped on driverless runners
//! by probing for a wgpu adapter — same gating pattern as the
//! TestBench unit test (ADR-0067).

use aether_smoke::{Runner, parse_script};
use aether_substrate_test_bench::TestBench;

/// Probe for any usable wgpu adapter. Headless Linux runners without
/// `mesa-vulkan-drivers` skip the test rather than panic.
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

#[test]
fn empty_script_passes_with_no_steps() {
    if !has_wgpu_adapter() {
        eprintln!("skipping: no wgpu adapter available");
        return;
    }
    let script = parse_script("name: empty\nsteps: []\n").expect("parse");
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let report = Runner::run(&mut bench, &script);
    assert!(report.passed, "empty script should pass: {report:?}");
    assert_eq!(report.script_name, "empty");
    assert!(report.steps.is_empty());
}

/// The chassis clears to a non-black color, so advance, capture, and
/// `not_all_black` all pass against a freshly-booted bench with no
/// scene loaded. End-to-end happy path: parse YAML, run, observe pass.
#[test]
fn advance_capture_assert_round_trip() {
    if !has_wgpu_adapter() {
        eprintln!("skipping: no wgpu adapter available");
        return;
    }
    let script = parse_script(
        r#"
name: clear-color round trip
steps:
  - op: advance
    ticks: 1
  - op: capture
  - op: assert
    check:
      kind: not_all_black
"#,
    )
    .expect("parse");
    let mut bench = TestBench::start_with_size(32, 32).expect("boot");
    let report = Runner::run(&mut bench, &script);
    assert!(report.passed, "round trip should pass: {report:?}");
    assert_eq!(report.steps.len(), 3);
    assert!(report.steps.iter().all(|s| s.status.is_pass()));
}

#[test]
fn assert_before_capture_short_circuits() {
    if !has_wgpu_adapter() {
        eprintln!("skipping: no wgpu adapter available");
        return;
    }
    let script = parse_script(
        r#"
name: ordering
steps:
  - op: assert
    check:
      kind: not_all_black
  - op: advance
    ticks: 1
"#,
    )
    .expect("parse");
    let mut bench = TestBench::start_with_size(32, 32).expect("boot");
    let report = Runner::run(&mut bench, &script);
    assert!(!report.passed);
    // First step (assert) fails; subsequent steps are reported as
    // skipped rather than executed.
    assert!(matches!(
        report.steps[0].status,
        aether_smoke::StepStatus::Fail(_)
    ));
    let aether_smoke::StepStatus::Fail(reason) = &report.steps[1].status else {
        panic!("expected step 1 to be skipped");
    };
    assert!(reason.contains("skipped"), "step 1 reason: {reason}");
}
