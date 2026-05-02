//! `aether-scenario` CLI — runs a YAML scenario script through the
//! in-process `TestBench` chassis. Designed for agents and CI: takes
//! one path argument, boots a fresh bench, walks the script, prints
//! a report, exits 0 on pass / 1 on fail. Component developers
//! typically reach for the `scenario_dir!` proc-macro instead so
//! they get IDE-friendly per-script `#[test]` integration.

use std::fs;
use std::process::ExitCode;

use aether_scenario::{RunReport, RunnerError, StepStatus, run_yaml_str};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: aether-scenario <path-to-yaml>");
        return ExitCode::from(2);
    };
    if args.next().is_some() {
        eprintln!("usage: aether-scenario <path-to-yaml> (extra args not supported)");
        return ExitCode::from(2);
    }

    let yaml = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("read {path}: {e}");
            return ExitCode::from(2);
        }
    };

    match run_yaml_str(&yaml) {
        Ok(report) => {
            print_report(&report);
            if report.passed {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(RunnerError::Parse(msg)) => {
            eprintln!("yaml parse failed in {path}: {msg}");
            ExitCode::from(2)
        }
        Err(RunnerError::Boot(msg)) => {
            eprintln!("test-bench boot failed: {msg}");
            ExitCode::from(2)
        }
        Err(other) => {
            eprintln!("runner error: {other}");
            ExitCode::from(2)
        }
    }
}

/// Pretty-print a `RunReport`. One line per step plus a final
/// pass/fail line so `grep` against CI logs identifies the script
/// and step that broke without re-parsing structured output.
fn print_report(report: &RunReport) {
    println!("scenario: {}", report.script_name);
    for step in &report.steps {
        match &step.status {
            StepStatus::Pass => println!("  [{:>3}] {} ok", step.index, step.op),
            StepStatus::Fail(reason) => {
                println!("  [{:>3}] {} FAIL: {}", step.index, step.op, reason)
            }
        }
    }
    if report.passed {
        println!("result: pass ({} steps)", report.steps.len());
    } else {
        println!("result: FAIL");
    }
}
