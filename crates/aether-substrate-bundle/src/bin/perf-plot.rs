//! `aether-perf-plot` (iamacoffeepot/aether#1155): run one latency sweep
//! and render each cell's `queued` / `drain` / `handler` sample
//! distributions as a single overlaid PNG, so the shape the percentiles
//! hide (drain's spread vs the tight queued / handler) is visible at a
//! glance. Feature-gated (`perf-plot`) so plotters stays off normal
//! builds.
//!
//! Diagnostics go to stderr; PNGs land in `AETHER_PERF_PLOT_DIR`
//! (default `./perf-plots`), one per `(topology × worker-count)` cell.
//! Sweep config matches `perf-trial` (shared env parsers), so the plots
//! describe the same cells the comparison measures.

#![allow(clippy::print_stdout, clippy::print_stderr)]
// Binning + axis math: latency samples and bin counts are small positive
// values, so the f64 <-> integer casts are benign. Matches the same
// allow on `perf/harness.rs` / `perf/report.rs`.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::env;
use std::error::Error;
use std::fs;
use std::io::BufWriter;
use std::path::Path;
use std::process::ExitCode;

use aether_substrate_bundle::perf::harness::{
    CellSamples, SweepConfig, pace_hz_from_env, parse_topologies, parse_workers, run_sweep_samples,
};
use plotters::prelude::*;
use plotters::style::register_font;

const WIDTH: u32 = 960;
const HEIGHT: u32 = 540;
const NBINS: usize = 48;

/// Embedded font (iamacoffeepot/aether#1155). plotters' `ab_glyph`
/// backend ships no font, so axis/legend text needs one registered.
/// Roboto Mono (SIL OFL 1.1 — see `assets/fonts/OFL.txt`); a variable
/// TTF, rendered at its default (Regular) instance. Embedding keeps the
/// render deterministic with zero system-font / CI dependency.
const FONT: &[u8] = include_bytes!("../../assets/fonts/RobotoMono.ttf");

fn main() -> ExitCode {
    let frames: u32 = env::var("AETHER_PERF_FRAMES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let dir = env::var("AETHER_PERF_PLOT_DIR").unwrap_or_else(|_| "perf-plots".to_owned());
    // Register the embedded font under "sans-serif" so every FontDesc the
    // chart builds (caption, mesh labels, legend) resolves to it.
    if register_font("sans-serif", FontStyle::Normal, FONT).is_err() {
        eprintln!("perf-plot: failed to parse the embedded font");
        return ExitCode::from(1);
    }
    let cfg = SweepConfig {
        workers: parse_workers(),
        topologies: parse_topologies(),
        frames,
        pace_hz: pace_hz_from_env(),
    };

    let cells = run_sweep_samples(&cfg);
    if cells.is_empty() {
        eprintln!("perf-plot: no cells measured (no wgpu adapter, or every cell boot failed)");
        return ExitCode::from(2);
    }
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("perf-plot: create dir {dir}: {e}");
        return ExitCode::from(1);
    }

    let mut rendered = 0usize;
    for c in &cells {
        let path = format!("{dir}/{}-{}w.png", c.topo, c.workers);
        match render_cell(Path::new(&path), c) {
            Ok(()) => rendered += 1,
            Err(e) => eprintln!("perf-plot: render {path} failed: {e}"),
        }
    }
    eprintln!(
        "perf-plot: wrote {rendered}/{} plot(s) to {dir}/",
        cells.len()
    );
    if rendered == 0 {
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

/// p50 of a sample slice, in microseconds, for the legend label.
fn p50_us(samples: &[u64]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut s = samples.to_vec();
    s.sort_unstable();
    s[s.len() / 2] as f64 / 1000.0
}

/// Render one cell's three spans as overlaid log-x histograms (outline /
/// step lines so three series read cleanly), with a legend carrying each
/// span's p50.
fn render_cell(path: &Path, c: &CellSamples) -> Result<(), Box<dyn Error>> {
    let spans: [(&str, &[u64], RGBColor); 3] = [
        ("queued", &c.queued, BLUE),
        ("drain", &c.drain, RED),
        ("handler", &c.handler, GREEN),
    ];

    // Combined positive range across the spans, in microseconds. A span
    // sample can be 0ns (sub-resolution); clamp to 1ns so the log axis
    // stays defined.
    let positive = || {
        spans
            .iter()
            .flat_map(|(_, s, _)| s.iter())
            .copied()
            .filter(|&v| v > 0)
    };
    let lo_ns = positive().min().unwrap_or(1);
    let hi_ns = positive().max().unwrap_or(1).max(lo_ns + 1);
    let xmin = (lo_ns as f64 / 1000.0).max(0.001);
    let xmax = (hi_ns as f64 / 1000.0).max(xmin * 1.001);

    let (lmin, lmax) = (xmin.ln(), xmax.ln());
    let bin_of = |us: f64| -> usize {
        let frac = (us.max(xmin).min(xmax).ln() - lmin) / (lmax - lmin);
        ((frac * NBINS as f64).floor() as usize).min(NBINS - 1)
    };
    // Geometric bin centres (µs) — the x of each step point.
    let centres: Vec<f64> = (0..NBINS)
        .map(|i| {
            ((i as f64 + 0.5) / NBINS as f64)
                .mul_add(lmax - lmin, lmin)
                .exp()
        })
        .collect();

    let mut series: Vec<(&str, RGBColor, Vec<u32>, f64)> = Vec::new();
    let mut ymax = 1u32;
    for (label, samples, color) in spans {
        if samples.is_empty() {
            continue;
        }
        let mut counts = vec![0u32; NBINS];
        for &v in samples {
            counts[bin_of(v.max(1) as f64 / 1000.0)] += 1;
        }
        ymax = ymax.max(counts.iter().copied().max().unwrap_or(1));
        series.push((label, color, counts, p50_us(samples)));
    }
    let ymax = (f64::from(ymax) * 1.1).ceil() as u32;

    let mut buf = vec![0u8; (WIDTH * HEIGHT * 3) as usize];
    {
        let root = BitMapBackend::with_buffer(&mut buf, (WIDTH, HEIGHT)).into_drawing_area();
        root.fill(&WHITE)?;
        let mut chart = ChartBuilder::on(&root)
            .caption(
                format!("{} @ {}w — per-mail span distribution", c.topo, c.workers),
                ("sans-serif", 22),
            )
            .margin(14)
            .x_label_area_size(44)
            .y_label_area_size(52)
            .build_cartesian_2d((xmin..xmax).log_scale(), 0u32..ymax)?;
        chart
            .configure_mesh()
            .x_desc("latency (µs, log)")
            .y_desc("samples")
            .draw()?;
        for (label, color, counts, p50) in &series {
            let color = *color;
            chart
                .draw_series(LineSeries::new(
                    centres.iter().zip(counts).map(|(&x, &n)| (x, n)),
                    color.stroke_width(2),
                ))?
                .label(format!(
                    "{label}  (p50 {p50:.2}µs, n={})",
                    counts.iter().sum::<u32>()
                ))
                .legend(move |(x, y)| {
                    PathElement::new(vec![(x, y), (x + 18, y)], color.stroke_width(2))
                });
        }
        chart
            .configure_series_labels()
            .border_style(BLACK)
            .background_style(WHITE.mix(0.85))
            .draw()?;
        root.present()?;
    }

    let file = fs::File::create(path)?;
    let mut enc = png::Encoder::new(BufWriter::new(file), WIDTH, HEIGHT);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&buf)?;
    Ok(())
}
