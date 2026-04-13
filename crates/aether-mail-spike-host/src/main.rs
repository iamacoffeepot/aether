// Mail-runtime spike: mail envelope, actor abstraction, and the four workloads
// from issue #7 (broadcast, bulk, chain, mixed) with matrix bench harness and
// CSV output. Throwaway code; abstractions kept as small as the spike needs.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use wasmtime::{Engine, Instance, Memory, Module, Store, TypedFunc};

const GUEST_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/guest.wasm"));

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

/// View a `&[u32]` as `&[u8]` without copying. Sound on all our targets
/// because u32 has no padding and wasm32 linear memory is little-endian
/// just like the hosts we run on.
fn u32_slice_as_bytes(slice: &[u32]) -> &[u8] {
    let len = std::mem::size_of_val(slice);
    // SAFETY: u32 is plain-old-data with no invalid representations; we're
    // narrowing the element type without changing the byte view.
    unsafe { std::slice::from_raw_parts(slice.as_ptr().cast::<u8>(), len) }
}

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

/// Workload 1 from #7. One tick mail to every actor each frame; each actor
/// does `work_per_actor` units of plain-data work; frame ends when every
/// actor has returned.
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

/// Workload 2 from #7. One sender + one receiver. Each frame the sender
/// hands the receiver a batch of `batch_size` u32 items; receiver runs
/// `work_per_item` work units on each. Tests how batching amortizes
/// per-mail boundary cost as the batch grows.
struct BulkWorkload {
    receiver: Actor,
    batch: Vec<u32>,
}

impl BulkWorkload {
    fn new(
        engine: &Engine,
        module: &Module,
        batch_size: usize,
        work_per_item: u32,
    ) -> wasmtime::Result<Self> {
        Ok(Self {
            receiver: Actor::new(engine, module)?,
            batch: vec![work_per_item; batch_size],
        })
    }

    fn tick(&mut self) -> wasmtime::Result<()> {
        let mail = Mail {
            recipient: 0,
            kind: KIND_TICK,
            batch_bytes: u32_slice_as_bytes(&self.batch),
            batch_count: self.batch.len() as u32,
        };
        self.receiver.deliver(&mail)?;
        Ok(())
    }
}

/// Workload 3 from #7. A chain of `depth` actors, dispatched sequentially
/// each frame. Per-link work is fixed; this measures the per-actor
/// dispatch cost as depth grows. The serial-dependency cost in this slice
/// is just the sequential dispatch (single-threaded execution); the
/// meaningful contrast against broadcast materializes once internal
/// parallelism lands and broadcast can run actors concurrently while
/// chain still cannot.
struct ChainWorkload {
    actors: Vec<Actor>,
    work_per_link: u32,
}

impl ChainWorkload {
    fn new(
        engine: &Engine,
        module: &Module,
        depth: usize,
        work_per_link: u32,
    ) -> wasmtime::Result<Self> {
        let actors = (0..depth)
            .map(|_| Actor::new(engine, module))
            .collect::<wasmtime::Result<Vec<_>>>()?;
        Ok(Self {
            actors,
            work_per_link,
        })
    }

    fn tick(&mut self) -> wasmtime::Result<()> {
        let payload = self.work_per_link.to_le_bytes();
        for actor in &mut self.actors {
            let mail = Mail {
                recipient: 0,
                kind: KIND_TICK,
                batch_bytes: &payload,
                batch_count: 1,
            };
            actor.deliver(&mail)?;
        }
        Ok(())
    }
}

/// Workload 4 from #7. A frame-shaped composite: a tick broadcast to all N
/// actors, followed by one small "neighbor" mail per actor (stand-in for
/// cross-subsystem coordination). Two phases per frame, 2N total mails.
struct MixedWorkload {
    actors: Vec<Actor>,
    work_per_actor: u32,
}

impl MixedWorkload {
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
        let tick_payload = self.work_per_actor.to_le_bytes();
        for actor in &mut self.actors {
            let mail = Mail {
                recipient: 0,
                kind: KIND_TICK,
                batch_bytes: &tick_payload,
                batch_count: 1,
            };
            actor.deliver(&mail)?;
        }
        // Cross-subsystem coordination phase: a small follow-up mail per
        // actor. Fixed small workload to keep the phase cheap relative to
        // the main tick.
        let neighbor_payload = 10u32.to_le_bytes();
        for actor in &mut self.actors {
            let mail = Mail {
                recipient: 0,
                kind: KIND_TICK,
                batch_bytes: &neighbor_payload,
                batch_count: 1,
            };
            actor.deliver(&mail)?;
        }
        Ok(())
    }
}

struct CellResult {
    workload: &'static str,
    dim_a: usize,
    dim_b: u32,
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

fn bench_loop(
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

fn write_csv(
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

fn print_cell(r: &CellResult) {
    eprintln!(
        "             iters={}  fps={:.0}  mean={:.1}us  p99={:.1}us",
        r.iterations,
        r.iterations as f64 / r.total.as_secs_f64(),
        r.mean.as_secs_f64() * 1_000_000.0,
        r.p99.as_secs_f64() * 1_000_000.0,
    );
}

fn main() -> wasmtime::Result<()> {
    let engine = Engine::default();
    let module = Module::new(&engine, GUEST_WASM)?;
    let budget = Duration::from_secs(2);
    let results_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("results");

    // Workload 1 — broadcast: full matrix from #7.
    let actor_counts = [1usize, 2, 4, 8, 16, 32];
    let work_sizes = [100u32, 1_000, 10_000, 100_000];
    let mut broadcast_results = Vec::new();
    for &n in &actor_counts {
        for &w in &work_sizes {
            eprintln!("broadcast  n={n:<3}  work={w:<7}  ...");
            let mut wl = BroadcastWorkload::new(&engine, &module, n, w)?;
            let r = bench_loop("broadcast", n, w, budget, || wl.tick())?;
            print_cell(&r);
            broadcast_results.push(r);
        }
    }
    write_csv(
        &results_dir.join("broadcast.csv"),
        "n_actors",
        "work_per_actor",
        &broadcast_results,
    )
    .map_err(|e| wasmtime::Error::msg(e.to_string()))?;

    // Workload 2 — bulk: 1 receiver, sweep batch_size with fixed per-item work.
    let batch_sizes = [1usize, 16, 256, 4096];
    let work_per_item = 100u32;
    let mut bulk_results = Vec::new();
    for &k in &batch_sizes {
        eprintln!("bulk       k={k:<5}  per_item={work_per_item}  ...");
        let mut wl = BulkWorkload::new(&engine, &module, k, work_per_item)?;
        let r = bench_loop("bulk", k, work_per_item, budget, || wl.tick())?;
        print_cell(&r);
        bulk_results.push(r);
    }
    write_csv(
        &results_dir.join("bulk.csv"),
        "batch_size",
        "work_per_item",
        &bulk_results,
    )
    .map_err(|e| wasmtime::Error::msg(e.to_string()))?;

    // Workload 3 — chain: sweep depth with fixed per-link work.
    let depths = [2usize, 4, 8, 16];
    let work_per_link = 1_000u32;
    let mut chain_results = Vec::new();
    for &d in &depths {
        eprintln!("chain      d={d:<3}    per_link={work_per_link}  ...");
        let mut wl = ChainWorkload::new(&engine, &module, d, work_per_link)?;
        let r = bench_loop("chain", d, work_per_link, budget, || wl.tick())?;
        print_cell(&r);
        chain_results.push(r);
    }
    write_csv(
        &results_dir.join("chain.csv"),
        "depth",
        "work_per_link",
        &chain_results,
    )
    .map_err(|e| wasmtime::Error::msg(e.to_string()))?;

    // Workload 4 — mixed: full matrix, broadcast tick + neighbor phase per frame.
    let mut mixed_results = Vec::new();
    for &n in &actor_counts {
        for &w in &work_sizes {
            eprintln!("mixed      n={n:<3}  work={w:<7}  ...");
            let mut wl = MixedWorkload::new(&engine, &module, n, w)?;
            let r = bench_loop("mixed", n, w, budget, || wl.tick())?;
            print_cell(&r);
            mixed_results.push(r);
        }
    }
    write_csv(
        &results_dir.join("mixed.csv"),
        "n_actors",
        "work_per_actor",
        &mixed_results,
    )
    .map_err(|e| wasmtime::Error::msg(e.to_string()))?;

    eprintln!(
        "\nwrote {}/{{broadcast,bulk,chain,mixed}}.csv",
        results_dir.display()
    );
    Ok(())
}
