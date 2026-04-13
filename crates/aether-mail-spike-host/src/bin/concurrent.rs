// Concurrent-scheduler spike (issue #14, PR A): parallel broadcast workload
// only. Dispatches a tick to every actor each frame across K worker threads
// using the hand-rolled `scheduler::Scheduler` from the lib. Matrix sweeps
// N (actor count) × K (worker count). Writes parallel_broadcast.csv.
//
// Mixed + churn workloads come in PR B; verdict + plotting + ADR-0004 in PR C.

use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use aether_mail_spike_host::{
    Actor, CellResult, GUEST_WASM, bench_loop, print_cell,
    scheduler::{Scheduler, Tick},
    write_csv,
};
use wasmtime::{Engine, Module};

/// Held constant across the N×K matrix. ADR-0003 already swept per-actor
/// work single-threaded; this spike is about the scheduler, not the
/// guest's ALU loop. 10k matches issue #7's gate cell.
const WORK_PER_ACTOR: u32 = 10_000;

struct ParallelBroadcastWorkload {
    scheduler: Scheduler,
}

impl ParallelBroadcastWorkload {
    fn new(
        engine: &Engine,
        module: &Module,
        n_actors: usize,
        k_workers: usize,
    ) -> wasmtime::Result<Self> {
        let actors = (0..n_actors)
            .map(|_| Actor::new(engine, module))
            .collect::<wasmtime::Result<Vec<_>>>()?;
        Ok(Self {
            scheduler: Scheduler::new(actors, k_workers),
        })
    }

    fn tick(&mut self) -> wasmtime::Result<()> {
        let n = self.scheduler.n_actors();
        let ticks: Vec<Tick> = (0..n)
            .map(|id| Tick {
                actor_id: id as u32,
                work_units: WORK_PER_ACTOR,
            })
            .collect();
        self.scheduler.run_frame(ticks);
        Ok(())
    }
}

fn main() -> wasmtime::Result<()> {
    let engine = Engine::default();
    let module = Module::new(&engine, GUEST_WASM)?;
    let budget = Duration::from_secs(2);
    let results_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("results");

    let all_cores = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    let actor_counts = [1usize, 8, 64, 512, 4096];
    let worker_counts = [1usize, 2, 4, all_cores];

    eprintln!("detected {all_cores} hardware threads (used as 'all-cores' K)");

    let mut results: Vec<CellResult> = Vec::new();
    for &n in &actor_counts {
        for &k in &worker_counts {
            eprintln!("parallel_broadcast  n={n:<5}  k={k:<3}  work={WORK_PER_ACTOR}  ...");
            let mut wl = ParallelBroadcastWorkload::new(&engine, &module, n, k)?;
            let r = bench_loop("parallel_broadcast", n, k as u32, budget, || wl.tick())?;
            print_cell(&r);
            results.push(r);
        }
    }
    write_csv(
        &results_dir.join("parallel_broadcast.csv"),
        "n_actors",
        "k_workers",
        &results,
    )
    .map_err(|e| wasmtime::Error::msg(e.to_string()))?;

    eprintln!("\nwrote {}/parallel_broadcast.csv", results_dir.display());
    Ok(())
}
