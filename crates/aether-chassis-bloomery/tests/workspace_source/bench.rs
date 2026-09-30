//! On-demand timing of the storage-source workspace on the shipped composition: an `Import` of an export of about
//! 1 GiB (20,000 files of 24 KiB in 200 directories plus one 512 MiB file) through the stub daemon, then a one-step
//! `Run` whose `/work` is the imported tree. It is the measurement behind #7129's timing table, kept so each
//! change under #7132 can show its own before/after rows against its own merge base.
//!
//! It prints one `BENCH` line per phase: wall time, and peak resident memory above the level the phase started at.
//! The peak is `VmHWM` after writing `5` to `/proc/self/clear_refs` at the phase start, minus `VmRSS` then. A
//! whole-process peak would hide the phases, because the export itself is built in memory first; `clear_refs` is
//! Linux-only, hence the module gate.
//!
//! The scenario is `#[ignore]`d, so CI never runs it, and it asserts nothing about speed: its only checks are that
//! the import answers `Ok` and the run answers an outcome, so a broken bench fails instead of printing a fast number
//! for work that did not happen. One sample is one process (the allocator keeps pages between samples in one
//! process, which would read later peaks low), so take five and use the median of each column, on one host:
//!
//! ```text
//! for i in 1 2 3 4 5; do
//!   cargo test --release -p aether-chassis-bloomery --test workspace_source -- \
//!     --ignored --exact bench::import_then_run --nocapture
//! done
//! ```
//!
//! Knobs, read from the shell because the harness boots hermetically:
//!
//! - `AETHER_WORKSPACE_PREFETCH_BYTES` is forwarded as `--workspace-prefetch-bytes`, so the whole-closure case can
//!   be read beside the default budget.
//! - `AETHER_BENCH_PHASE=import` stops after the import; unset runs both phases.

use std::error::Error;
use std::fs;
use std::time::{Duration, Instant};

use aether_bloomery_workspace::testing::{StubDaemon, StubReply, TarWriter};
use aether_bloomery_workspace::{ImageRef, Import, ImportResult, RunRequest};

use crate::run::{FLAGS, Inputs, built_work, outcome, over, script};
use crate::support::{CONTAINER, IMAGE, TestResult, answering};

/// The hang bound for one phase; the reply is what the harness waits on, so it never costs time on success.
const GUARD: Duration = Duration::from_secs(600);

const DIRECTORIES: u64 = 200;
const FILES_PER_DIRECTORY: u64 = 100;
const SMALL_FILE_BYTES: usize = 24 << 10;
const LARGE_FILE_BYTES: usize = 512 << 20;

/// `len` deterministic, incompressible-looking bytes from an xorshift stream seeded by `seed`.
fn fill(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut bytes = Vec::with_capacity(len);
    while bytes.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let word = state.to_le_bytes();
        bytes.extend_from_slice(&word[..word.len().min(len - bytes.len())]);
    }
    bytes
}

/// The export: `bench/dNNN/fNNN` files of [`SMALL_FILE_BYTES`] plus `bench/big.bin` of [`LARGE_FILE_BYTES`].
fn export() -> Vec<u8> {
    let mut tar = TarWriter::new().directory("bench/");
    for dir in 0..DIRECTORIES {
        tar = tar.directory(&format!("bench/d{dir:03}/"));
        for file in 0..FILES_PER_DIRECTORY {
            let content = fill(dir * 1000 + file + 1, SMALL_FILE_BYTES);
            tar = tar.file(&format!("bench/d{dir:03}/f{file:03}"), &content);
        }
    }
    tar.file("bench/big.bin", &fill(7, LARGE_FILE_BYTES)).finish()
}

/// One `/proc/self/status` field in bytes (the file reports kB).
fn rss_bytes(field: &str) -> Result<u64, Box<dyn Error>> {
    let status = fs::read_to_string("/proc/self/status")?;
    let line = status.lines().find_map(|line| line.strip_prefix(field)).ok_or("the status field is missing")?;
    let kib: u64 = line.trim_start_matches(':').trim().trim_end_matches("kB").trim().parse()?;
    Ok(kib << 10)
}

/// Reset the peak to the current resident size and answer that size.
fn mark() -> Result<u64, Box<dyn Error>> {
    fs::write("/proc/self/clear_refs", "5")?;
    rss_bytes("VmRSS")
}

/// The peak resident size since [`mark`] answered `base`, above `base`.
fn peak_over(base: u64) -> Result<u64, Box<dyn Error>> {
    Ok(rss_bytes("VmHWM")?.saturating_sub(base))
}

#[test]
#[ignore = "measurement: run on demand, see the module doc"]
fn import_then_run() -> TestResult {
    let inputs = Inputs::new(Vec::new())?;
    let (hex, template) = (inputs.hex(), inputs.request("tool", "target")?);
    let stub = StubDaemon::bind()?;

    #[allow(clippy::disallowed_methods)] // on-demand bench knob read from the shell; the harness boots hermetically
    let prefetch = std::env::var("AETHER_WORKSPACE_PREFETCH_BYTES").ok();
    let mut flags: Vec<&str> = FLAGS.to_vec();
    if let Some(bytes) = prefetch.as_deref() {
        flags.extend(["--workspace-prefetch-bytes", bytes]);
    }
    let mut harness = inputs.boot(&stub, &flags)?;

    let import = Import { image: ImageRef::new(IMAGE)?, source: harness.source() };
    let export = export();
    let replies = StubReply::import_script(IMAGE, CONTAINER, &export);
    drop(export);

    let base = mark()?;
    let started = Instant::now();
    let (answer, _) = answering(&stub, replies, || {
        let pending = harness.send_import(&import);
        harness.wait_within(pending, GUARD)
    })?;
    let import_millis = started.elapsed().as_millis();
    println!("BENCH import_millis={import_millis} import_peak_over_start_bytes={}", peak_over(base)?);
    let tree = match answer {
        ImportResult::Ok { tree } => tree,
        ImportResult::Failed { detail } => return Err(format!("the import failed: {}", detail.as_str()).into()),
    };

    #[allow(clippy::disallowed_methods)] // on-demand bench knob read from the shell; the harness boots hermetically
    let import_only = std::env::var("AETHER_BENCH_PHASE").is_ok_and(|phase| phase == "import");
    if import_only {
        return Ok(());
    }

    let run = over(&harness, RunRequest { tree, ..template });
    let output = built_work();
    let replies = script(&hex, &output).replies();

    let base = mark()?;
    let started = Instant::now();
    let (answer, _) = answering(&stub, replies, || {
        let pending = harness.send_run(&run);
        harness.wait_within(pending, GUARD)
    })?;
    let run_millis = started.elapsed().as_millis();
    println!(
        "BENCH run_millis={run_millis} run_peak_over_start_bytes={} prefetch_bytes={}",
        peak_over(base)?,
        prefetch.as_deref().unwrap_or("default")
    );
    outcome(answer)?;
    Ok(())
}
