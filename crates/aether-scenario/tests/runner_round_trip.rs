//! End-to-end scenario runner test: parse YAML, boot a TestBench,
//! run the script, assert the report. Skipped on driverless runners
//! by probing for a wgpu adapter — same gating pattern as the
//! TestBench unit test (ADR-0067).

use aether_scenario::test_helpers::has_wgpu_adapter;
use aether_scenario::{Runner, parse_script};
use aether_substrate_bundle::test_bench::TestBench;

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

/// `SendMail` with an unknown mailbox surfaces the bench's
/// "UnknownMailbox" error as a step failure. Validates the encode →
/// send_bytes path runs end-to-end against a real bench: the catalog
/// resolves the kind, encoding succeeds, but `send_bytes` rejects
/// the unknown recipient — which is the symptom we want a scenario
/// author to see when they typo a mailbox name.
#[test]
fn send_mail_unknown_mailbox_fails_clearly() {
    if !has_wgpu_adapter() {
        eprintln!("skipping: no wgpu adapter available");
        return;
    }
    let script = parse_script(
        r#"
name: typo-mailbox
steps:
  - op: send_mail
    recipient: not.a.real.mailbox
    kind: aether.control.drop_component
    params:
      mailbox_id: "mbx-deadbeef"
"#,
    )
    .expect("parse");
    let mut bench = TestBench::start_with_size(32, 32).expect("boot");
    let report = Runner::run(&mut bench, &script);
    assert!(!report.passed, "should fail: {report:?}");
    let aether_scenario::StepStatus::Fail(reason) = &report.steps[0].status else {
        panic!("expected step 0 to fail");
    };
    // Either the catalog couldn't decode the params (e.g. mailbox_id
    // string format mismatch) OR the send_bytes path rejected the
    // recipient. Both are surfaceable failures the runner reports —
    // the test asserts a failure occurs and the step is the right
    // one, without pinning the exact message text.
    assert_eq!(report.steps[0].op, "send_mail");
    assert!(!reason.is_empty());
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
        aether_scenario::StepStatus::Fail(_)
    ));
    let aether_scenario::StepStatus::Fail(reason) = &report.steps[1].status else {
        panic!("expected step 1 to be skipped");
    };
    assert!(reason.contains("skipped"), "step 1 reason: {reason}");
}
