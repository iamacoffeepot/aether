//! `perf-bloomery` (issue #7281): drive S Muse sessions through the shipped
//! bloomery chassis and driver against a loopback stub vendor, time every
//! driver step, and emit one `TrialReport` JSON on stdout with a
//! `bloomery.steps` section and a `bloomery.session` section. Diagnostics go
//! to stderr, so stdout stays pure JSON, and `aether-perf-compare` pairs two
//! builds of this bin unchanged.
//!
//! # What is measured
//!
//! The stub answers each turn from a stateless reply function, holding each
//! reply for the configured vendor delay, so with the default delay the
//! numbers are engine overhead alone. Each session makes T turns: every
//! turn but the last asks for N tool calls rotating over the configured
//! tools, and the last completes, so each session rests `Completed`.
//!
//! The window opens after the seed is appended, the chassis is booted, the
//! bundle is loaded, and the session reactor is warm; it closes when the last
//! session records its rest. Step samples from before the window are
//! discarded. Every step span the driver closes in the window is a sample:
//! `read_artifact.<purpose>`, `read_closure`, `load`, `invoke`, `append`,
//! `read_events`, `read_artifacts`, `warm`, `evaluate`, `status`, `fetch`,
//! `run_workspace`, and `tick`, each summarised as p50 / p90 / p99 / max / n
//! in nanoseconds. The session cell carries sessions per second over the
//! window, the journal root's growth per session, and the process's peak
//! resident set from `/proc/self/status` (absent off Linux).
//!
//! One bundle runs up to `AETHER_BLOOMERY_BUNDLE_INVOCATIONS` requests at once
//! (default 16), and the rest wait in FIFO order, so S sessions past that
//! limit queue rather than running in parallel. The session cell reports that
//! throughput as measured; the bench does not work around it.
//!
//! One (sessions × shape) cell per process: the peak resident set and the
//! step subscriber are process-wide, so a sweep is several invocations.
//!
//! # Knobs
//!
//! - `AETHER_PERF_BLOOMERY_SESSIONS` — concurrent sessions. Default `1`.
//! - `AETHER_PERF_BLOOMERY_TURNS` — turns per session, the last one
//!   completing it. Default `8`.
//! - `AETHER_PERF_BLOOMERY_CALLS` — tool calls per turn but the last, at most
//!   `ToolCalls::MAX_CALLS`. Default `4`.
//! - `AETHER_PERF_BLOOMERY_TOOLS` — a comma list of `write`, `list`, `read`,
//!   and `grep` the calls rotate over. Default `write`.
//! - `AETHER_PERF_BLOOMERY_TREE` — `<files>x<bytes>` generated files, 16 to a
//!   directory, or `dir:<path>` to stage a host directory without its `.git`
//!   and `target`. Default `64x4096`.
//! - `AETHER_PERF_BLOOMERY_VENDOR_DELAY_MILLIS` — how long the stub vendor
//!   holds each reply, in millis, serving replies concurrently. Default `0`.
//! - `AETHER_PERF_GIT_SHA` — stamped into the report; falls back to
//!   `git rev-parse HEAD`.
//!
//! A session's conversation at rest must fit `TurnItems::MAX_ITEMS`
//! (2 + (T − 1) × 2 × N items).
//!
//! # Exit codes
//!
//! - `0` — the report is on stdout.
//! - `2` — the muse wasm is not built (run `cargo xtask build-wasm`), or the
//!   run measured nothing.
//! - `3` — the report did not serialize.
//! - `4` — a knob is malformed or past a bound, or a session faulted, failed
//!   to open, or rested other than `Completed`.
//!
//! A harness wait that times out (a reply or head watch past thirty seconds,
//! or a boot that fails) panics, naming what it waited on.

#![forbid(unsafe_code)]

mod drive;
mod knobs;
mod replies;
mod report;
mod seed;
mod steps;

use std::fmt::Display;
use std::fs;
use std::io::{self, Write};
use std::process::ExitCode;
use std::thread;

use aether_harness_bloomery::{BloomeryHarness, StubVendor};
use aether_harness_substrate::test_helpers::locate_component_wasm;

use crate::knobs::Knobs;
use crate::replies::Script;
use crate::report::Measured;
use crate::seed::Seed;
use crate::steps::StepTimer;

/// Why a run produced no report: the exit code and what to say on stderr.
pub struct Failure {
    code: u8,
    message: String,
}

impl Failure {
    /// Exit 2: the run could not measure what it set out to.
    pub fn unmeasured(message: impl Display) -> Self {
        Self { code: 2, message: message.to_string() }
    }

    /// Exit 4: a knob is unusable or a session did not run the workload.
    pub fn session(message: impl Display) -> Self {
        Self { code: 4, message: message.to_string() }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure { code, message }) => {
            let _ = writeln!(io::stderr(), "perf-bloomery: {message}");
            ExitCode::from(code)
        }
    }
}

fn run() -> Result<(), Failure> {
    let knobs = Knobs::from_env().map_err(Failure::session)?;
    let wasm_path = locate_component_wasm("aether_bloomery_muse")
        .ok_or_else(|| Failure::unmeasured("no aether_bloomery_muse wasm: run `cargo xtask build-wasm` first"))?;
    let wasm = fs::read(&wasm_path).map_err(|error| Failure::unmeasured(format!("read the muse wasm: {error}")))?;
    let timer =
        StepTimer::install().map_err(|error| Failure::unmeasured(format!("install the step subscriber: {error}")))?;

    let (samples, measured) = thread::scope(|scope| {
        let seed = Seed::new(&knobs, &wasm).map_err(Failure::session)?;
        let script = Script::new(&knobs, seed.probe.clone());
        let vendor = StubVendor::start(scope, move |request| script.reply(request))
            .map_err(|error| Failure::unmeasured(format!("start the stub vendor: {error}")))?;
        let seeded = seed.open(&knobs, &vendor.endpoint()).map_err(Failure::session)?;
        let mut harness = BloomeryHarness::start_allowing([seeded.batch], ["127.0.0.1"]);

        drive::activate(&mut harness, seeded.set)?;
        timer.drain();
        let before = journal_size(&harness)?;
        let window = drive::sessions(&mut harness, &seeded.calls)?;
        let journal_bytes = journal_size(&harness)?.saturating_sub(before);
        let samples = timer.drain();
        drive::completed(&harness, &window.records)?;

        Ok::<_, Failure>((samples, Measured { elapsed: window.elapsed, rested: window.records.len(), journal_bytes }))
    })?;
    if samples.is_empty() {
        return Err(Failure::unmeasured("no step span closed in the window: the driver's step target changed?"));
    }

    let _ = writeln!(
        io::stderr(),
        "perf-bloomery: {} sessions ({}) rested in {:.3} s",
        measured.rested,
        knobs.shape(),
        measured.elapsed.as_secs_f64()
    );
    let json = serde_json::to_string(&report::trial(&knobs, samples, &measured))
        .map_err(|error| Failure { code: 3, message: format!("serialize the report: {error}") })?;
    writeln!(io::stdout(), "{json}").map_err(|error| Failure { code: 3, message: format!("write the report: {error}") })
}

/// The journal root's size on disk.
fn journal_size(harness: &BloomeryHarness) -> Result<u64, Failure> {
    report::journal_bytes(harness.journal_path())
        .map_err(|error| Failure::unmeasured(format!("size the journal root: {error}")))
}
