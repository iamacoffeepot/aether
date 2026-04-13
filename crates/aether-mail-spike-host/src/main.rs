// Sequential mail-runtime spike: the four workloads from issue #7
// (broadcast, bulk, chain, mixed) with matrix bench harness and CSV output.
// Verdict recorded in ADR-0003. Shared types live in `lib.rs`.

use std::path::PathBuf;
use std::time::Duration;

use aether_mail_spike_host::{
    Actor, CellResult, GUEST_WASM, KIND_TICK, Mail, bench_loop, print_cell, u32_slice_as_bytes,
    write_csv,
};
use wasmtime::{Engine, Module};

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
                recipient: id as u32,
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

fn main() -> wasmtime::Result<()> {
    let engine = Engine::default();
    let module = Module::new(&engine, GUEST_WASM)?;
    let budget = Duration::from_secs(2);
    let results_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("results");

    // Workload 1 — broadcast: full matrix from #7.
    let actor_counts = [1usize, 2, 4, 8, 16, 32];
    let work_sizes = [100u32, 1_000, 10_000, 100_000];
    let mut broadcast_results: Vec<CellResult> = Vec::new();
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
