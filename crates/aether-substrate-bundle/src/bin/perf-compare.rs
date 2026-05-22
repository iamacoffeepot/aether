//! `perf-compare` (iamacoffeepot/aether#1077): interleave K base /
//! candidate `perf-trial` runs on one runner and render the noise-aware
//! paired comparison (ADR-0085) as a sticky-comment markdown body
//! (stdout) plus an optional JSON report (`--out`).
//!
//! Base and candidate run on the *same* runner, interleaved trial by
//! trial, so shared run-to-run drift cancels in the per-trial paired
//! delta (ADR-0085 §3). Each side is invoked as a subprocess so every
//! trial is a fresh process (§1).
//!
//! ```text
//! # two checkouts (CI):
//! aether-perf-compare --base ./base/perf-trial --cand ./pr/perf-trial -k 12 --out report.json
//!
//! # env A/B on one binary (the #1076 stickiness self-test):
//! aether-perf-compare --base ./perf-trial --cand ./perf-trial \
//!     --base-env AETHER_LOCAL_STICKY_MAX=1 --cand-env AETHER_LOCAL_STICKY_MAX=8 -k 12
//! ```
//!
//! Informational, never gating: a comparison run exits 0 even with
//! regressions present; a non-zero exit means an *operational* failure
//! (a trial crashed, bad args). ADR-0085 §4.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::env;
use std::fs;
use std::process::{Command, ExitCode};

use aether_substrate_bundle::perf::report::{CompareConfig, TrialReport, compare, markdown};

struct Args {
    base: String,
    cand: String,
    base_env: Vec<(String, String)>,
    cand_env: Vec<(String, String)>,
    k: usize,
    out: Option<String>,
    title: String,
    subtitle: Option<String>,
}

fn split_kv(s: &str) -> Result<(String, String), String> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| format!("env must be KEY=VALUE: {s}"))?;
    Ok((k.to_owned(), v.to_owned()))
}

fn parse_args() -> Result<Args, String> {
    let mut base = None;
    let mut cand = None;
    let mut base_env = Vec::new();
    let mut cand_env = Vec::new();
    let mut k = 12usize;
    let mut out = None;
    let mut title = "candidate vs base".to_owned();
    let mut subtitle = None;

    let mut it = env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--base" => base = it.next(),
            "--cand" => cand = it.next(),
            "--base-env" => {
                if let Some(kv) = it.next() {
                    base_env.push(split_kv(&kv)?);
                }
            }
            "--cand-env" => {
                if let Some(kv) = it.next() {
                    cand_env.push(split_kv(&kv)?);
                }
            }
            "-k" | "--trials" => {
                let n = it.next().ok_or("-k needs a value")?;
                k = n.parse().map_err(|_| format!("bad -k: {n}"))?;
            }
            "--out" => out = it.next(),
            "--title" => {
                if let Some(t) = it.next() {
                    title = t;
                }
            }
            "--subtitle" => subtitle = it.next(),
            other => return Err(format!("unknown arg: {other}")),
        }
    }
    if k == 0 {
        return Err("-k must be >= 1".to_owned());
    }
    Ok(Args {
        base: base.ok_or("--base <path> required")?,
        cand: cand.ok_or("--cand <path> required")?,
        base_env,
        cand_env,
        k,
        out,
        title,
        subtitle,
    })
}

fn run_trial(path: &str, extra: &[(String, String)]) -> Result<TrialReport, String> {
    let out = Command::new(path)
        .envs(extra.iter().cloned())
        .output()
        .map_err(|e| format!("spawn {path}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{path} exited {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    serde_json::from_slice::<TrialReport>(&out.stdout)
        .map_err(|e| format!("parse trial json from {path}: {e}"))
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("perf-compare: {e}");
            return ExitCode::from(2);
        }
    };

    let mut base_reports = Vec::with_capacity(args.k);
    let mut cand_reports = Vec::with_capacity(args.k);
    for t in 0..args.k {
        eprintln!("perf-compare: trial {}/{}", t + 1, args.k);
        match run_trial(&args.base, &args.base_env) {
            Ok(r) => base_reports.push(r),
            Err(e) => {
                eprintln!("perf-compare: base trial {t} failed: {e}");
                return ExitCode::from(1);
            }
        }
        match run_trial(&args.cand, &args.cand_env) {
            Ok(r) => cand_reports.push(r),
            Err(e) => {
                eprintln!("perf-compare: candidate trial {t} failed: {e}");
                return ExitCode::from(1);
            }
        }
    }

    let report = compare(&base_reports, &cand_reports, CompareConfig::default());
    let subtitle = args
        .subtitle
        .unwrap_or_else(|| format!("{} trials/config, interleaved on one runner", report.trials));
    println!("{}", markdown(&report, &args.title, &subtitle));

    if let Some(path) = &args.out {
        match serde_json::to_string_pretty(&report) {
            Ok(j) => {
                if let Err(e) = fs::write(path, j) {
                    eprintln!("perf-compare: write {path}: {e}");
                    return ExitCode::from(1);
                }
            }
            Err(e) => {
                eprintln!("perf-compare: serialize report: {e}");
                return ExitCode::from(1);
            }
        }
    }

    eprintln!(
        "perf-compare: {} improved, {} stable, {} regressed (informational)",
        report.improved, report.stable, report.regressed
    );
    ExitCode::SUCCESS
}
