//! Steady load: every frame the driver offers `handlers` logging handlers of
//! `lines` lines each and `quiet` handlers that log nothing, at `pace` frames
//! per second (0 runs frames back to back).

use std::sync::PoisonError;
use std::time::Instant;

use crate::process::{cpu_secs, resident_mebibytes};
use crate::rig::{Args, Rig, Setup, quantile, set_load};
use crate::stats::{self, CONSOLE_BATCH_SIZES, CONSOLE_MAX_NANOS, GATHER_TICK_MAX_NANOS, snapshot};

pub const HEADER: &str = "scenario,mode,logpath,workers,actors,handlers,lines,quiet,pace_hz,frames,\
frame_p50_us,frame_p90_us,frame_p99_us,frame_max_us,wall_s,cpu_s,\
produced_window,slices_window,gather_ticks_window,gather_tick_ns_mean,gather_tick_ns_max,gather_slice_ns_ewma,\
console_batches_window,console_lines_window,batch_p50,batch_max,console_ns_per_batch,console_ns_max,\
inbox_peak,buffer_peak,rss_mib,\
produced_total,delivered_total,skipped_total,dropped_total,cap_skipped_total,ring_lost_total,foreign_total,\
unaccounted,drain_frames";

pub fn run(args: &Args) -> Result<(), String> {
    if args.number("header", 0) != 0 {
        println!("{HEADER}");
        return Ok(());
    }
    let setup = Setup::from_args(args)?;
    let handlers = args.number("handlers", 100);
    let lines = args.number("lines", 1);
    let quiet = args.number("quiet", 0);
    let pace_hz = args.number("pace", 60);
    let frames = args.number("frames", 300);
    let warmup = args.number("warmup", 60);

    let mut rig = Rig::boot(&setup)?;
    rig.attach(&setup, true)?;
    set_load(handlers, lines, quiet);
    for _ in 0..warmup {
        rig.frame()?;
    }

    CONSOLE_BATCH_SIZES.lock().unwrap_or_else(PoisonError::into_inner).clear();
    let before = snapshot();
    let cpu_before = cpu_secs();
    let started = Instant::now();
    let mut frame_micros = Vec::with_capacity(frames as usize);
    let mut inbox_peak = 0;
    let mut buffer_peak = 0;
    for _ in 0..frames {
        let took = rig.paced_frame(pace_hz)?;
        frame_micros.push(u64::try_from(took.as_micros()).unwrap_or(u64::MAX));
        inbox_peak = inbox_peak.max(stats::inbox_depth());
        buffer_peak = buffer_peak.max(stats::buffer_occupancy());
    }
    let wall = started.elapsed().as_secs_f64();
    let cpu = cpu_secs() - cpu_before;
    let after = snapshot();
    let rss = resident_mebibytes();
    let mut batch_sizes: Vec<u64> = CONSOLE_BATCH_SIZES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .map(|&size| u64::from(size))
        .collect();

    let drain_frames = rig.drain(3000)?;
    let end = snapshot();
    let (slice_nanos, _tick_nanos) = rig.gatherer_cost();

    frame_micros.sort_unstable();
    batch_sizes.sort_unstable();
    let gather_ticks = after.gather_ticks - before.gather_ticks;
    let gather_tick_mean = (after.gather_tick_nanos - before.gather_tick_nanos).checked_div(gather_ticks).unwrap_or(0);
    let console_batches = after.console_batches - before.console_batches;
    let console_per_batch = (after.console_nanos - before.console_nanos).checked_div(console_batches).unwrap_or(0);
    let unaccounted = end.produced as i64 - end.console_lines as i64 - end.console_skipped as i64;

    println!(
        "steady,{},{},{},{},{handlers},{lines},{quiet},{pace_hz},{frames},{},{},{},{},{wall:.4},{cpu:.3},{},{},{gather_ticks},\
         {gather_tick_mean},{},{slice_nanos},{console_batches},{},{},{},{console_per_batch},{},{inbox_peak},{buffer_peak},\
         {rss:.1},{},{},{},{},{},{},{},{unaccounted},{drain_frames}",
        setup.mode,
        setup.logpath,
        setup.workers,
        setup.actors,
        quantile(&frame_micros, 0.5),
        quantile(&frame_micros, 0.9),
        quantile(&frame_micros, 0.99),
        frame_micros.last().copied().unwrap_or(0),
        after.produced - before.produced,
        after.slices - before.slices,
        stats::get(&GATHER_TICK_MAX_NANOS),
        after.console_lines - before.console_lines,
        quantile(&batch_sizes, 0.5),
        batch_sizes.last().copied().unwrap_or(0),
        stats::get(&CONSOLE_MAX_NANOS),
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
