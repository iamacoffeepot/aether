// Shared pieces for the spike binaries in this crate. Both the sequential
// mail-boundary spike (`src/main.rs`, issue #7 / ADR-0003) and the concurrent
// scheduler spike (`src/bin/concurrent.rs`, issue #14) need the same Actor /
// Mail types, wasm guest bytes, timing harness, and CSV writer — extracted
// here so they don't drift. Spike code; abstractions kept minimal.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use wasmtime::{Engine, Instance, Memory, Module, Store, TypedFunc};

pub mod scheduler;

pub const GUEST_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/guest.wasm"));

pub type ActorId = u32;
pub type MailKind = u32;

pub const KIND_TICK: MailKind = 1;

/// One mail. The recipient identifies which actor; the kind says how the
/// guest should interpret `batch_bytes`; `batch_count` is the number of
/// items the kind's layout implies (host and guest agree on per-item size).
pub struct Mail<'a> {
    // unused in some dispatch paths; kept because the envelope concept
    // includes addressing
    #[allow(dead_code)]
    pub recipient: ActorId,
    pub kind: MailKind,
    pub batch_bytes: &'a [u8],
    pub batch_count: u32,
}

/// View a `&[u32]` as `&[u8]` without copying. Sound on all our targets
/// because u32 has no padding and wasm32 linear memory is little-endian
/// just like the hosts we run on.
pub fn u32_slice_as_bytes(slice: &[u32]) -> &[u8] {
    let len = std::mem::size_of_val(slice);
    // SAFETY: u32 is plain-old-data with no invalid representations; we're
    // narrowing the element type without changing the byte view.
    unsafe { std::slice::from_raw_parts(slice.as_ptr().cast::<u8>(), len) }
}

/// One wasm instance plus the cached handles needed to deliver mail to it.
/// One `Store` per actor — wasmtime stores are not shareable across
/// concurrently-executing code paths (they are `Send` but not `Sync`).
pub struct Actor {
    store: Store<()>,
    memory: Memory,
    receive: TypedFunc<(u32, u32, u32), u32>,
}

impl Actor {
    pub fn new(engine: &Engine, module: &Module) -> wasmtime::Result<Self> {
        let mut store = Store::new(engine, ());
        let instance = Instance::new(&mut store, module, &[])?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| wasmtime::Error::msg("guest exports no memory"))?;
        let receive = instance.get_typed_func::<(u32, u32, u32), u32>(&mut store, "receive")?;
        Ok(Self {
            store,
            memory,
            receive,
        })
    }

    /// Writes the mail's bytes into the actor's linear memory at a fixed
    /// offset and invokes `receive`. Static-buffer convention for the spike;
    /// no guest-side allocator yet.
    pub fn deliver(&mut self, mail: &Mail) -> wasmtime::Result<u32> {
        const MAIL_OFFSET: u32 = 1024;
        self.memory
            .write(&mut self.store, MAIL_OFFSET as usize, mail.batch_bytes)?;
        self.receive
            .call(&mut self.store, (mail.kind, MAIL_OFFSET, mail.batch_count))
    }
}

pub struct CellResult {
    pub workload: &'static str,
    pub dim_a: usize,
    pub dim_b: u32,
    pub iterations: usize,
    pub total: Duration,
    pub p50: Duration,
    pub p95: Duration,
    pub p99: Duration,
    pub mean: Duration,
}

fn percentile(sorted: &[Duration], pct: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let idx = ((sorted.len() as f64 - 1.0) * pct / 100.0).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Run `tick` repeatedly inside `budget` (after a short warmup) and return
/// the per-frame latency distribution. Warmup is 5% of the budget.
pub fn bench_loop(
    workload: &'static str,
    dim_a: usize,
    dim_b: u32,
    budget: Duration,
    mut tick: impl FnMut() -> wasmtime::Result<()>,
) -> wasmtime::Result<CellResult> {
    let warmup_until = Instant::now() + budget / 20;
    while Instant::now() < warmup_until {
        tick()?;
    }

    let mut latencies: Vec<Duration> = Vec::with_capacity(1024);
    let started = Instant::now();
    let stop_at = started + budget;
    while Instant::now() < stop_at {
        let t = Instant::now();
        tick()?;
        latencies.push(t.elapsed());
    }
    let total = started.elapsed();

    latencies.sort_unstable();
    let mean = if latencies.is_empty() {
        Duration::ZERO
    } else {
        let sum: Duration = latencies.iter().sum();
        sum / (latencies.len() as u32)
    };

    Ok(CellResult {
        workload,
        dim_a,
        dim_b,
        iterations: latencies.len(),
        total,
        p50: percentile(&latencies, 50.0),
        p95: percentile(&latencies, 95.0),
        p99: percentile(&latencies, 99.0),
        mean,
    })
}

pub fn write_csv(
    path: &Path,
    dim_a_name: &str,
    dim_b_name: &str,
    rows: &[CellResult],
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut w = BufWriter::new(File::create(path)?);
    writeln!(
        w,
        "workload,{dim_a_name},{dim_b_name},iterations,total_ms,frames_per_sec,mean_us,p50_us,p95_us,p99_us"
    )?;
    for r in rows {
        let total_secs = r.total.as_secs_f64();
        let fps = if total_secs > 0.0 {
            r.iterations as f64 / total_secs
        } else {
            0.0
        };
        writeln!(
            w,
            "{},{},{},{},{:.3},{:.2},{:.3},{:.3},{:.3},{:.3}",
            r.workload,
            r.dim_a,
            r.dim_b,
            r.iterations,
            r.total.as_secs_f64() * 1000.0,
            fps,
            r.mean.as_secs_f64() * 1_000_000.0,
            r.p50.as_secs_f64() * 1_000_000.0,
            r.p95.as_secs_f64() * 1_000_000.0,
            r.p99.as_secs_f64() * 1_000_000.0,
        )?;
    }
    Ok(())
}

pub fn print_cell(r: &CellResult) {
    eprintln!(
        "             iters={}  fps={:.0}  mean={:.1}us  p99={:.1}us",
        r.iterations,
        r.iterations as f64 / r.total.as_secs_f64(),
        r.mean.as_secs_f64() * 1_000_000.0,
        r.p99.as_secs_f64() * 1_000_000.0,
    );
}
