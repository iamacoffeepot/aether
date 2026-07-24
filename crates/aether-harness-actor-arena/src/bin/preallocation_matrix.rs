use std::{
    env,
    fmt::Write as _,
    fs,
    num::NonZero,
    path::{Path, PathBuf},
    process::Command,
    thread::available_parallelism,
    time::{SystemTime, UNIX_EPOCH},
};

use aether_harness_actor_arena::{
    HolePattern, PreallocationConfig, PreallocationReport, PreallocationTarget, SweepMode,
};
use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};

/// Run the preallocation sensitivity campaigns in interleaved fresh processes.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    #[arg(long)]
    artifact_dir: PathBuf,

    #[arg(long, value_enum, default_value = "all")]
    campaign: Campaign,

    #[arg(long, default_value_t = 7)]
    samples: usize,

    #[arg(long, default_value_t = 65_536)]
    actors: usize,

    #[arg(long, default_value_t = 80)]
    sweeps: usize,

    #[arg(long, default_value_t = 8)]
    warmup_sweeps: usize,

    #[arg(long, default_value_t = 64)]
    page_slots: usize,

    #[arg(long, default_value_t = 64)]
    state_bytes: usize,

    #[arg(long, default_value_t = 4_096)]
    burst_actors: usize,

    #[arg(long, default_value_t = 0x5eed_5eed_cafe_f00d)]
    seed: u64,

    #[arg(long)]
    touch_reserved: bool,

    #[arg(long)]
    instrument_allocations: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Campaign {
    All,
    Forecast,
    Chunks,
    Sparse,
    Boundary,
    Wasm,
    Diagnostic,
}

#[derive(Debug)]
struct Cell {
    name: String,
    campaign: &'static str,
    config: PreallocationConfig,
}

#[derive(Debug, Serialize, Deserialize)]
struct Environment {
    generated_unix_seconds: u64,
    git_commit: String,
    git_branch: String,
    rustc: String,
    operating_system: String,
    cpu: String,
    logical_cpus: usize,
    trial_executable: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct CellStatistics {
    median_preallocation_nanos: f64,
    preallocation_iqr_nanos: f64,
    median_spawn_nanos_per_actor: f64,
    spawn_iqr_nanos_per_actor: f64,
    median_cold_nanos_per_actor: f64,
    cold_iqr_nanos_per_actor: f64,
    median_incremental_growth_p95_nanos: f64,
    median_incremental_growth_p99_nanos: f64,
    median_maximum_incremental_growth_nanos: f64,
    median_nanos_per_update: f64,
    update_iqr_nanos: f64,
    median_cold_peak_rss_bytes: f64,
    median_peak_rss_bytes: f64,
    median_allocation_calls: Option<f64>,
    median_allocated_bytes: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CellReport {
    name: String,
    campaign: String,
    config: PreallocationConfig,
    samples: usize,
    statistics: CellStatistics,
    checksum: String,
    completed_updates: u64,
    preallocated_chunks: usize,
    incremental_chunks: usize,
    wasm_memory_grow_calls: u64,
    wasm_pages_grown: u64,
    reserved_actor_capacity: usize,
    allocated_arena_pages: usize,
    live_actors: usize,
    live_arena_pages: usize,
    visited_arena_pages: u64,
    reserved_state_bytes: u64,
    live_state_bytes: u64,
    unused_state_bytes: u64,
    guest_linear_memory_bytes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct MatrixReport {
    schema: u32,
    rounds: usize,
    order: String,
    cells: Vec<CellReport>,
    environment: Environment,
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.samples < 3 {
        bail!("samples must be at least 3");
    }

    let cells = cells(&args)?;
    if cells.is_empty() {
        bail!("campaign produced no cells");
    }
    for cell in &cells {
        cell.config.validate().with_context(|| format!("validate matrix cell {}", cell.name))?;
    }

    let trial_executable = sibling_trial_executable()?;
    fs::create_dir_all(args.artifact_dir.join("raw"))
        .with_context(|| format!("create artifact directory {}", args.artifact_dir.display()))?;
    let mut samples: Vec<Vec<PreallocationReport>> =
        (0..cells.len()).map(|_| Vec::with_capacity(args.samples)).collect();

    for round in 0..args.samples {
        let mut order: Vec<_> = (0..cells.len()).collect();
        let rotation = round * cells.len().div_ceil(args.samples) % cells.len();
        order.rotate_left(rotation);
        if !round.is_multiple_of(2) {
            order.reverse();
        }

        for cell_index in order {
            let cell = &cells[cell_index];
            let report = run_trial_process(&trial_executable, &cell.config)
                .with_context(|| format!("run {} sample {round}", cell.name))?;
            ensure!(report.config == cell.config, "{} returned a different configuration", cell.name);
            let directory = args.artifact_dir.join("raw").join(&cell.name);
            fs::create_dir_all(&directory).with_context(|| format!("create {}", directory.display()))?;
            write_json(&directory.join(format!("sample-{round:02}.json")), &report)?;
            samples[cell_index].push(report);
        }
    }

    let cell_reports: Vec<_> =
        cells.iter().zip(samples.iter()).map(|(cell, samples)| aggregate(cell, samples)).collect::<Result<_>>()?;
    verify_equivalent_work(&cell_reports)?;
    let environment = environment(&trial_executable);
    let report = MatrixReport {
        schema: 1,
        rounds: args.samples,
        order: "evenly distributed rotating forward/reverse cell order; one fresh process per cell and round"
            .to_owned(),
        cells: cell_reports,
        environment,
    };

    write_json(&args.artifact_dir.join("matrix.json"), &report)?;
    write_json(&args.artifact_dir.join("environment.json"), &report.environment)?;
    fs::write(args.artifact_dir.join("report.md"), markdown_report(&report))
        .context("write preallocation Markdown report")?;
    fs::write(args.artifact_dir.join("matrix.csv"), matrix_csv(&report)).context("write preallocation CSV")?;
    fs::write(
        args.artifact_dir.join("hot-update.svg"),
        metric_svg(&report, "Hot bullet update by capacity-study cell (ns/update)", |cell| {
            cell.statistics.median_nanos_per_update
        }),
    )
    .context("write hot-update plot")?;
    fs::write(
        args.artifact_dir.join("growth-pause.svg"),
        metric_svg(&report, "Maximum incremental chunk-growth pause by cell (microseconds)", |cell| {
            cell.statistics.median_maximum_incremental_growth_nanos / 1_000.0
        }),
    )
    .context("write growth-pause plot")?;
    fs::write(args.artifact_dir.join("reproduce.txt"), reproduction_command(&args))
        .context("write reproduction command")?;

    Ok(())
}

fn cells(args: &Args) -> Result<Vec<Cell>> {
    let mut cells = Vec::new();
    if matches!(args.campaign, Campaign::All | Campaign::Forecast) {
        add_forecast_cells(&mut cells, args);
    }
    if matches!(args.campaign, Campaign::All | Campaign::Chunks) {
        add_chunk_cells(&mut cells, args);
    }
    if matches!(args.campaign, Campaign::All | Campaign::Sparse) {
        add_sparse_cells(&mut cells, args);
    }
    if matches!(args.campaign, Campaign::All | Campaign::Boundary) {
        add_boundary_cells(&mut cells, args)?;
    }
    if matches!(args.campaign, Campaign::All | Campaign::Wasm) {
        add_wasm_cells(&mut cells, args);
    }
    if args.campaign == Campaign::Diagnostic {
        add_diagnostic_cells(&mut cells, args);
    }
    Ok(cells)
}

fn add_forecast_cells(cells: &mut Vec<Cell>, args: &Args) {
    for percent in [50, 75, 100, 125, 200, 400] {
        cells.push(Cell {
            name: format!("native-forecast-hint-{percent:03}"),
            campaign: "native forecast error",
            config: base_config(args, PreallocationTarget::Native, percent_hint(args.actors, percent), 16),
        });
    }
}

fn add_chunk_cells(cells: &mut Vec<Cell>, args: &Args) {
    for percent in [75, 100, 125] {
        for growth_pages in [1, 4, 16, 64] {
            cells.push(Cell {
                name: format!("native-chunk-hint-{percent:03}-pages-{growth_pages:02}"),
                campaign: "native growth chunk",
                config: base_config(
                    args,
                    PreallocationTarget::Native,
                    percent_hint(args.actors, percent),
                    growth_pages,
                ),
            });
        }
    }
}

fn add_sparse_cells(cells: &mut Vec<Cell>, args: &Args) {
    for live_percent in [25, 50, 75, 90, 100] {
        let patterns: &[_] = if live_percent == 100 {
            &[HolePattern::Packed]
        } else {
            &[HolePattern::Packed, HolePattern::Random]
        };
        for pattern in patterns {
            for sweep_mode in [SweepMode::LiveBitmap, SweepMode::CapacityScan] {
                let mut config = base_config(args, PreallocationTarget::Native, percent_hint(args.actors, 200), 16);
                config.live_percent = live_percent;
                config.hole_pattern = *pattern;
                config.sweep_mode = sweep_mode;
                cells.push(Cell {
                    name: format!(
                        "native-sparse-live-{live_percent:03}-{}-{}",
                        hole_name(*pattern),
                        sweep_name(sweep_mode)
                    ),
                    campaign: "native sparse occupancy",
                    config,
                });
            }
        }
    }
}

fn add_boundary_cells(cells: &mut Vec<Cell>, args: &Args) -> Result<()> {
    let growth_pages = 16;
    let chunk_slots = growth_pages * args.page_slots;
    let boundary = (args.actors / chunk_slots).max(1) * chunk_slots;
    ensure!(boundary > 1, "boundary campaign requires at least two actors");

    for (suffix, actors) in [("minus-one", boundary - 1), ("exact", boundary), ("plus-one", boundary + 1)] {
        let mut config = base_config(args, PreallocationTarget::Native, boundary, growth_pages);
        config.actors = actors;
        config.sweeps = 1;
        config.burst_actors = chunk_slots + 1;
        cells.push(Cell { name: format!("native-boundary-{suffix}"), campaign: "native chunk boundary", config });
    }
    Ok(())
}

fn add_wasm_cells(cells: &mut Vec<Cell>, args: &Args) {
    for percent in [50, 75, 100, 125, 200, 400] {
        cells.push(Cell {
            name: format!("wasm-forecast-hint-{percent:03}"),
            campaign: "Wasm forecast error",
            config: base_config(args, PreallocationTarget::Wasm, percent_hint(args.actors, percent), 16),
        });
    }
    for growth_pages in [1, 4, 16, 64] {
        cells.push(Cell {
            name: format!("wasm-chunk-hint-075-pages-{growth_pages:02}"),
            campaign: "Wasm growth chunk",
            config: base_config(args, PreallocationTarget::Wasm, percent_hint(args.actors, 75), growth_pages),
        });
    }
}

fn add_diagnostic_cells(cells: &mut Vec<Cell>, args: &Args) {
    let campaign = if args.touch_reserved {
        "forced page-touch diagnostic"
    } else if args.instrument_allocations {
        "allocation diagnostic"
    } else {
        "diagnostic subset"
    };
    for percent in [50, 100, 200] {
        cells.push(Cell {
            name: format!("diagnostic-native-hint-{percent:03}"),
            campaign,
            config: base_config(args, PreallocationTarget::Native, percent_hint(args.actors, percent), 16),
        });
    }
    for growth_pages in [1, 16, 64] {
        cells.push(Cell {
            name: format!("diagnostic-native-pages-{growth_pages:02}"),
            campaign,
            config: base_config(args, PreallocationTarget::Native, percent_hint(args.actors, 75), growth_pages),
        });
    }
    for percent in [50, 100, 200] {
        cells.push(Cell {
            name: format!("diagnostic-wasm-hint-{percent:03}"),
            campaign,
            config: base_config(args, PreallocationTarget::Wasm, percent_hint(args.actors, percent), 16),
        });
    }
}

fn base_config(
    args: &Args,
    target: PreallocationTarget,
    capacity_hint: usize,
    growth_pages: usize,
) -> PreallocationConfig {
    PreallocationConfig {
        target,
        actors: args.actors,
        capacity_hint,
        growth_pages,
        page_slots: args.page_slots,
        state_bytes: args.state_bytes,
        live_percent: 100,
        hole_pattern: HolePattern::Packed,
        sweep_mode: SweepMode::LiveBitmap,
        warmup_sweeps: args.warmup_sweeps,
        sweeps: args.sweeps,
        burst_actors: args.burst_actors.min(args.actors).max(1),
        seed: args.seed,
        touch_reserved: args.touch_reserved,
        instrument_allocations: args.instrument_allocations,
    }
}

fn percent_hint(actors: usize, percent: usize) -> usize {
    actors.checked_mul(percent).expect("matrix capacity hint does not overflow").div_ceil(100)
}

fn run_trial_process(executable: &Path, config: &PreallocationConfig) -> Result<PreallocationReport> {
    let mut command = Command::new(executable);
    command
        .arg("--target")
        .arg(target_name(config.target))
        .arg("--actors")
        .arg(config.actors.to_string())
        .arg("--capacity-hint")
        .arg(config.capacity_hint.to_string())
        .arg("--growth-pages")
        .arg(config.growth_pages.to_string())
        .arg("--page-slots")
        .arg(config.page_slots.to_string())
        .arg("--state-bytes")
        .arg(config.state_bytes.to_string())
        .arg("--live-percent")
        .arg(config.live_percent.to_string())
        .arg("--hole-pattern")
        .arg(hole_name(config.hole_pattern))
        .arg("--sweep-mode")
        .arg(sweep_name(config.sweep_mode))
        .arg("--warmup-sweeps")
        .arg(config.warmup_sweeps.to_string())
        .arg("--sweeps")
        .arg(config.sweeps.to_string())
        .arg("--burst-actors")
        .arg(config.burst_actors.to_string())
        .arg("--seed")
        .arg(config.seed.to_string());
    if config.touch_reserved {
        command.arg("--touch-reserved");
    }
    if config.instrument_allocations {
        command.arg("--instrument-allocations");
    }

    let output = command.output().with_context(|| format!("launch {}", executable.display()))?;
    if !output.status.success() {
        bail!("{} failed with {}:\n{}", executable.display(), output.status, String::from_utf8_lossy(&output.stderr));
    }

    serde_json::from_slice(&output.stdout).context("decode preallocation trial JSON")
}

#[allow(
    clippy::cast_precision_loss,
    reason = "bounded measurement totals intentionally become descriptive floating-point statistics"
)]
fn aggregate(cell: &Cell, samples: &[PreallocationReport]) -> Result<CellReport> {
    let first = samples.first().context("matrix cell has no samples")?;
    for sample in samples {
        ensure!(sample.checksum == first.checksum, "{} checksum changed between samples", cell.name);
        ensure!(
            sample.completed_updates == first.completed_updates,
            "{} completion count changed between samples",
            cell.name
        );
        ensure!(
            sample.reserved_actor_capacity == first.reserved_actor_capacity,
            "{} capacity changed between samples",
            cell.name
        );
    }

    let preallocation = values(samples, |sample| sample.preallocation_nanos as f64);
    let spawn = values(samples, |sample| sample.spawn_nanos_per_actor);
    let cold = values(samples, |sample| {
        (sample.preallocation_nanos + sample.spawn_nanos) as f64 / sample.config.actors as f64
    });
    let growth = values(samples, |sample| sample.maximum_incremental_growth_nanos as f64);
    let growth_p95 = values(samples, |sample| sample.incremental_growth_p95_nanos as f64);
    let growth_p99 = values(samples, |sample| sample.incremental_growth_p99_nanos as f64);
    let update = values(samples, |sample| sample.nanos_per_update);
    let cold_rss = values(samples, |sample| sample.cold_peak_rss_bytes as f64);
    let peak_rss = values(samples, |sample| sample.peak_rss_bytes as f64);
    let allocation_calls =
        optional_values(samples, |sample| sample.cold_allocations.map(|snapshot| snapshot.allocation_calls as f64));
    let allocated_bytes =
        optional_values(samples, |sample| sample.cold_allocations.map(|snapshot| snapshot.allocated_bytes as f64));

    Ok(CellReport {
        name: cell.name.clone(),
        campaign: cell.campaign.to_owned(),
        config: cell.config.clone(),
        samples: samples.len(),
        statistics: CellStatistics {
            median_preallocation_nanos: percentile(&preallocation, 0.5),
            preallocation_iqr_nanos: iqr(&preallocation),
            median_spawn_nanos_per_actor: percentile(&spawn, 0.5),
            spawn_iqr_nanos_per_actor: iqr(&spawn),
            median_cold_nanos_per_actor: percentile(&cold, 0.5),
            cold_iqr_nanos_per_actor: iqr(&cold),
            median_incremental_growth_p95_nanos: percentile(&growth_p95, 0.5),
            median_incremental_growth_p99_nanos: percentile(&growth_p99, 0.5),
            median_maximum_incremental_growth_nanos: percentile(&growth, 0.5),
            median_nanos_per_update: percentile(&update, 0.5),
            update_iqr_nanos: iqr(&update),
            median_cold_peak_rss_bytes: percentile(&cold_rss, 0.5),
            median_peak_rss_bytes: percentile(&peak_rss, 0.5),
            median_allocation_calls: allocation_calls.as_ref().map(|values| percentile(values, 0.5)),
            median_allocated_bytes: allocated_bytes.as_ref().map(|values| percentile(values, 0.5)),
        },
        checksum: first.checksum.clone(),
        completed_updates: first.completed_updates,
        preallocated_chunks: first.preallocated_chunks,
        incremental_chunks: first.incremental_chunks,
        wasm_memory_grow_calls: first.wasm_memory_grow_calls,
        wasm_pages_grown: first.wasm_pages_grown,
        reserved_actor_capacity: first.reserved_actor_capacity,
        allocated_arena_pages: first.allocated_arena_pages,
        live_actors: first.live_actors,
        live_arena_pages: first.live_arena_pages,
        visited_arena_pages: first.visited_arena_pages,
        reserved_state_bytes: first.reserved_state_bytes,
        live_state_bytes: first.live_state_bytes,
        unused_state_bytes: first.unused_state_bytes,
        guest_linear_memory_bytes: first.guest_linear_memory_bytes,
    })
}

fn verify_equivalent_work(cells: &[CellReport]) -> Result<()> {
    for (index, cell) in cells.iter().enumerate() {
        for other in &cells[index + 1..] {
            let same_work = cell.config.target == other.config.target
                && cell.config.actors == other.config.actors
                && cell.config.state_bytes == other.config.state_bytes
                && cell.config.live_percent == other.config.live_percent
                && cell.config.hole_pattern == other.config.hole_pattern
                && cell.config.sweeps == other.config.sweeps
                && cell.config.seed == other.config.seed;
            if same_work {
                ensure!(
                    cell.completed_updates == other.completed_updates && cell.checksum == other.checksum,
                    "{} and {} did not complete equivalent work",
                    cell.name,
                    other.name
                );
            }
        }
    }
    Ok(())
}

fn values(samples: &[PreallocationReport], get: impl Fn(&PreallocationReport) -> f64) -> Vec<f64> {
    samples.iter().map(get).collect()
}

fn optional_values(
    samples: &[PreallocationReport],
    get: impl Fn(&PreallocationReport) -> Option<f64>,
) -> Option<Vec<f64>> {
    samples.iter().map(get).collect()
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::suboptimal_flops,
    reason = "quantiles interpolate non-negative indexes within a small sample vector"
)]
fn percentile(values: &[f64], quantile: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let position = quantile * (sorted.len() - 1) as f64;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    let fraction = position - lower as f64;
    sorted[lower] * (1.0 - fraction) + sorted[upper] * fraction
}

fn iqr(values: &[f64]) -> f64 {
    percentile(values, 0.75) - percentile(values, 0.25)
}

#[allow(clippy::cast_precision_loss, reason = "bounded counts intentionally become human-readable percentages and MiB")]
fn markdown_report(report: &MatrixReport) -> String {
    let first = &report.cells[0].config;
    let mut markdown = format!(
        "# Actor arena preallocation matrix\n\n\
         Each cell has {} fresh-process samples. Cell order rotates by an evenly distributed stride and alternates \
         forward/reverse between rounds. Cold rates include capacity reservation plus actor state initialization. \
        Hot rates follow a warm/reset phase and contain bullet updates only.\n\n\
        Forced reserved-page touching: **{}**. Allocation instrumentation: **{}**.\n\n",
        report.rounds,
        if first.touch_reserved {
            "enabled"
        } else {
            "disabled"
        },
        if first.instrument_allocations {
            "enabled"
        } else {
            "disabled"
        },
    );
    let mut campaign = "";

    for (index, cell) in report.cells.iter().enumerate() {
        if cell.campaign != campaign {
            campaign = &cell.campaign;
            let _ = write!(
                markdown,
                "## {campaign}\n\n\
                 | # | Cell | Target | Hint | Growth pages | Live | Sweep | Cold ns/actor | Cold IQR | Spawn ns/actor | \
                 Growth p99 µs | Max growth µs | Hot ns/update | Hot IQR | Capacity | Unused MiB | Peak RSS MiB |\n\
                 |---:|---|---|---:|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n"
            );
        }
        let hint_percent = cell.config.capacity_hint as f64 / cell.config.actors as f64 * 100.0;
        let stats = &cell.statistics;
        let _ = writeln!(
            markdown,
            "| {index} | `{}` | {} | {hint_percent:.1}% | {} | {}% {} | {} | {:.2} | {:.2} | {:.2} | {:.2} | \
             {:.2} | {:.3} | {:.3} | {} | {:.2} | {:.2} |",
            cell.name,
            target_name(cell.config.target),
            cell.config.growth_pages,
            cell.config.live_percent,
            hole_name(cell.config.hole_pattern),
            sweep_name(cell.config.sweep_mode),
            stats.median_cold_nanos_per_actor,
            stats.cold_iqr_nanos_per_actor,
            stats.median_spawn_nanos_per_actor,
            stats.median_incremental_growth_p99_nanos / 1_000.0,
            stats.median_maximum_incremental_growth_nanos / 1_000.0,
            stats.median_nanos_per_update,
            stats.update_iqr_nanos,
            cell.reserved_actor_capacity,
            cell.unused_state_bytes as f64 / 1_048_576.0,
            stats.median_peak_rss_bytes / 1_048_576.0,
        );
    }

    markdown.push_str(
        "\n## Interpretation limits\n\n\
         - Reserve time and spawn time are separate. Native reserve allocates stable chunks; Wasm reserve performs \
           one host `memory.grow` to the estimated size after module/store construction.\n",
    );
    if first.touch_reserved {
        markdown.push_str(
            "- Reserved state is forcibly touched once per 4 KiB host page before actor initialization. This \
             diagnoses physical commitment and intentionally perturbs cold timing.\n",
        );
    } else {
        markdown.push_str(
            "- Spare state is not explicitly touched. It can remain lazily physically backed, so logical \
             reserved/live byte counts carry more meaning than small RSS differences.\n",
        );
    }
    if first.instrument_allocations {
        markdown.push_str(
            "- Global allocation counting is enabled only around reserve and spawn. Its atomics intentionally make \
             this a diagnostic rather than a primary timing pass.\n",
        );
    } else {
        markdown.push_str("- Global allocation counting is disabled so its atomics cannot perturb primary timing.\n");
    }
    markdown.push_str(
        "- `live-bitmap` traverses a two-level live-page hierarchy and live slot words. `capacity-scan` deliberately \
         models the failure mode that visits every reserved page.\n\
         - Wasm hot cells execute a real guest sweep over packed live state. Sparse Wasm state is excluded because \
         it would require choosing a production live-set ABI that this capacity test is not intended to decide.\n\
         - Fresh processes prevent allocator and Wasmtime state from leaking between cells. Rotated order reduces \
         thermal and frequency bias; medians and IQRs remain descriptive rather than inferential statistics.\n\
         - Checksums and exact update counts are verified within every cell and across cells that declare equivalent \
         logical work.\n",
    );
    markdown
}

fn matrix_csv(report: &MatrixReport) -> String {
    let mut csv = String::from(
        "index,campaign,cell,target,actors,capacity_hint,growth_pages,live_percent,hole_pattern,sweep_mode,\
         touch_reserved,instrument_allocations,cold_ns_per_actor,cold_iqr_ns,spawn_ns_per_actor,growth_p95_ns,\
         growth_p99_ns,max_growth_ns,hot_ns_per_update,hot_iqr_ns,reserved_capacity,unused_state_bytes,\
         peak_rss_bytes,allocation_calls,allocated_bytes\n",
    );
    for (index, cell) in report.cells.iter().enumerate() {
        let stats = &cell.statistics;
        let _ = writeln!(
            csv,
            "{index},{},{},{},{},{},{},{},{},{},{},{},{:.6},{:.6},{:.6},{:.3},{:.3},{:.3},{:.6},{:.6},{},{},\
             {:.3},{},{}",
            cell.campaign,
            cell.name,
            target_name(cell.config.target),
            cell.config.actors,
            cell.config.capacity_hint,
            cell.config.growth_pages,
            cell.config.live_percent,
            hole_name(cell.config.hole_pattern),
            sweep_name(cell.config.sweep_mode),
            cell.config.touch_reserved,
            cell.config.instrument_allocations,
            stats.median_cold_nanos_per_actor,
            stats.cold_iqr_nanos_per_actor,
            stats.median_spawn_nanos_per_actor,
            stats.median_incremental_growth_p95_nanos,
            stats.median_incremental_growth_p99_nanos,
            stats.median_maximum_incremental_growth_nanos,
            stats.median_nanos_per_update,
            stats.update_iqr_nanos,
            cell.reserved_actor_capacity,
            cell.unused_state_bytes,
            stats.median_peak_rss_bytes,
            stats.median_allocation_calls.map_or_else(String::new, |value| format!("{value:.0}")),
            stats.median_allocated_bytes.map_or_else(String::new, |value| format!("{value:.0}")),
        );
    }
    csv
}

#[allow(
    clippy::cast_precision_loss,
    clippy::suboptimal_flops,
    reason = "small chart coordinates intentionally become floating-point SVG positions"
)]
fn metric_svg(report: &MatrixReport, title: &str, get: impl Fn(&CellReport) -> f64) -> String {
    let width = 1_200.0;
    let height = 420.0;
    let margin = 60.0;
    let maximum = report.cells.iter().map(&get).fold(0.0, f64::max).max(0.001);
    let bar_width = (width - 2.0 * margin) / report.cells.len() as f64;
    let scale = (height - 2.0 * margin) / maximum;
    let mut bars = String::new();

    for (index, cell) in report.cells.iter().enumerate() {
        let value = get(cell);
        let x = margin + index as f64 * bar_width;
        let bar_height = value * scale;
        let y = height - margin - bar_height;
        let color = if cell.config.target == PreallocationTarget::Native {
            "#2468a2"
        } else {
            "#8b5a2b"
        };
        let _ = write!(
            bars,
            r#"<rect x="{x:.2}" y="{y:.2}" width="{:.2}" height="{bar_height:.2}" fill="{color}"><title>{index}: {} = {value:.3}</title></rect>"#,
            (bar_width - 1.0).max(0.5),
            cell.name,
        );
    }

    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">
<rect width="100%" height="100%" fill="white"/>
<line x1="{margin}" y1="{}" x2="{}" y2="{}" stroke="#555" stroke-width="1"/>
{bars}
<text x="{margin}" y="28" font-family="sans-serif" font-size="16">{title}</text>
<text x="8" y="{}" font-family="sans-serif" font-size="12">{maximum:.3}</text>
<text x="8" y="{}" font-family="sans-serif" font-size="12">0</text>
</svg>
"##,
        height - margin,
        width - margin,
        height - margin,
        margin + 4.0,
        height - margin + 4.0,
    )
}

fn reproduction_command(args: &Args) -> String {
    format!(
        "{} --artifact-dir <output> --campaign {} --samples {} --actors {} --warmup-sweeps {} --sweeps {} \
         --page-slots {} --state-bytes {} --burst-actors {} --seed {}{}{}\n",
        env::current_exe()
            .map_or_else(|_| "aether-actor-arena-preallocation-matrix".into(), |path| path.display().to_string()),
        campaign_name(args.campaign),
        args.samples,
        args.actors,
        args.warmup_sweeps,
        args.sweeps,
        args.page_slots,
        args.state_bytes,
        args.burst_actors,
        args.seed,
        if args.touch_reserved {
            " --touch-reserved"
        } else {
            ""
        },
        if args.instrument_allocations {
            " --instrument-allocations"
        } else {
            ""
        },
    )
}

fn environment(trial_executable: &Path) -> Environment {
    Environment {
        generated_unix_seconds: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |duration| duration.as_secs()),
        git_commit: command_text("git", &["rev-parse", "HEAD"]),
        git_branch: command_text("git", &["branch", "--show-current"]),
        rustc: command_text("rustc", &["-Vv"]),
        operating_system: command_text("uname", &["-a"]),
        cpu: if cfg!(target_os = "macos") {
            command_text("sysctl", &["-n", "machdep.cpu.brand_string"])
        } else {
            command_text("uname", &["-m"])
        },
        logical_cpus: available_parallelism().map_or(0, NonZero::get),
        trial_executable: trial_executable.display().to_string(),
    }
}

fn command_text(program: &str, arguments: &[&str]) -> String {
    Command::new(program)
        .args(arguments)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map_or_else(|| "<unavailable>".to_owned(), |output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn sibling_trial_executable() -> Result<PathBuf> {
    let executable = env::current_exe().context("resolve preallocation matrix executable")?;
    let trial = executable.with_file_name(format!("aether-actor-arena-preallocation-trial{}", env::consts::EXE_SUFFIX));
    if !trial.is_file() {
        bail!("preallocation trial executable not found at {}; build both release binaries first", trial.display());
    }
    Ok(trial)
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    fs::write(path, serde_json::to_vec_pretty(value)?).with_context(|| format!("write {}", path.display()))
}

const fn target_name(target: PreallocationTarget) -> &'static str {
    match target {
        PreallocationTarget::Native => "native",
        PreallocationTarget::Wasm => "wasm",
    }
}

const fn hole_name(pattern: HolePattern) -> &'static str {
    match pattern {
        HolePattern::Packed => "packed",
        HolePattern::Random => "random",
    }
}

const fn sweep_name(mode: SweepMode) -> &'static str {
    match mode {
        SweepMode::LiveBitmap => "live-bitmap",
        SweepMode::CapacityScan => "capacity-scan",
    }
}

const fn campaign_name(campaign: Campaign) -> &'static str {
    match campaign {
        Campaign::All => "all",
        Campaign::Forecast => "forecast",
        Campaign::Chunks => "chunks",
        Campaign::Sparse => "sparse",
        Campaign::Boundary => "boundary",
        Campaign::Wasm => "wasm",
        Campaign::Diagnostic => "diagnostic",
    }
}
