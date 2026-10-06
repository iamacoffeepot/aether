//! Sustained overload: `loopers` producers log as fast as the workers can
//! dispatch them, on their own chains, for `secs` seconds, while frames are
//! offered at `pace` per second. A sampling thread prints the backlog and
//! resident memory over time; when the flood stops the run measures how long
//! the backlog and the frame time take to come back.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::process::resident_mebibytes;
use crate::rig::{Args, Rig, Setup, quantile};
use crate::stats::{self, GATHER_PENDING, LOAD_LINES, LOOP_START, STOP, snapshot};

pub const TIMELINE_HEADER: &str = "row,mode,logpath,actors,loopers,t_s,phase,inbox_depth,buffer_occupancy,gather_pending,\
rss_mib,frames_done,frame_in_progress_ms,worst_frame_ms,produced,delivered,skipped";
pub const SUMMARY_HEADER: &str = "row,mode,logpath,workers,actors,loopers,flood_secs,flood_ended_by,baseline_frame_p50_us,\
flood_frames,flood_frame_p50_us,flood_frame_max_us,produced_per_sec,peak_inbox,peak_buffer,peak_rss_mib,start_rss_mib,\
backlog_zero_after_s,frame_time_back_after_s,produced_total,delivered_total,skipped_total,dropped_total,cap_skipped_total,\
ring_lost_total,foreign_total,unaccounted";

static PHASE: AtomicU64 = AtomicU64::new(0);
static FRAMES_DONE: AtomicU64 = AtomicU64::new(0);
static FRAME_STARTED_MICROS: AtomicU64 = AtomicU64::new(0);
static WORST_FRAME_MICROS: AtomicU64 = AtomicU64::new(0);
static PEAK_INBOX: AtomicU64 = AtomicU64::new(0);
static PEAK_BUFFER: AtomicU64 = AtomicU64::new(0);
static PEAK_RSS: AtomicU64 = AtomicU64::new(0);
static OVER_MEMORY: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);

const PHASES: [&str; 3] = ["baseline", "flood", "recover"];

struct Sampler {
    origin: Instant,
    label: String,
    period: Duration,
    rss_cap_mebibytes: f64,
}

impl Sampler {
    fn run(self) {
        while !DONE.load(Ordering::Relaxed) {
            thread::sleep(self.period);
            let now_micros = u64::try_from(self.origin.elapsed().as_micros()).unwrap_or(0);
            let inbox = stats::inbox_depth();
            let buffer = stats::buffer_occupancy();
            let rss = resident_mebibytes();
            let frame_started = FRAME_STARTED_MICROS.load(Ordering::Relaxed);
            let in_progress = if frame_started == 0 {
                0.0
            } else {
                (now_micros.saturating_sub(frame_started)) as f64 / 1000.0
            };
            let worst = WORST_FRAME_MICROS.swap(0, Ordering::Relaxed) as f64 / 1000.0;
            PEAK_INBOX.fetch_max(inbox, Ordering::Relaxed);
            PEAK_BUFFER.fetch_max(buffer, Ordering::Relaxed);
            PEAK_RSS.fetch_max(rss as u64, Ordering::Relaxed);
            println!(
                "T,{},{:.2},{},{inbox},{buffer},{},{rss:.0},{},{in_progress:.1},{worst:.1},{},{},{}",
                self.label,
                now_micros as f64 / 1e6,
                PHASES[PHASE.load(Ordering::Relaxed) as usize],
                stats::get(&GATHER_PENDING),
                FRAMES_DONE.load(Ordering::Relaxed),
                stats::produced(),
                stats::get(&stats::CONSOLE_LINES),
                stats::get(&stats::CONSOLE_SKIPPED),
            );
            if rss > self.rss_cap_mebibytes && !STOP.load(Ordering::Relaxed) {
                OVER_MEMORY.store(true, Ordering::Relaxed);
                STOP.store(true, Ordering::Relaxed);
            }
        }
    }
}

/// Run one paced frame, publishing its start so the sampler can show a frame
/// that has not come back.
fn tracked_frame(rig: &mut Rig, origin: Instant, pace_hz: u64) -> Result<u64, String> {
    FRAME_STARTED_MICROS.store(u64::try_from(origin.elapsed().as_micros()).unwrap_or(1).max(1), Ordering::Relaxed);
    let took = u64::try_from(rig.paced_frame(pace_hz)?.as_micros()).unwrap_or(u64::MAX);
    FRAME_STARTED_MICROS.store(0, Ordering::Relaxed);
    FRAMES_DONE.fetch_add(1, Ordering::Relaxed);
    WORST_FRAME_MICROS.fetch_max(took, Ordering::Relaxed);
    Ok(took)
}

pub fn run(args: &Args) -> Result<(), String> {
    if args.number("header", 0) != 0 {
        println!("{TIMELINE_HEADER}");
        println!("{SUMMARY_HEADER}");
        return Ok(());
    }
    let setup = Setup::from_args(args)?;
    let loopers =
        usize::try_from(args.number("loopers", setup.actors as u64)).unwrap_or(setup.actors).min(setup.actors);
    let lines = u32::try_from(args.number("lines", 1)).unwrap_or(1);
    let secs = args.number("secs", 30);
    let pace_hz = args.number("pace", 60);
    let recover_limit = Duration::from_secs(args.number("recover_secs", 120));

    let mut rig = Rig::boot(&setup)?;
    rig.attach(&setup, true)?;
    let origin = Instant::now();
    let label = format!("{},{},{},{loopers}", setup.mode, setup.logpath, setup.actors);
    let sampler = Sampler {
        origin,
        label,
        period: Duration::from_millis(args.number("sample_millis", 1000)),
        rss_cap_mebibytes: args.number("rss_cap_mib", 8192) as f64,
    };
    let sampling = thread::spawn(move || sampler.run());

    let mut baseline = Vec::new();
    for _ in 0..120 {
        baseline.push(tracked_frame(&mut rig, origin, pace_hz)?);
    }
    baseline.sort_unstable();
    let baseline_p50 = quantile(&baseline, 0.5);
    let start_rss = resident_mebibytes();

    PHASE.store(1, Ordering::Relaxed);
    LOAD_LINES.store(u64::from(lines), Ordering::Relaxed);
    LOOP_START.store(loopers as u64, Ordering::Relaxed);
    let flood_started = Instant::now();
    let produced_at_start = stats::produced();
    let mut flood = Vec::new();
    while flood_started.elapsed() < Duration::from_secs(secs) && !STOP.load(Ordering::Relaxed) {
        flood.push(tracked_frame(&mut rig, origin, pace_hz)?);
    }
    let flood_secs = flood_started.elapsed().as_secs_f64();
    let produced_per_sec = (stats::produced() - produced_at_start) as f64 / flood_secs;
    let ended_by = if OVER_MEMORY.load(Ordering::Relaxed) {
        "memory_cap"
    } else {
        "time"
    };

    STOP.store(true, Ordering::Relaxed);
    PHASE.store(2, Ordering::Relaxed);
    let stopped = Instant::now();
    let settled_frame = baseline_p50.saturating_mul(3) / 2 + 200;
    let mut backlog_zero_after = None;
    let mut frame_back_after = None;
    let mut calm_since = None;
    let mut calm_frames = 0;
    while stopped.elapsed() < recover_limit {
        let frame_began = stopped.elapsed();
        let took = tracked_frame(&mut rig, origin, pace_hz)?;
        let empty = stats::inbox_depth() == 0 && stats::get(&GATHER_PENDING) == 0 && stats::buffer_occupancy() == 0;
        if empty && backlog_zero_after.is_none() {
            backlog_zero_after = Some(stopped.elapsed().as_secs_f64());
        }
        if took <= settled_frame {
            calm_since.get_or_insert(frame_began);
            calm_frames += 1;
        } else {
            calm_since = None;
            calm_frames = 0;
        }
        if calm_frames >= 30 && frame_back_after.is_none() {
            frame_back_after = calm_since.map(|since: Duration| since.as_secs_f64());
        }
        if backlog_zero_after.is_some() && frame_back_after.is_some() {
            break;
        }
    }
    let _ = rig.drain(3000)?;
    DONE.store(true, Ordering::Relaxed);
    let _ = sampling.join();

    flood.sort_unstable();
    let end = snapshot();
    let unaccounted = end.produced as i64 - end.console_lines as i64 - end.console_skipped as i64;
    let seconds = |value: Option<f64>| value.map_or("never".to_owned(), |secs| format!("{secs:.3}"));
    println!(
        "S,{},{},{},{},{loopers},{flood_secs:.2},{ended_by},{baseline_p50},{},{},{},{produced_per_sec:.0},{},{},{},\
         {start_rss:.0},{},{},{},{},{},{},{},{},{},{unaccounted}",
        setup.mode,
        setup.logpath,
        setup.workers,
        setup.actors,
        flood.len(),
        quantile(&flood, 0.5),
        flood.last().copied().unwrap_or(0),
        PEAK_INBOX.load(Ordering::Relaxed),
        PEAK_BUFFER.load(Ordering::Relaxed),
        PEAK_RSS.load(Ordering::Relaxed),
        seconds(backlog_zero_after),
        seconds(frame_back_after),
        end.produced,
        end.console_lines,
        end.console_skipped,
        end.dropped,
        end.cap_skipped,
        end.ring_lost,
        end.console_foreign,
    );
    Ok(())
}
