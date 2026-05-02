//! End-to-end CLI test: invoke the `aether-scenario` binary against
//! a fixture YAML, assert exit code + stdout. The binary boots a real
//! TestBench, so this needs a wgpu adapter — same gating as the
//! library's integration tests.

use std::process::Command;

use aether_scenario::test_helpers::has_wgpu_adapter;

#[test]
fn cli_runs_passing_scenario() {
    if !has_wgpu_adapter() {
        eprintln!("skipping: no wgpu adapter available");
        return;
    }
    let bin = env!("CARGO_BIN_EXE_aether-scenario");
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/clear_color.yml"
    );
    let output = Command::new(bin).arg(fixture).output().expect("run cli");
    assert!(
        output.status.success(),
        "cli exited with {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("result: pass"),
        "missing pass marker in stdout:\n{stdout}",
    );
}

#[test]
fn cli_missing_path_arg_exits_nonzero() {
    let bin = env!("CARGO_BIN_EXE_aether-scenario");
    let output = Command::new(bin).output().expect("run cli");
    assert!(
        !output.status.success(),
        "cli should fail without args, got {:?}",
        output.status
    );
}

#[test]
fn cli_missing_file_exits_nonzero() {
    let bin = env!("CARGO_BIN_EXE_aether-scenario");
    let output = Command::new(bin)
        .arg("/this/path/does/not/exist.yml")
        .output()
        .expect("run cli");
    assert!(!output.status.success());
}
