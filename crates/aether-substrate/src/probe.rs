//! Throwaway per-thread instrumentation for the issue #4177 drain-tail
//! investigation. NEVER to be merged — every hook is temporary and the
//! whole module leaves with the probe branch.
//!
//! Design constraints (issue #4177 method rules): no shared synchronisation
//! on any measured path — each thread folds into its own `Arc<Acc>`
//! (registered once per thread lifetime, merged only at [`dump`]), and the
//! only cross-thread lock is the slow-cycle capture, taken strictly *after*
//! a cycle completes (outside every measured span) and only for cycles
//! already classified slow (a handful per cell).

#![allow(clippy::print_stderr)]
#![allow(missing_docs)]
#![allow(clippy::cast_possible_truncation)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// Max in-cycle dispatch positions tracked (fanout-4's widest demux cycle
/// dispatches 4 mails).
pub const POSITIONS: usize = 8;
/// A blob cycle slower than this (nanos) is captured in full.
pub const SLOW_CYCLE_NANOS: u64 = 2_500;
const SLOW_CAP: usize = 64;

/// Registry seal gate (#4177): `true` = production behaviour (the ADR-0165
/// seal runs), `false` = the pre-seal direct-commit path. Flipped only by
/// the perf-trial unsealed replay.
static SEAL_ENABLED: AtomicBool = AtomicBool::new(true);

#[must_use]
pub fn seal_enabled() -> bool {
    SEAL_ENABLED.load(Relaxed)
}

pub fn set_seal_enabled(v: bool) {
    SEAL_ENABLED.store(v, Relaxed);
}

/// Current CPU id (Linux `sched_getcpu`; 255 where unavailable).
#[must_use]
pub fn current_cpu() -> u8 {
    #[cfg(target_os = "linux")]
    {
        let cpu = unsafe { libc::sched_getcpu() };
        if cpu < 0 {
            255
        } else {
            cpu.min(254) as u8
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        255
    }
}

/// This thread's kernel tid (Linux; 0 elsewhere).
fn current_tid() -> u64 {
    #[cfg(target_os = "linux")]
    {
        unsafe { libc::syscall(libc::SYS_gettid) as u64 }
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

/// Read `voluntary_ctxt_switches` / `nonvoluntary_ctxt_switches` for a tid
/// from `/proc/self/task/<tid>/status`. `None` off Linux or if the thread
/// is gone.
fn ctxt_switches(tid: u64) -> Option<(u64, u64)> {
    if tid == 0 {
        return None;
    }
    let text = std::fs::read_to_string(format!("/proc/self/task/{tid}/status")).ok()?;
    let mut vol = None;
    let mut nonvol = None;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("voluntary_ctxt_switches:") {
            vol = v.trim().parse::<u64>().ok();
        } else if let Some(v) = line.strip_prefix("nonvoluntary_ctxt_switches:") {
            nonvol = v.trim().parse::<u64>().ok();
        }
    }
    Some((vol?, nonvol?))
}

/// Process thread count from `/proc/self/status` (0 off Linux).
fn process_threads() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|t| t.lines().find_map(|l| l.strip_prefix("Threads:").and_then(|v| v.trim().parse::<u64>().ok())))
        .unwrap_or(0)
}

#[derive(Clone, Copy, Default)]
pub struct Part {
    pub route_ns: u32,
    pub run_ns: u32,
    pub seized: bool,
}

/// One blob cycle's in-order per-mail breakdown, accumulated on the
/// draining worker's stack and folded into its thread `Acc` at cycle end.
#[derive(Default)]
pub struct CycleRec {
    pub n: usize,
    pub parts: [Part; 16],
}

impl CycleRec {
    pub fn push(&mut self, route_ns: u64, run_ns: u64, seized: bool) {
        if self.n < self.parts.len() {
            self.parts[self.n] = Part {
                route_ns: route_ns.min(u64::from(u32::MAX)) as u32,
                run_ns: run_ns.min(u64::from(u32::MAX)) as u32,
                seized,
            };
        }
        self.n += 1;
    }
}

struct SlowCycle {
    thread: String,
    total_ns: u64,
    n: usize,
    parts: [Part; 16],
    cpu_start: u8,
    cpu_end: u8,
}

#[derive(Default)]
pub struct Acc {
    // Blob demux decomposition, per in-cycle dispatch position.
    blob_cycles: AtomicU64,
    blob_cycle_ns: AtomicU64,
    pos_count: [AtomicU64; POSITIONS],
    pos_route_ns: [AtomicU64; POSITIONS],
    pos_route_max: [AtomicU64; POSITIONS],
    pos_run_ns: [AtomicU64; POSITIONS],
    pos_run_max: [AtomicU64; POSITIONS],
    pos_deposit: [AtomicU64; POSITIONS],
    // Pooled-cycle census by slot label (what this worker actually ran).
    cyc_blob: AtomicU64,
    cyc_owner: AtomicU64,
    cyc_activation: AtomicU64,
    cyc_other: AtomicU64,
    // Worker-side scheduler census.
    own_pops: AtomicU64,
    injector_steals: AtomicU64,
    peer_steals: AtomicU64,
    parks: AtomicU64,
    stamped_wakes: AtomicU64,
    unstamped_wakes: AtomicU64,
    spin_entries: AtomicU64,
    // Producer-side census.
    notify_spinner: AtomicU64,
    notify_unpark: AtomicU64,
    notify_noidle: AtomicU64,
    wake_req: AtomicU64,
    wake_unparked: AtomicU64,
    flushes: AtomicU64,
    recruit_fires: AtomicU64,
    keep_local: AtomicU64,
    spills: AtomicU64,
    // CPU residency: which vCPU this thread's blob cycles start on (and,
    // for the embedder, which vCPU its notifies run on).
    cpu_hist: [AtomicU64; 16],
    cpu_migrations: AtomicU64,
    // Last-dumped cumulative context-switch counts, so each dump prints
    // the per-window delta.
    vol_last: AtomicU64,
    nonvol_last: AtomicU64,
}

fn registry() -> &'static Mutex<Vec<(String, u64, Arc<Acc>)>> {
    static R: OnceLock<Mutex<Vec<(String, u64, Arc<Acc>)>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(Vec::new()))
}

fn slow_store() -> &'static Mutex<Vec<SlowCycle>> {
    static S: OnceLock<Mutex<Vec<SlowCycle>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(Vec::new()))
}

thread_local! {
    static ACC: Arc<Acc> = {
        let name = std::thread::current()
            .name()
            .map_or_else(|| format!("{:?}", std::thread::current().id()), str::to_owned);
        let acc = Arc::new(Acc::default());
        registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((name, current_tid(), Arc::clone(&acc)));
        acc
    };
}

#[inline]
fn with<R>(f: impl FnOnce(&Acc) -> R) -> R {
    ACC.with(|a| f(a))
}

#[inline]
pub fn record_cycle(elapsed: Duration, rec: &CycleRec, cpu_start: u8, cpu_end: u8) {
    if rec.n == 0 {
        return;
    }
    let total_ns = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
    with(|a| {
        a.blob_cycles.fetch_add(1, Relaxed);
        a.blob_cycle_ns.fetch_add(total_ns, Relaxed);
        a.cpu_hist[usize::from(cpu_start.min(15))].fetch_add(1, Relaxed);
        if cpu_start != cpu_end {
            a.cpu_migrations.fetch_add(1, Relaxed);
        }
        for (i, p) in rec.parts.iter().take(rec.n.min(POSITIONS)).enumerate() {
            a.pos_count[i].fetch_add(1, Relaxed);
            a.pos_route_ns[i].fetch_add(u64::from(p.route_ns), Relaxed);
            a.pos_route_max[i].fetch_max(u64::from(p.route_ns), Relaxed);
            a.pos_run_ns[i].fetch_add(u64::from(p.run_ns), Relaxed);
            a.pos_run_max[i].fetch_max(u64::from(p.run_ns), Relaxed);
            if !p.seized {
                a.pos_deposit[i].fetch_add(1, Relaxed);
            }
        }
    });
    if total_ns > SLOW_CYCLE_NANOS {
        let thread =
            std::thread::current().name().map_or_else(|| format!("{:?}", std::thread::current().id()), str::to_owned);
        let mut store = slow_store().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if store.len() < SLOW_CAP {
            store.push(SlowCycle { thread, total_ns, n: rec.n, parts: rec.parts, cpu_start, cpu_end });
        } else if let Some(min_idx) = store.iter().enumerate().min_by_key(|(_, s)| s.total_ns).map(|(i, _)| i)
            && store[min_idx].total_ns < total_ns
        {
            store[min_idx] = SlowCycle { thread, total_ns, n: rec.n, parts: rec.parts, cpu_start, cpu_end };
        }
    }
}

/// Fold the calling thread's current CPU into its residency histogram —
/// used by the embedder's per-frame notify so its vCPU shows up next to
/// the workers'.
#[inline]
pub fn note_cpu() {
    let cpu = current_cpu();
    with(|a| a.cpu_hist[usize::from(cpu.min(15))].fetch_add(1, Relaxed));
}

#[inline]
pub fn count_slot_label(label: &str) {
    with(|a| {
        match label {
            "blob" => a.cyc_blob.fetch_add(1, Relaxed),
            "registry-owner" => a.cyc_owner.fetch_add(1, Relaxed),
            "native-activation" => a.cyc_activation.fetch_add(1, Relaxed),
            _ => a.cyc_other.fetch_add(1, Relaxed),
        };
    });
}

#[inline]
pub fn on_own_pop() {
    with(|a| a.own_pops.fetch_add(1, Relaxed));
}

#[inline]
pub fn on_injector_steal() {
    with(|a| a.injector_steals.fetch_add(1, Relaxed));
}

#[inline]
pub fn on_peer_steal() {
    with(|a| a.peer_steals.fetch_add(1, Relaxed));
}

#[inline]
pub fn on_park() {
    with(|a| a.parks.fetch_add(1, Relaxed));
}

#[inline]
pub fn on_wake(stamped: bool) {
    with(|a| {
        if stamped {
            a.stamped_wakes.fetch_add(1, Relaxed)
        } else {
            a.unstamped_wakes.fetch_add(1, Relaxed)
        }
    });
}

#[inline]
pub fn on_spin_entry() {
    with(|a| a.spin_entries.fetch_add(1, Relaxed));
}

pub enum NotifyOutcome {
    SpinnerHit,
    Unparked,
    NoIdle,
}

#[inline]
pub fn on_notify(outcome: &NotifyOutcome) {
    with(|a| {
        match outcome {
            NotifyOutcome::SpinnerHit => a.notify_spinner.fetch_add(1, Relaxed),
            NotifyOutcome::Unparked => a.notify_unpark.fetch_add(1, Relaxed),
            NotifyOutcome::NoIdle => a.notify_noidle.fetch_add(1, Relaxed),
        };
    });
}

#[inline]
pub fn on_wake_workers(requested: usize, unparked: usize) {
    with(|a| {
        a.wake_req.fetch_add(requested as u64, Relaxed);
        a.wake_unparked.fetch_add(unparked as u64, Relaxed);
    });
}

#[inline]
pub fn on_flush(recruit_extra: usize) {
    with(|a| {
        a.flushes.fetch_add(1, Relaxed);
        if recruit_extra > 0 {
            a.recruit_fires.fetch_add(1, Relaxed);
        }
    });
}

#[inline]
pub fn on_schedule(kept: bool) {
    with(|a| {
        if kept {
            a.keep_local.fetch_add(1, Relaxed)
        } else {
            a.spills.fetch_add(1, Relaxed)
        }
    });
}

fn mean(sum: u64, n: u64) -> u64 {
    if n == 0 {
        0
    } else {
        sum / n
    }
}

/// Print every thread's accumulated counters (nonzero threads only) plus
/// the captured slow cycles, then reset everything. Called at the probe
/// cell boundaries, so each dump covers exactly one phase.
pub fn dump(label: &str) {
    eprintln!("PROBE[{label}] process threads={}", process_threads());
    let reg = registry().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    for (name, tid, a) in reg.iter() {
        let cycles = a.blob_cycles.load(Relaxed);
        let scheduler_activity = a.own_pops.load(Relaxed)
            + a.injector_steals.load(Relaxed)
            + a.peer_steals.load(Relaxed)
            + a.parks.load(Relaxed)
            + a.flushes.load(Relaxed)
            + a.notify_spinner.load(Relaxed)
            + a.notify_unpark.load(Relaxed)
            + a.notify_noidle.load(Relaxed)
            + a.spin_entries.load(Relaxed);
        if cycles == 0 && scheduler_activity == 0 {
            continue;
        }
        // CPU residency histogram + per-window context-switch deltas.
        let mut cpus = String::new();
        for c in 0..16 {
            let n = a.cpu_hist[c].load(Relaxed);
            if n > 0 {
                cpus.push_str(&format!(" c{c}={n}"));
            }
        }
        let ctxt = match ctxt_switches(*tid) {
            Some((vol, nonvol)) => {
                let dv = vol.saturating_sub(a.vol_last.swap(vol, Relaxed));
                let dn = nonvol.saturating_sub(a.nonvol_last.swap(nonvol, Relaxed));
                format!(" ctxt v/nv=+{dv}/+{dn}")
            }
            None => String::new(),
        };
        let mut pos = String::new();
        for i in 0..POSITIONS {
            let n = a.pos_count[i].load(Relaxed);
            if n == 0 {
                continue;
            }
            pos.push_str(&format!(
                " p{}[n={} rt={}me/{}mx run={}me/{}mx dep={}]",
                i,
                n,
                mean(a.pos_route_ns[i].load(Relaxed), n),
                a.pos_route_max[i].load(Relaxed),
                mean(a.pos_run_ns[i].load(Relaxed), n),
                a.pos_run_max[i].load(Relaxed),
                a.pos_deposit[i].load(Relaxed),
            ));
        }
        eprintln!(
            "PROBE[{label}] {name}: blob_cyc={} cyc_ns_me={}{pos} | cpu[{cpus} mig={}]{ctxt} | slots b/o/a/x={}/{}/{}/{} | pops o/i/p={}/{}/{} parks={} wake s/u={}/{} spin={} | ntfy s/u/n={}/{}/{} wkw {}/{} flush={} recruit={} keep/spill={}/{}",
            cycles,
            mean(a.blob_cycle_ns.load(Relaxed), cycles),
            a.cpu_migrations.load(Relaxed),
            a.cyc_blob.load(Relaxed),
            a.cyc_owner.load(Relaxed),
            a.cyc_activation.load(Relaxed),
            a.cyc_other.load(Relaxed),
            a.own_pops.load(Relaxed),
            a.injector_steals.load(Relaxed),
            a.peer_steals.load(Relaxed),
            a.parks.load(Relaxed),
            a.stamped_wakes.load(Relaxed),
            a.unstamped_wakes.load(Relaxed),
            a.spin_entries.load(Relaxed),
            a.notify_spinner.load(Relaxed),
            a.notify_unpark.load(Relaxed),
            a.notify_noidle.load(Relaxed),
            a.wake_req.load(Relaxed),
            a.wake_unparked.load(Relaxed),
            a.flushes.load(Relaxed),
            a.recruit_fires.load(Relaxed),
            a.keep_local.load(Relaxed),
            a.spills.load(Relaxed),
        );
        // Reset.
        a.blob_cycles.store(0, Relaxed);
        a.blob_cycle_ns.store(0, Relaxed);
        for i in 0..POSITIONS {
            a.pos_count[i].store(0, Relaxed);
            a.pos_route_ns[i].store(0, Relaxed);
            a.pos_route_max[i].store(0, Relaxed);
            a.pos_run_ns[i].store(0, Relaxed);
            a.pos_run_max[i].store(0, Relaxed);
            a.pos_deposit[i].store(0, Relaxed);
        }
        a.cyc_blob.store(0, Relaxed);
        a.cyc_owner.store(0, Relaxed);
        a.cyc_activation.store(0, Relaxed);
        a.cyc_other.store(0, Relaxed);
        a.own_pops.store(0, Relaxed);
        a.injector_steals.store(0, Relaxed);
        a.peer_steals.store(0, Relaxed);
        a.parks.store(0, Relaxed);
        a.stamped_wakes.store(0, Relaxed);
        a.unstamped_wakes.store(0, Relaxed);
        a.spin_entries.store(0, Relaxed);
        a.notify_spinner.store(0, Relaxed);
        a.notify_unpark.store(0, Relaxed);
        a.notify_noidle.store(0, Relaxed);
        a.wake_req.store(0, Relaxed);
        a.wake_unparked.store(0, Relaxed);
        a.flushes.store(0, Relaxed);
        a.recruit_fires.store(0, Relaxed);
        a.keep_local.store(0, Relaxed);
        a.spills.store(0, Relaxed);
        for c in 0..16 {
            a.cpu_hist[c].store(0, Relaxed);
        }
        a.cpu_migrations.store(0, Relaxed);
    }
    drop(reg);
    let mut slow = slow_store().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    slow.sort_by_key(|s| std::cmp::Reverse(s.total_ns));
    for s in slow.iter() {
        let parts: Vec<String> = s
            .parts
            .iter()
            .take(s.n.min(16))
            .map(|p| {
                format!(
                    "rt{}+run{}{}",
                    p.route_ns,
                    p.run_ns,
                    if p.seized {
                        ""
                    } else {
                        "!dep"
                    }
                )
            })
            .collect();
        eprintln!(
            "PROBE[{label}] SLOW {} total={}ns n={} cpu={}->{} [{}]",
            s.thread,
            s.total_ns,
            s.n,
            s.cpu_start,
            s.cpu_end,
            parts.join(", ")
        );
    }
    slow.clear();
}
