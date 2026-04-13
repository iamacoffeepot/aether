// Concurrent-scheduler spike (issue #14). Dispatches ticks to N actors
// across K worker threads using the hand-rolled `scheduler::Scheduler`
// from the lib. Three workloads × 5 N × 4 K = 60 cells.
//
//   parallel_broadcast — one tick per actor per frame, work=10_000.
//     Measures dispatch + scheduler scaling with real per-tick work.
//   parallel_mixed     — two phases per frame: tick (work=10_000) +
//     neighbor follow-up (work=10). 2N ticks/frame; measures the cost
//     of two barriers per logical frame.
//   churn              — one tick per actor per frame, work=10. Isolates
//     scheduler overhead from actor work; finds the dispatch-cost floor.
//
// Verdict + plotting + ADR-0004 land in PR C.

use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use aether_mail_spike_host::{
    Actor, CellResult, GUEST_WASM, bench_loop, print_cell,
    scheduler::{Scheduler, Tick},
    write_csv,
};
use wasmtime::{Engine, Module};

/// Representative per-actor work for the non-churn workloads. Matches
/// issue #7's gate cell so the scheduler numbers stay comparable to
/// ADR-0003's single-threaded baseline.
const WORK_HEAVY: u32 = 10_000;
/// Small neighbor-phase / churn work. Deliberately tiny so scheduler
/// cost is the dominant term.
const WORK_LIGHT: u32 = 10;

/// One tick per actor per frame, uniform work. Used for both
/// parallel_broadcast (work=WORK_HEAVY) and churn (work=WORK_LIGHT) —
/// same dispatch shape, different work cost, exposes the floor.
struct ParallelBroadcastWorkload {
    scheduler: Scheduler,
    work_per_actor: u32,
}

impl ParallelBroadcastWorkload {
    fn new(
        engine: &Engine,
        module: &Module,
        n_actors: usize,
        k_workers: usize,
        work_per_actor: u32,
    ) -> wasmtime::Result<Self> {
        let actors = (0..n_actors)
            .map(|_| Actor::new(engine, module))
            .collect::<wasmtime::Result<Vec<_>>>()?;
        Ok(Self {
            scheduler: Scheduler::new(actors, k_workers),
            work_per_actor,
        })
    }

    fn tick(&mut self) -> wasmtime::Result<()> {
        let n = self.scheduler.n_actors();
        let ticks: Vec<Tick> = (0..n)
            .map(|id| Tick {
                actor_id: id as u32,
                work_units: self.work_per_actor,
            })
            .collect();
        self.scheduler.run_frame(ticks);
        Ok(())
    }
}

/// Two barriered phases per frame: broadcast tick (WORK_HEAVY) then
/// neighbor follow-up (WORK_LIGHT). Matches sequential MixedWorkload's
/// shape. Each phase is its own `run_frame()` call so the frame barrier
/// fires twice — the cost of that extra sync shows up in the numbers.
struct ParallelMixedWorkload {
    scheduler: Scheduler,
}

impl ParallelMixedWorkload {
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
        let tick_phase: Vec<Tick> = (0..n)
            .map(|id| Tick {
                actor_id: id as u32,
                work_units: WORK_HEAVY,
            })
            .collect();
        self.scheduler.run_frame(tick_phase);

        let neighbor_phase: Vec<Tick> = (0..n)
            .map(|id| Tick {
                actor_id: id as u32,
                work_units: WORK_LIGHT,
            })
            .collect();
        self.scheduler.run_frame(neighbor_phase);
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

    let mut broadcast_results: Vec<CellResult> = Vec::new();
    for &n in &actor_counts {
        for &k in &worker_counts {
            eprintln!("parallel_broadcast  n={n:<5}  k={k:<3}  work={WORK_HEAVY}  ...");
            let mut wl = ParallelBroadcastWorkload::new(&engine, &module, n, k, WORK_HEAVY)?;
            let r = bench_loop("parallel_broadcast", n, k as u32, budget, || wl.tick())?;
            print_cell(&r);
            broadcast_results.push(r);
        }
    }
    write_csv(
        &results_dir.join("parallel_broadcast.csv"),
        "n_actors",
        "k_workers",
        &broadcast_results,
    )
    .map_err(|e| wasmtime::Error::msg(e.to_string()))?;

    let mut mixed_results: Vec<CellResult> = Vec::new();
    for &n in &actor_counts {
        for &k in &worker_counts {
            eprintln!("parallel_mixed      n={n:<5}  k={k:<3}  ...");
            let mut wl = ParallelMixedWorkload::new(&engine, &module, n, k)?;
            let r = bench_loop("parallel_mixed", n, k as u32, budget, || wl.tick())?;
            print_cell(&r);
            mixed_results.push(r);
        }
    }
    write_csv(
        &results_dir.join("parallel_mixed.csv"),
        "n_actors",
        "k_workers",
        &mixed_results,
    )
    .map_err(|e| wasmtime::Error::msg(e.to_string()))?;

    let mut churn_results: Vec<CellResult> = Vec::new();
    for &n in &actor_counts {
        for &k in &worker_counts {
            eprintln!("churn               n={n:<5}  k={k:<3}  work={WORK_LIGHT}  ...");
            let mut wl = ParallelBroadcastWorkload::new(&engine, &module, n, k, WORK_LIGHT)?;
            let r = bench_loop("churn", n, k as u32, budget, || wl.tick())?;
            print_cell(&r);
            churn_results.push(r);
        }
    }
    write_csv(
        &results_dir.join("churn.csv"),
        "n_actors",
        "k_workers",
        &churn_results,
    )
    .map_err(|e| wasmtime::Error::msg(e.to_string()))?;

    eprintln!(
        "\nwrote {}/{{parallel_broadcast,parallel_mixed,churn}}.csv",
        results_dir.display()
    );
    Ok(())
}
