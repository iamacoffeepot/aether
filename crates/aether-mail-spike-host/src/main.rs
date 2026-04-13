// Mail-runtime spike, slice 2: mail envelope, actor abstraction, broadcast
// workload, matrix bench harness, CSV output. Throwaway code; abstractions
// kept as small as the spike needs and no smaller. See issue #7.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use wasmtime::{Engine, Instance, Memory, Module, Store, TypedFunc};

const GUEST_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/guest.wasm"));

// --- mail envelope -----------------------------------------------------

type ActorId = u32;
type MailKind = u32;

const KIND_TICK: MailKind = 1;

/// One mail. The recipient identifies which actor; the kind says how the
/// guest should interpret `batch_bytes`; `batch_count` is the number of
/// items the kind's layout implies (host and guest agree on per-item size).
struct Mail<'a> {
    // unused in the bench loop's direct dispatch; kept because the envelope
    // concept includes addressing
    #[allow(dead_code)]
    recipient: ActorId,
    kind: MailKind,
    batch_bytes: &'a [u8],
    batch_count: u32,
}

// --- actor -------------------------------------------------------------

/// One wasm instance plus the cached handles needed to deliver mail to it.
/// One `Store` per actor — wasmtime stores are not shareable across
/// concurrently-executing code paths.
struct Actor {
    store: Store<()>,
    memory: Memory,
    receive: TypedFunc<(u32, u32, u32), u32>,
}

impl Actor {
    fn new(engine: &Engine, module: &Module) -> wasmtime::Result<Self> {
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
    /// no guest-side allocator yet (see #7's open sub-questions).
    fn deliver(&mut self, mail: &Mail) -> wasmtime::Result<u32> {
        const MAIL_OFFSET: u32 = 1024;
        self.memory
            .write(&mut self.store, MAIL_OFFSET as usize, mail.batch_bytes)?;
        self.receive
            .call(&mut self.store, (mail.kind, MAIL_OFFSET, mail.batch_count))
    }
}

// --- broadcast workload ------------------------------------------------

/// Workload 1 from #7. One `tick` mail to every actor each frame; each
/// actor does `work_per_actor` units of plain-data work; frame ends when
/// every actor has returned.
struct BroadcastWorkload {
    actors: Vec<Actor>,
    work_per_actor: u32,
}

impl BroadcastWorkload {
    fn new(
        engine: &Engine,
        module: &Module,
        n_actors: usize,
        work_per_actor: u32,
    ) -> wasmtime::Result<Self> {
        let actors = (0..n_actors)
            .map(|_| Actor::new(engine, module))
            .collect::<wasmtime::Result<Vec<_>>>()?;
        Ok(Self {
            actors,
            work_per_actor,
        })
    }

    fn tick(&mut self) -> wasmtime::Result<()> {
        let payload = self.work_per_actor.to_le_bytes();
        for (id, actor) in self.actors.iter_mut().enumerate() {
            let mail = Mail {
                recipient: id as ActorId,
                kind: KIND_TICK,
                batch_bytes: &payload,
                batch_count: 1,
            };
            actor.deliver(&mail)?;
        }
        Ok(())
    }
}

// --- bench harness -----------------------------------------------------

struct CellResult {
    workload: &'static str,
    n_actors: usize,
    work_per_actor: u32,
    iterations: usize,
    total: Duration,
    p50: Duration,
    p95: Duration,
    p99: Duration,
    mean: Duration,
}

fn percentile(sorted: &[Duration], pct: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let idx = ((sorted.len() as f64 - 1.0) * pct / 100.0).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn bench_broadcast(
    engine: &Engine,
    module: &Module,
    n_actors: usize,
    work_per_actor: u32,
    budget: Duration,
) -> wasmtime::Result<CellResult> {
    let mut workload = BroadcastWorkload::new(engine, module, n_actors, work_per_actor)?;

    // Warm up for ~5% of the budget so JIT and caches settle before timing.
    let warmup_until = Instant::now() + budget / 20;
    while Instant::now() < warmup_until {
        workload.tick()?;
    }

    let mut latencies: Vec<Duration> = Vec::with_capacity(1024);
    let started = Instant::now();
    let stop_at = started + budget;
    while Instant::now() < stop_at {
        let t = Instant::now();
        workload.tick()?;
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
        workload: "broadcast",
        n_actors,
        work_per_actor,
        iterations: latencies.len(),
        total,
        p50: percentile(&latencies, 50.0),
        p95: percentile(&latencies, 95.0),
        p99: percentile(&latencies, 99.0),
        mean,
    })
}

// --- CSV output --------------------------------------------------------

fn write_csv(path: &PathBuf, rows: &[CellResult]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut w = BufWriter::new(File::create(path)?);
    writeln!(
        w,
        "workload,n_actors,work_per_actor,iterations,total_ms,frames_per_sec,mails_per_sec,mean_us,p50_us,p95_us,p99_us"
    )?;
    for r in rows {
        let total_secs = r.total.as_secs_f64();
        let fps = if total_secs > 0.0 {
            r.iterations as f64 / total_secs
        } else {
            0.0
        };
        let mps = fps * r.n_actors as f64;
        writeln!(
            w,
            "{},{},{},{},{:.3},{:.2},{:.2},{:.3},{:.3},{:.3},{:.3}",
            r.workload,
            r.n_actors,
            r.work_per_actor,
            r.iterations,
            r.total.as_secs_f64() * 1000.0,
            fps,
            mps,
            r.mean.as_secs_f64() * 1_000_000.0,
            r.p50.as_secs_f64() * 1_000_000.0,
            r.p95.as_secs_f64() * 1_000_000.0,
            r.p99.as_secs_f64() * 1_000_000.0,
        )?;
    }
    Ok(())
}

// --- entry point -------------------------------------------------------

fn main() -> wasmtime::Result<()> {
    let engine = Engine::default();
    let module = Module::new(&engine, GUEST_WASM)?;

    // Matrix from #7. Capped at 32 actors per ADR-0002's granularity
    // discipline; see the issue for rationale.
    let actor_counts = [1usize, 2, 4, 8, 16, 32];
    let work_sizes = [100u32, 1_000, 10_000, 100_000];
    let budget_per_cell = Duration::from_secs(2);

    let mut results = Vec::new();
    for &n in &actor_counts {
        for &w in &work_sizes {
            eprintln!("broadcast  n={n:<3}  work={w:<7}  ...");
            let r = bench_broadcast(&engine, &module, n, w, budget_per_cell)?;
            eprintln!(
                "             iters={}  fps={:.0}  mean={:.1}us  p99={:.1}us",
                r.iterations,
                r.iterations as f64 / r.total.as_secs_f64(),
                r.mean.as_secs_f64() * 1_000_000.0,
                r.p99.as_secs_f64() * 1_000_000.0,
            );
            results.push(r);
        }
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_path = manifest_dir.join("results").join("broadcast.csv");
    write_csv(&out_path, &results).map_err(|e| wasmtime::Error::msg(e.to_string()))?;
    eprintln!("\nwrote {}", out_path.display());

    Ok(())
}
