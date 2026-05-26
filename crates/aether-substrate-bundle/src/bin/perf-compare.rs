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
//!
//! # Two-level versioning (iamacoffeepot/aether#1206)
//!
//! The comparator no longer punts the whole report when a metric set
//! changes. A trial is versioned at two levels:
//!
//! - The *envelope* `schema` tag ([`TRIAL_SCHEMA`]) guards only the
//!   container shape — "a report is a list of named, versioned
//!   sections". A trial on the wrong envelope tag genuinely can't be
//!   sectioned, so it stays a whole-container `TrialErr::Schema` skip.
//! - Each section carries its own `version`. Adding or changing a metric
//!   bumps that section's version; [`compare`] dispatches per section and
//!   renders a changed/new section as "new this run" *without* blinding
//!   the sections that still pair.
//!
//! A back-compat shim lifts a pre-sections `v3` envelope (a flat
//! top-level `cells` array) into a synthetic `latency` section, so a `v3`
//! base on `main` still compares its latency spans against a `v4`
//! candidate right after this merges. Older `v1` / `v2` envelopes carried
//! the retired `hop` / `send_enqueue` / `residence` metric names and
//! can't be mapped, so they remain an envelope `TrialErr::Schema` skip.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::env;
use std::fs;
use std::process::{Command, ExitCode};

use aether_substrate_bundle::perf::report::{
    CompareConfig, ComparisonReport, LatencySection, RawSection, STICKY_MARKER, SectionReport,
    TRIAL_SCHEMA, TrialReport, compare, markdown, probe_schema,
};

/// The envelope tag of the last pre-sections report shape: a flat
/// top-level `cells` array of the *current* `CellJson` shape. A `v3` base
/// is lifted into a synthetic `latency` section so it still compares
/// against a `v4` candidate (the back-compat shim).
const LEGACY_CELLS_SCHEMA: &str = "aether.perf.trial.v3";

/// A trial subprocess run that didn't yield a comparable [`TrialReport`].
enum TrialErr {
    /// An operational failure — the trial crashed, bad args, unparseable
    /// output. Non-zero exit (this is what gates the *operational*
    /// health of the run, ADR-0085 §4).
    Op(String),
    /// The trial's *envelope* schema tag isn't one this comparator can
    /// section (iamacoffeepot/aether#1206) and isn't the `v3` shape the
    /// back-compat shim can lift. A whole-container comparison is
    /// meaningless, so it is an informational skip, not a crash. Carries
    /// the trial's tag.
    Schema(String),
}

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

fn run_trial(path: &str, extra: &[(String, String)]) -> Result<TrialReport, TrialErr> {
    let out = Command::new(path)
        .envs(extra.iter().cloned())
        .output()
        .map_err(|e| TrialErr::Op(format!("spawn {path}: {e}")))?;
    if !out.status.success() {
        return Err(TrialErr::Op(format!(
            "{path} exited {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    ingest_trial(&out.stdout).map_err(|e| match e {
        TrialErr::Op(msg) => TrialErr::Op(format!("parse trial json from {path}: {msg}")),
        other @ TrialErr::Schema(_) => other,
    })
}

/// Decode one trial's stdout into a [`TrialReport`], probing the envelope
/// tag first so an unreadable / unsectionable envelope is surfaced as its
/// own [`TrialErr::Schema`] skip rather than an operational crash.
///
/// - `v4` (the current envelope) decodes straight through; per-section
///   evolution is [`compare`]'s job, not a whole-report gate.
/// - `v3` (the last pre-sections envelope) carried a flat top-level
///   `cells` array of the *current* `CellJson` shape. The back-compat
///   shim lifts those cells into a synthetic `latency` section so a `v3`
///   base on `main` still compares against a `v4` candidate.
/// - any other tag (`v1` / `v2`, with retired metric names) can't be
///   sectioned and stays a [`TrialErr::Schema`] skip.
fn ingest_trial(stdout: &[u8]) -> Result<TrialReport, TrialErr> {
    // iamacoffeepot/aether#1151: read the envelope tag before the full
    // parse — an older trial carries retired `Metric` variant names, and
    // `from_slice::<TrialReport>` would hard-fail on serde's
    // unknown-variant error.
    match probe_schema(stdout) {
        Some(tag) if tag == TRIAL_SCHEMA => {
            serde_json::from_slice::<TrialReport>(stdout).map_err(|e| TrialErr::Op(e.to_string()))
        }
        Some(tag) if tag == LEGACY_CELLS_SCHEMA => lift_legacy_cells(stdout),
        Some(tag) => Err(TrialErr::Schema(tag)),
        None => Err(TrialErr::Op(
            "missing or non-string `schema` field".to_owned(),
        )),
    }
}

/// Back-compat shim: lift a `v3`-envelope `{schema, ..., cells:[...]}`
/// into a `v4` [`TrialReport`] with one synthetic `latency` section. The
/// `v3` cells use the same `CellJson` shape as today, so they decode into
/// a [`LatencySection`] unchanged. Self-obsoletes once every base on
/// `main` emits the `v4` envelope.
fn lift_legacy_cells(stdout: &[u8]) -> Result<TrialReport, TrialErr> {
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct LegacyTrial {
        git_sha: Option<String>,
        pace_hz: Option<u64>,
        frames: u32,
        #[serde(flatten)]
        latency: LatencySection,
    }

    let legacy: LegacyTrial =
        serde_json::from_slice(stdout).map_err(|e| TrialErr::Op(format!("lift v3 cells: {e}")))?;
    let body = serde_json::to_value(LatencySection {
        cells: legacy.latency.cells,
    })
    .map_err(|e| TrialErr::Op(format!("lift v3 cells: {e}")))?;
    Ok(TrialReport {
        schema: TRIAL_SCHEMA.to_owned(),
        git_sha: legacy.git_sha,
        pace_hz: legacy.pace_hz,
        frames: legacy.frames,
        sections: vec![RawSection {
            name: LatencySection::NAME.to_owned(),
            version: LatencySection::VERSION.to_owned(),
            body,
        }],
    })
}

/// A trial's *envelope* schema tag can't be sectioned and isn't the `v3`
/// shape the shim can lift (iamacoffeepot/aether#1206) — the container
/// shape changed, so a paired comparison isn't meaningful this run. Emit
/// an informational sticky note and exit 0 (this job never gates,
/// ADR-0085 §4); the next comparison resumes once the new envelope is on
/// both sides. Per-section metric evolution no longer reaches here — that
/// renders as a "new this run" section inside the report.
fn schema_skip(side: &str, got: &str) -> ExitCode {
    println!(
        "{STICKY_MARKER}\n## dispatch perf\n\n_The {side} trial uses envelope schema `{got}`, but this comparator expects `{TRIAL_SCHEMA}` — the report container shape changed, so a paired comparison isn't meaningful this run. The next comparison resumes once the new envelope is on both sides._\n"
    );
    eprintln!(
        "perf-compare: {side} envelope schema {got} != expected {TRIAL_SCHEMA} — container transition, comparison skipped (informational)"
    );
    ExitCode::SUCCESS
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
            Err(TrialErr::Schema(tag)) => return schema_skip("base", &tag),
            Err(TrialErr::Op(e)) => {
                eprintln!("perf-compare: base trial {t} failed: {e}");
                return ExitCode::from(1);
            }
        }
        match run_trial(&args.cand, &args.cand_env) {
            Ok(r) => cand_reports.push(r),
            Err(TrialErr::Schema(tag)) => return schema_skip("candidate", &tag),
            Err(TrialErr::Op(e)) => {
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

    let (improved, stable, regressed) = roll_up(&report);
    eprintln!(
        "perf-compare: {improved} improved, {stable} stable, {regressed} regressed (informational)"
    );
    ExitCode::SUCCESS
}

/// Sum improved / stable / regressed across the compared sections of a
/// report — latency and throughput alike (iamacoffeepot/aether#1202).
/// Uncompared sections contribute nothing.
fn roll_up(report: &ComparisonReport) -> (usize, usize, usize) {
    report
        .sections
        .iter()
        .fold((0, 0, 0), |(i, s, r), sec| match sec {
            SectionReport::Compared {
                improved,
                stable,
                regressed,
                ..
            }
            | SectionReport::ThroughputCompared {
                improved,
                stable,
                regressed,
                ..
            } => (i + improved, s + stable, r + regressed),
            SectionReport::Uncompared { .. } => (i, s, r),
        })
}

#[cfg(test)]
mod tests {
    use super::{ComparisonReport, ingest_trial};
    use aether_substrate_bundle::perf::report::{
        CellJson, CompareConfig, LatencySection, Metric, RawSection, SectionReport, TRIAL_SCHEMA,
        TrialReport, Verdict, compare,
    };

    /// Build a `v4` candidate side with a single `latency` cell whose p50
    /// follows `p50s` (one trial each).
    fn v4_side(p50s: &[u64]) -> Vec<TrialReport> {
        p50s.iter()
            .map(|&p50| {
                let cells = vec![CellJson {
                    workers: 11,
                    topo: "fanout-8".to_owned(),
                    metric: Metric::Drain,
                    p50,
                    p90: p50,
                    p99: p50,
                    max: p50,
                    n: 1800,
                }];
                let body =
                    serde_json::to_value(LatencySection { cells }).expect("encode latency body");
                TrialReport {
                    schema: TRIAL_SCHEMA.to_owned(),
                    git_sha: None,
                    pace_hz: None,
                    frames: 200,
                    sections: vec![RawSection {
                        name: LatencySection::NAME.to_owned(),
                        version: LatencySection::VERSION.to_owned(),
                        body,
                    }],
                }
            })
            .collect()
    }

    /// A `v3`-envelope trial's JSON for one cell — the flat pre-sections
    /// shape with a top-level `cells` array of the current `CellJson`.
    fn v3_json(p50: u64) -> String {
        format!(
            r#"{{"schema":"aether.perf.trial.v3","git_sha":null,"pace_hz":null,"frames":200,"cells":[{{"workers":11,"topo":"fanout-8","metric":"drain","p50":{p50},"p90":{p50},"p99":{p50},"max":{p50},"n":1800}}]}}"#
        )
    }

    fn p50_verdict(rep: &ComparisonReport) -> Verdict {
        rep.sections
            .iter()
            .find_map(|s| match s {
                SectionReport::Compared { name, cells, .. } if name == LatencySection::NAME => {
                    cells
                        .iter()
                        .find(|c| c.percentile == "p50")
                        .map(|c| c.verdict)
                }
                _ => None,
            })
            .expect("compared latency p50 cell")
    }

    #[test]
    fn v3_envelope_lifts_into_latency_section() {
        // The back-compat shim turns a flat v3 `{schema, cells:[...]}`
        // into a v4 report with one synthetic `latency` section.
        let lifted = ingest_trial(v3_json(167_000).as_bytes())
            .unwrap_or_else(|_| panic!("v3 envelope should lift into a v4 latency section"));
        assert_eq!(lifted.schema, TRIAL_SCHEMA);
        assert_eq!(lifted.sections.len(), 1);
        assert_eq!(lifted.sections[0].name, LatencySection::NAME);
        assert_eq!(lifted.sections[0].version, LatencySection::VERSION);
        let latency: LatencySection =
            serde_json::from_value(lifted.sections[0].body.clone()).expect("decode lifted body");
        assert_eq!(latency.cells.len(), 1);
        assert_eq!(latency.cells[0].p50, 167_000);
    }

    #[test]
    fn v3_base_compares_against_v4_candidate() {
        // A v3-envelope base (still on main right after this merges) is
        // lifted and compared against a v4 candidate — the latency win
        // still reads instead of blinding.
        let base: Vec<TrialReport> = [167_000u64, 165_000, 169_000, 166_000]
            .iter()
            .map(|&p50| {
                ingest_trial(v3_json(p50).as_bytes())
                    .unwrap_or_else(|_| panic!("lift v3 base p50={p50}"))
            })
            .collect();
        let cand = v4_side(&[33_000, 34_000, 32_000, 33_500]);
        let rep = compare(&base, &cand, CompareConfig::default());
        assert_eq!(p50_verdict(&rep), Verdict::Improved);
    }
}
