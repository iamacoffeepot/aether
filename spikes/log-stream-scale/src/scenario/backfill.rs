//! Opening the tap on an engine whose actors hold full rings. Every producer
//! first fills its ring with the tap closed; then the tap opens, with or
//! without backfill, while each producer is dispatched one quiet handler per
//! frame.

use std::time::Instant;

use crate::kinds::{Job, OP_WORK};
use crate::process::resident_mebibytes;
use crate::rig::{Args, Rig, Setup, quantile, set_load};
use crate::stats::{self, GATHER_PENDING, snapshot};

pub const HEADER: &str = "scenario,mode,backfill,workers,actors,ring_lines,baseline_frame_p50_us,first_frame_us,\
second_frame_us,worst_frame_us,frames_over_16ms,peak_inbox,peak_buffer,rss_before_mib,rss_peak_mib,slices,\
lines_in_rings,delivered,skipped,dropped,cap_skipped,ring_lost,frames_until_quiet,secs_until_quiet";

pub fn run(args: &Args) -> Result<(), String> {
    if args.number("header", 0) != 0 {
        println!("{HEADER}");
        return Ok(());
    }
    let setup = Setup::from_args(args)?;
    let ring_lines = u32::try_from(args.number("ring_lines", 1024)).unwrap_or(1024);
    let pace_hz = args.number("pace", 60);
    let frames = args.number("frames", 300);

    let mut rig = Rig::boot(&setup)?;
    rig.attach(&setup, false)?;
    let fill = Job { op: OP_WORK, count: 0, lines: ring_lines, direct: setup.direct };
    for producer in rig.producers.clone() {
        rig.tb.settle_bytes(producer, &fill).map_err(|error| format!("fill: {error:?}"))?;
    }
    let lines_in_rings = stats::produced();

    set_load(0, 1, setup.actors as u64);
    let mut baseline = Vec::new();
    for _ in 0..120 {
        baseline.push(u64::try_from(rig.paced_frame(pace_hz)?.as_micros()).unwrap_or(u64::MAX));
    }
    baseline.sort_unstable();
    let rss_before = resident_mebibytes();

    rig.attach(&setup, true)?;
    let opened = Instant::now();
    let mut frame_micros = Vec::new();
    let mut peak_inbox = 0;
    let mut peak_buffer = 0;
    let mut rss_peak = rss_before;
    let mut quiet_at = None;
    let mut last = (0, 0);
    let mut still = 0;
    for frame in 0..frames {
        frame_micros.push(u64::try_from(rig.paced_frame(pace_hz)?.as_micros()).unwrap_or(u64::MAX));
        peak_inbox = peak_inbox.max(stats::inbox_depth());
        peak_buffer = peak_buffer.max(stats::buffer_occupancy());
        rss_peak = rss_peak.max(resident_mebibytes());
        let totals = (stats::get(&stats::CONSOLE_LINES), stats::get(&stats::CONSOLE_SKIPPED));
        let empty = stats::inbox_depth() == 0 && stats::get(&GATHER_PENDING) == 0 && stats::buffer_occupancy() == 0;
        still = if empty && totals == last {
            still + 1
        } else {
            0
        };
        last = totals;
        if still == 3 && quiet_at.is_none() {
            quiet_at = Some((frame + 1 - 3, opened.elapsed().as_secs_f64()));
        }
    }
    let end = snapshot();
    let over_budget = frame_micros.iter().filter(|&&micros| micros > 16_600).count();
    let worst = frame_micros.iter().copied().max().unwrap_or(0);
    let (quiet_frames, quiet_secs) = quiet_at
        .map_or(("never".to_owned(), "never".to_owned()), |(frames, secs)| (frames.to_string(), format!("{secs:.3}")));
    println!(
        "backfill,{},{},{},{},{ring_lines},{},{},{},{worst},{over_budget},{peak_inbox},{peak_buffer},{rss_before:.0},\
         {rss_peak:.0},{},{lines_in_rings},{},{},{},{},{},{quiet_frames},{quiet_secs}",
        setup.mode,
        u8::from(setup.backfill),
        setup.workers,
        setup.actors,
        quantile(&baseline, 0.5),
        frame_micros.first().copied().unwrap_or(0),
        frame_micros.get(1).copied().unwrap_or(0),
        end.slices,
        end.console_lines,
        end.console_skipped,
        end.dropped,
        end.cap_skipped,
        end.ring_lost,
    );
    Ok(())
}
