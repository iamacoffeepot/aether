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

use aether_harness_actor_arena::{AccessPattern, Backend, TrialReport, Workload};
use anyhow::{Context, Result, bail};
use clap::Parser;
use serde::{Deserialize, Serialize};

/// Compare two actor-storage arms with alternating, fresh-process pairs.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    #[arg(long, value_enum)]
    base: Backend,

    #[arg(long, value_enum)]
    candidate: Backend,

    #[arg(long, value_enum, default_value = "dispatch")]
    workload: Workload,

    #[arg(long, default_value_t = 9)]
    pairs: usize,

    #[arg(long)]
    artifact_dir: PathBuf,

    #[arg(long, default_value_t = 1_024)]
    actors: usize,

    #[arg(long, default_value_t = 1_000_000)]
    mails: u64,

    #[arg(long, default_value_t = 16)]
    mails_per_activation: usize,

    #[arg(long, default_value_t = 64)]
    page_slots: usize,

    #[arg(long, default_value_t = 256)]
    state_bytes: usize,

    #[arg(long, value_enum, default_value = "random")]
    pattern: AccessPattern,

    #[arg(long, default_value_t = 0x5eed_5eed_cafe_f00d)]
    seed: u64,

    #[arg(long, default_value_t = 100_000)]
    warmup_mails: u64,

    #[arg(long)]
    instrument_allocations: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum PairOrder {
    BaseCandidate,
    CandidateBase,
}

#[derive(Debug, Serialize, Deserialize)]
struct PairReport {
    index: usize,
    order: PairOrder,
    base: TrialReport,
    candidate: TrialReport,
    paired_delta_nanos_per_mail: f64,
    speedup: f64,
}

#[derive(Debug, Serialize, Deserialize)]
struct Statistics {
    base_median_nanos_per_mail: f64,
    candidate_median_nanos_per_mail: f64,
    median_paired_delta_nanos_per_mail: f64,
    paired_delta_iqr_nanos_per_mail: f64,
    relative_change_percent: f64,
    median_speedup: f64,
    directional_consistency: f64,
    noise_floor_nanos_per_mail: f64,
    classification: String,
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
struct ComparisonReport {
    schema: u32,
    workload: Workload,
    base: Backend,
    candidate: Backend,
    pairs: Vec<PairReport>,
    statistics: Statistics,
    environment: Environment,
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.pairs < 3 {
        bail!("pairs must be at least 3");
    }
    if args.base == args.candidate {
        bail!("base and candidate must differ");
    }

    let trial_executable = sibling_trial_executable()?;
    fs::create_dir_all(args.artifact_dir.join("raw"))
        .with_context(|| format!("create artifact directory {}", args.artifact_dir.display()))?;

    let mut pairs = Vec::with_capacity(args.pairs);
    for index in 0..args.pairs {
        let order = if index.is_multiple_of(2) {
            PairOrder::BaseCandidate
        } else {
            PairOrder::CandidateBase
        };
        let (base, candidate) = if matches!(order, PairOrder::BaseCandidate) {
            (
                run_trial_process(&trial_executable, &args, args.base)?,
                run_trial_process(&trial_executable, &args, args.candidate)?,
            )
        } else {
            let candidate = run_trial_process(&trial_executable, &args, args.candidate)?;
            let base = run_trial_process(&trial_executable, &args, args.base)?;
            (base, candidate)
        };
        pairs.push(pair(index, order, base, candidate)?);
        write_raw_pair(&args.artifact_dir, pairs.last().expect("just pushed"))?;
    }

    let statistics = statistics(&pairs);
    let environment = environment(&trial_executable);
    let report = ComparisonReport {
        schema: 1,
        workload: args.workload,
        base: args.base,
        candidate: args.candidate,
        pairs,
        statistics,
        environment,
    };

    write_json(&args.artifact_dir.join("comparison.json"), &report)?;
    write_json(&args.artifact_dir.join("environment.json"), &report.environment)?;
    fs::write(args.artifact_dir.join("report.md"), markdown_report(&report, &args))
        .context("write Markdown comparison report")?;
    fs::write(args.artifact_dir.join("paired-deltas.svg"), delta_svg(&report)).context("write paired-delta plot")?;
    fs::write(args.artifact_dir.join("reproduce.txt"), reproduction_command(&args, &trial_executable))
        .context("write reproduction command")?;

    Ok(())
}

fn pair(index: usize, order: PairOrder, base: TrialReport, candidate: TrialReport) -> Result<PairReport> {
    if base.checksum != candidate.checksum {
        bail!("pair {index} checksum mismatch: base {} candidate {}", base.checksum, candidate.checksum);
    }
    if base.completed_mails != candidate.completed_mails {
        bail!(
            "pair {index} completion mismatch: base {} candidate {}",
            base.completed_mails,
            candidate.completed_mails
        );
    }

    Ok(PairReport {
        index,
        order,
        paired_delta_nanos_per_mail: candidate.nanos_per_mail - base.nanos_per_mail,
        speedup: base.nanos_per_mail / candidate.nanos_per_mail,
        base,
        candidate,
    })
}

fn run_trial_process(executable: &Path, args: &Args, backend: Backend) -> Result<TrialReport> {
    let mut command = Command::new(executable);
    command
        .arg("--backend")
        .arg(backend_name(backend))
        .arg("--workload")
        .arg(workload_name(args.workload))
        .arg("--actors")
        .arg(args.actors.to_string())
        .arg("--mails")
        .arg(args.mails.to_string())
        .arg("--mails-per-activation")
        .arg(args.mails_per_activation.to_string())
        .arg("--page-slots")
        .arg(args.page_slots.to_string())
        .arg("--state-bytes")
        .arg(args.state_bytes.to_string())
        .arg("--pattern")
        .arg(pattern_name(args.pattern))
        .arg("--seed")
        .arg(args.seed.to_string())
        .arg("--warmup-mails")
        .arg(args.warmup_mails.to_string());
    if args.instrument_allocations {
        command.arg("--instrument-allocations");
    }

    let output =
        command.output().with_context(|| format!("launch fresh trial process for {}", backend_name(backend)))?;
    if !output.status.success() {
        bail!(
            "{} trial failed with {}:\n{}",
            backend_name(backend),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    serde_json::from_slice(&output.stdout).with_context(|| format!("decode {} trial JSON", backend_name(backend)))
}

#[allow(
    clippy::cast_precision_loss,
    reason = "pair counts are CLI-bounded and converted only for descriptive statistics"
)]
fn statistics(pairs: &[PairReport]) -> Statistics {
    let base: Vec<_> = pairs.iter().map(|pair| pair.base.nanos_per_mail).collect();
    let candidate: Vec<_> = pairs.iter().map(|pair| pair.candidate.nanos_per_mail).collect();
    let deltas: Vec<_> = pairs.iter().map(|pair| pair.paired_delta_nanos_per_mail).collect();
    let speedups: Vec<_> = pairs.iter().map(|pair| pair.speedup).collect();
    let base_median = percentile(&base, 0.5);
    let candidate_median = percentile(&candidate, 0.5);
    let median_delta = percentile(&deltas, 0.5);
    let delta_iqr = percentile(&deltas, 0.75) - percentile(&deltas, 0.25);
    let noise_floor = (delta_iqr * 1.5).max(base_median * 0.10).max(0.3);
    let sign = median_delta.signum();
    let directional =
        deltas.iter().filter(|delta| delta.signum() == sign || **delta == 0.0).count() as f64 / deltas.len() as f64;
    let classification = if median_delta < -noise_floor && directional >= 0.75 {
        "improvement"
    } else if median_delta > noise_floor && directional >= 0.75 {
        "regression"
    } else {
        "inconclusive"
    };

    Statistics {
        base_median_nanos_per_mail: base_median,
        candidate_median_nanos_per_mail: candidate_median,
        median_paired_delta_nanos_per_mail: median_delta,
        paired_delta_iqr_nanos_per_mail: delta_iqr,
        relative_change_percent: (candidate_median / base_median - 1.0) * 100.0,
        median_speedup: percentile(&speedups, 0.5),
        directional_consistency: directional,
        noise_floor_nanos_per_mail: noise_floor,
        classification: classification.to_owned(),
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::suboptimal_flops,
    reason = "quantiles interpolate non-negative indexes within a small in-memory pair vector"
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

fn markdown_report(report: &ComparisonReport, args: &Args) -> String {
    let (stats, first) = (&report.statistics, &report.pairs[0]);
    let unit = work_unit(args.workload);
    let mut markdown = format!(
        "# Actor arena paired comparison\n\n\
         Base: `{}`  \n\
         Candidate: `{}`  \n\
         Classification: **{}**\n\n\
         | Metric | Result |\n\
         |---|---:|\n\
         | Base median | {:.3} ns/{} |\n\
         | Candidate median | {:.3} ns/{} |\n\
         | Median paired delta | {:+.3} ns/{} |\n\
         | Paired delta IQR | {:.3} ns/{} |\n\
         | Relative median change | {:+.2}% |\n\
         | Median speedup | {:.3}× |\n\
         | Directional consistency | {:.1}% |\n\
         | ADR-0085 noise floor | {:.3} ns/{unit} |\n\n\
         Configuration: `{}` workload, {} actors, {} work units, {} bytes/state, {} mails/activation, \
         {} slots/page, `{}` access, seed `{}`.\n\n\
         ## Pairs\n\n\
         | Pair | Order | Base ns/{unit} | Candidate ns/{unit} | Delta | Speedup |\n\
         |---:|---|---:|---:|---:|---:|\n",
        backend_name(report.base),
        backend_name(report.candidate),
        stats.classification,
        stats.base_median_nanos_per_mail,
        unit,
        stats.candidate_median_nanos_per_mail,
        unit,
        stats.median_paired_delta_nanos_per_mail,
        unit,
        stats.paired_delta_iqr_nanos_per_mail,
        unit,
        stats.relative_change_percent,
        stats.median_speedup,
        stats.directional_consistency * 100.0,
        stats.noise_floor_nanos_per_mail,
        workload_name(args.workload),
        args.actors,
        args.mails,
        args.state_bytes,
        args.mails_per_activation,
        args.page_slots,
        pattern_name(args.pattern),
        args.seed,
    );
    for pair in &report.pairs {
        let _ = writeln!(
            markdown,
            "| {} | {:?} | {:.3} | {:.3} | {:+.3} | {:.3}× |",
            pair.index,
            pair.order,
            pair.base.nanos_per_mail,
            pair.candidate.nanos_per_mail,
            pair.paired_delta_nanos_per_mail,
            pair.speedup
        );
    }
    let _ = write!(
        markdown,
        "\n## Mechanism counters\n\n\
         Counters are deterministic and shown from the first pair.\n\n\
         | Counter | Base | Candidate |\n\
         |---|---:|---:|\n\
         | Route lookups | {} | {} |\n\
         | State lock acquisitions | {} | {} |\n\
         | Scheduled items | {} | {} |\n\
         | Host entries | {} | {} |\n\
         | Host-to-guest bytes | {} | {} |\n\
         | Guest-to-host bytes | {} | {} |\n\
         | State round trips | {} | {} |\n\
         | Guest linear memory bytes | {} | {} |\n\
         | Peak RSS bytes | {} | {} |\n\n\
         ## Interpretation limits\n\n\
         - Every side runs in a fresh process. Pair order alternates AB/BA, and both sides use the same precomputed seed and access trace.\n\
         - Compilation, Wasmtime module/store construction, allocation of actor state, warmup, reset, checksum, and JSON encoding are outside the timed interval.\n\
         - Peak RSS intentionally includes runtime setup. Allocation counters are absent unless this was an explicit perturbing allocation pass.\n\
         - Native arms mirror Aether's route, boxed state, mutex, and activation shapes; they do not invoke the complete substrate mail envelope or lifecycle wrappers.\n\
         - Wasm arms execute real Wasmtime code and linear-memory reads/writes, but the fixture is a core-Wasm storage model rather than Aether's full component ABI.\n\
         - Hardware performance counters are not collected by this portable runner. Use platform tooling in a separate diagnostic pass so counter collection cannot perturb the primary result.\n",
        first.base.counters.route_lookups,
        first.candidate.counters.route_lookups,
        first.base.counters.state_lock_acquisitions,
        first.candidate.counters.state_lock_acquisitions,
        first.base.counters.scheduled_items,
        first.candidate.counters.scheduled_items,
        first.base.counters.host_entries,
        first.candidate.counters.host_entries,
        first.base.counters.host_to_guest_bytes,
        first.candidate.counters.host_to_guest_bytes,
        first.base.counters.guest_to_host_bytes,
        first.candidate.counters.guest_to_host_bytes,
        first.base.counters.state_round_trips,
        first.candidate.counters.state_round_trips,
        first.base.counters.guest_linear_memory_bytes,
        first.candidate.counters.guest_linear_memory_bytes,
        first.base.peak_rss_bytes,
        first.candidate.peak_rss_bytes,
    );
    markdown
}

#[allow(
    clippy::cast_precision_loss,
    clippy::suboptimal_flops,
    reason = "small integer plot coordinates intentionally become floating-point SVG coordinates"
)]
fn delta_svg(report: &ComparisonReport) -> String {
    let unit = work_unit(report.workload);
    let width = 900.0;
    let height = 360.0;
    let margin = 55.0;
    let maximum = report
        .pairs
        .iter()
        .map(|pair| pair.paired_delta_nanos_per_mail.abs())
        .fold(report.statistics.noise_floor_nanos_per_mail, f64::max)
        .max(0.001);
    let x_step = (width - 2.0 * margin) / (report.pairs.len() - 1).max(1) as f64;
    let scale = (height / 2.0 - margin) / maximum;
    let center = height / 2.0;
    let mut points = String::new();
    let mut circles = String::new();

    for (index, pair) in report.pairs.iter().enumerate() {
        let x = margin + index as f64 * x_step;
        let y = center - pair.paired_delta_nanos_per_mail * scale;
        let _ = write!(points, "{x:.1},{y:.1} ");
        let _ = write!(circles, r#"<circle cx="{x:.1}" cy="{y:.1}" r="4"/>"#);
    }

    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">
<rect width="100%" height="100%" fill="white"/>
<line x1="{margin}" y1="{center}" x2="{}" y2="{center}" stroke="#555" stroke-width="1"/>
<line x1="{margin}" y1="{}" x2="{}" y2="{}" stroke="#2b6" stroke-dasharray="5 5"/>
<line x1="{margin}" y1="{}" x2="{}" y2="{}" stroke="#c44" stroke-dasharray="5 5"/>
<polyline points="{points}" fill="none" stroke="#2468a2" stroke-width="2"/>
<g fill="#2468a2">{circles}</g>
<text x="{margin}" y="24" font-family="sans-serif" font-size="16">candidate − base paired delta (ns/{unit})</text>
<text x="8" y="{}" font-family="sans-serif" font-size="12">0</text>
<text x="8" y="{}" font-family="sans-serif" font-size="12">faster</text>
<text x="8" y="{}" font-family="sans-serif" font-size="12">slower</text>
</svg>
"##,
        width - margin,
        center + report.statistics.noise_floor_nanos_per_mail * scale,
        width - margin,
        center + report.statistics.noise_floor_nanos_per_mail * scale,
        center - report.statistics.noise_floor_nanos_per_mail * scale,
        width - margin,
        center - report.statistics.noise_floor_nanos_per_mail * scale,
        center + 4.0,
        height - 12.0,
        18.0,
    )
}

const fn work_unit(workload: Workload) -> &'static str {
    match workload {
        Workload::Dispatch => "mail",
        Workload::LifecycleChurn => "lifecycle op",
        Workload::SceneSweep => "entity update",
    }
}

fn reproduction_command(args: &Args, executable: &Path) -> String {
    format!(
        "{} --base {} --candidate {} --workload {} --pairs {} --artifact-dir <output> --actors {} --mails {} \
         --mails-per-activation {} --page-slots {} --state-bytes {} --pattern {} --seed {} \
         --warmup-mails {}{}\n\nTrial executable used: {}\n",
        env::current_exe().map_or_else(|_| "aether-actor-arena-compare".into(), |path| path.display().to_string()),
        backend_name(args.base),
        backend_name(args.candidate),
        workload_name(args.workload),
        args.pairs,
        args.actors,
        args.mails,
        args.mails_per_activation,
        args.page_slots,
        args.state_bytes,
        pattern_name(args.pattern),
        args.seed,
        args.warmup_mails,
        if args.instrument_allocations {
            " --instrument-allocations"
        } else {
            ""
        },
        executable.display(),
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
    let executable = env::current_exe().context("resolve comparison executable")?;
    let trial = executable.with_file_name(format!("aether-actor-arena-trial{}", env::consts::EXE_SUFFIX));
    if !trial.is_file() {
        bail!("trial executable not found at {}; build both release binaries first", trial.display());
    }
    Ok(trial)
}

fn write_raw_pair(directory: &Path, pair: &PairReport) -> Result<()> {
    write_json(&directory.join("raw").join(format!("pair-{:02}.json", pair.index)), pair)
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    fs::write(path, serde_json::to_vec_pretty(value)?).with_context(|| format!("write {}", path.display()))
}

const fn backend_name(backend: Backend) -> &'static str {
    match backend {
        Backend::BoxedCurrent => "boxed-current",
        Backend::ArenaState => "arena-state",
        Backend::ArenaEndpoint => "arena-endpoint",
        Backend::ArenaPage => "arena-page",
        Backend::WasmDetached => "wasm-detached",
        Backend::WasmInline => "wasm-inline",
        Backend::WasmArena => "wasm-arena",
        Backend::WasmBatch => "wasm-batch",
        Backend::WasmCopyRoundtrip => "wasm-copy-roundtrip",
    }
}

const fn pattern_name(pattern: AccessPattern) -> &'static str {
    match pattern {
        AccessPattern::Sequential => "sequential",
        AccessPattern::Random => "random",
        AccessPattern::Clustered => "clustered",
        AccessPattern::HotCold => "hot-cold",
    }
}

const fn workload_name(workload: Workload) -> &'static str {
    match workload {
        Workload::Dispatch => "dispatch",
        Workload::LifecycleChurn => "lifecycle-churn",
        Workload::SceneSweep => "scene-sweep",
    }
}
