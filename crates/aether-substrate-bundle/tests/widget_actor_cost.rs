//! issue 1793: widget-actor per-frame cost spike.
//!
//! Measures the actual per-frame execution cost of an actor-backed widget
//! — a wasm component that subscribes to the frame lifecycle and re-emits
//! its draw across the boundary each tick — against the stable-frame floor
//! a host-cached-replay widget would pay, so the UI design's
//! immediate-vs-actor tier line is drawn on data instead of intuition.
//!
//! It loads the `ui_widget` fixture N times in each of two profiles
//! (`naive` = re-emit every tick, `cached` = early-return on an unchanged
//! frame), advances the platform, and reads each widget's per-handler
//! `Tick` cost from the EWMA table (ADR-0036) via `cost_table()`. The
//! per-widget mean is one widget's per-frame cost; multiplied by the widget
//! count it is the aggregate the 60fps frame budget has to absorb.
//!
//! Two measurements live here: `widget_actor_per_frame_cost` sweeps the
//! widget count in both profiles, and `widget_cost_vs_draw_weight` sweeps the
//! draw weight (quads per batch) at a fixed count and fits a line to split the
//! fixed per-frame floor (intercept — boundary + dispatch, not reducible by
//! optimizing the draw) from the marginal per-draw-item cost (slope — guest
//! batch build + host mail encode, where wasm JIT/AOT tuning and real widget
//! complexity move the number).
//!
//! This is an on-demand MEASUREMENT, not a CI gate: it is `#[ignore]`d
//! (zero CI cost, same as `lifecycle_latency_observe`) and prints a table
//! on stderr. Run it with:
//!
//! ```text
//! cargo test -p aether-substrate-bundle --release --test widget_actor_cost \
//!     -- --ignored --nocapture
//! ```
//!
//! Release matters: a debug guest inflates the boundary + handler cost
//! several-fold, so a debug verdict would be pessimistic.
//!
//! Skipped when no wgpu adapter / no pre-built `ui_widget` wasm (same gate
//! as the other bench integration tests).
//!
//! Caveats the verdict must carry: host-cached draw replay is the proposed
//! mechanism and is not yet built, so the `cached` profile measures the
//! guest-side floor (boundary + dispatch of a still-subscribed guest), not
//! the real cache's host-side bookkeeping. A true host-replay widget is not
//! dispatched at all on an unchanged frame, so its guest cost is ~0 and the
//! `cached` row here is the upper bound on what replay leaves behind. The
//! number is therefore directional — enough to size the tier line, not a
//! production benchmark.
//!
//! It is also a snapshot of the current wasmtime config: the guest is
//! Cranelift-AOT-compiled at load (not interpreted), so the per-handler cost
//! is mostly the fixed boundary + mail marshaling, the guest body already
//! near-native. Engine-config headroom the spike does not explore (opt
//! level, an AOT module cache, the pooling allocator) and any further JIT
//! tuning only lower the number, so the affordability verdict is
//! conservative. The measurement does not separate fixed marshaling from
//! JIT-optimizable guest compute — a worthwhile refinement, since a trivial
//! synthetic widget understates the guest share a compute-heavy real widget
//! would carry (and where the compiled guest holds up best).

// On-demand measurement: the table is the deliverable, printed to stderr
// where the test harness surfaces it under `--nocapture`.
#![allow(clippy::print_stderr)]

use std::env;
use std::fs;
use std::time::Instant;

use aether_actor::Actor;
use aether_capabilities::ComponentHostCapability;
use aether_data::{Kind, MailboxId};
use aether_kinds::{CostTail, CostTailResult, LoadComponent, LoadResult, Tick};
use aether_substrate_bundle::test_bench::{BenchOp, TestBench, test_helpers::require_runtime};
use aether_test_fixtures::UiWidgetConfig;

// Pin the fixture rlib so its descriptor `inventory::submit!` entries land
// in this test binary (mirrors `cost_table.rs`).
#[allow(unused_imports)]
use aether_test_fixtures as _;

/// One frame at 60fps, in nanoseconds — the budget the aggregate per-frame
/// cost has to fit inside.
const FRAME_BUDGET_NANOS: u64 = 16_666_667;

/// Widgets loaded per draw-weight cell in the fixed-vs-variable fit, averaged
/// to smooth per-instance noise.
const FIT_WIDGET_COUNT: usize = 4;

/// Load `count` instances of the `ui_widget` fixture under one profile,
/// returning their mailbox ids. Each load carries a distinct name so the
/// instances register as separate mailboxes with their own cost cells.
fn load_widgets(
    bench: &mut TestBench,
    wasm: &[u8],
    count: usize,
    redraw_each_tick: bool,
    quad_count: u32,
) -> Vec<MailboxId> {
    let config = UiWidgetConfig {
        redraw_each_tick,
        quad_count,
    }
    .encode_into_bytes();
    let mut ids = Vec::with_capacity(count);
    for i in 0..count {
        let report = bench
            .execute(vec![(
                "load",
                BenchOp::send_and_await(
                    ComponentHostCapability::NAMESPACE,
                    &LoadComponent {
                        wasm: wasm.to_vec(),
                        name: Some(format!("ui-widget-{i}")),
                        config: config.clone(),
                        export: None,
                    },
                ),
            )])
            .expect("load sequence");
        match report
            .reply::<LoadResult>("load")
            .expect("decode LoadResult")
        {
            LoadResult::Ok { mailbox_id, .. } => ids.push(mailbox_id),
            LoadResult::Err { error } => panic!("load ui-widget-{i}: {error}"),
        }
    }
    ids
}

/// The EWMA mean execution time of one widget's `Tick` handler, in
/// nanoseconds — its per-frame cost. Zero if the cell is missing (it should
/// always be seeded at load).
fn tick_mean_nanos(bench: &TestBench, mbox: MailboxId) -> u64 {
    let CostTailResult::Ok { rows } = bench.cost_table().tail(mbox, &CostTail { kind: None })
    else {
        panic!("cost tail for widget mailbox");
    };
    rows.iter()
        .find(|r| r.kind_id == Tick::ID)
        .map_or(0, |r| r.mean_nanos)
}

/// Mean per-widget `Tick` cost across a set of loaded widgets, in nanoseconds.
fn widget_mean_nanos(bench: &TestBench, ids: &[MailboxId]) -> u64 {
    let loaded = u64::try_from(ids.len()).unwrap_or(0);
    let total: u64 = ids.iter().map(|&m| tick_mean_nanos(bench, m)).sum();
    total.checked_div(loaded).unwrap_or(0)
}

/// Least-squares line `y = intercept + slope * x` over `(x, y)` samples,
/// returning `(intercept, slope)`.
#[allow(clippy::cast_precision_loss)] // nanos costs + tiny sample counts fit f64 exactly; this is a measurement aid.
fn linear_fit(samples: &[(u32, u64)]) -> (f64, f64) {
    let n = samples.len() as f64;
    if n == 0.0 {
        return (0.0, 0.0);
    }
    let xs: Vec<f64> = samples.iter().map(|&(x, _)| f64::from(x)).collect();
    let ys: Vec<f64> = samples.iter().map(|&(_, y)| y as f64).collect();
    let mean_x = xs.iter().sum::<f64>() / n;
    let mean_y = ys.iter().sum::<f64>() / n;
    let mut num = 0.0;
    let mut den = 0.0;
    for (&x, &y) in xs.iter().zip(ys.iter()) {
        let dx = x - mean_x;
        num = dx.mul_add(y - mean_y, num);
        den = dx.mul_add(dx, den);
    }
    let slope = if den == 0.0 { 0.0 } else { num / den };
    (mean_y - slope * mean_x, slope)
}

/// Parse a `u32` env override, falling back to `default`.
fn env_or(key: &str, default: u32) -> u32 {
    env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// Parse a comma-separated count sweep from the env, dropping entries below
/// `min`; falls back to `default` when unset or empty.
fn env_counts(key: &str, default: &[usize], min: usize) -> Vec<usize> {
    let parsed: Vec<usize> = env::var(key)
        .ok()
        .map(|v| {
            v.split(',')
                .filter_map(|s| s.trim().parse::<usize>().ok())
                .filter(|&n| n >= min)
                .collect()
        })
        .unwrap_or_default();
    if parsed.is_empty() {
        default.to_vec()
    } else {
        parsed
    }
}

#[test]
#[ignore = "on-demand --release measurement; run with --ignored --nocapture"]
fn widget_actor_per_frame_cost() {
    let Some(wasm_path) = require_runtime("ui_widget") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read ui_widget wasm");

    let ticks = env_or("AETHER_UI_COST_TICKS", 600);
    let quad_count = env_or("AETHER_UI_COST_QUADS", 8);
    let counts = env_counts("AETHER_UI_COST_WIDGETS", &[1, 8, 32, 64], 1);

    eprintln!();
    eprintln!(
        "widget-actor per-frame cost (issue 1793): {ticks} ticks, {quad_count} quads/draw, \
         60fps budget {FRAME_BUDGET_NANOS} nanos",
    );
    eprintln!(
        "{:>8}  {:>8}  {:>16}  {:>18}  {:>12}  {:>14}",
        "widgets", "profile", "per_widget_nanos", "aggregate_nanos", "wall_millis", "affordable@60",
    );

    for redraw_each_tick in [true, false] {
        let profile = if redraw_each_tick { "naive" } else { "cached" };
        for &count in &counts {
            let mut bench = TestBench::start_with_size(64, 48).expect("boot");
            let ids = load_widgets(&mut bench, &wasm, count, redraw_each_tick, quad_count);
            let start = Instant::now();
            bench
                .execute(vec![("advance", BenchOp::advance(ticks))])
                .expect("advance");
            let wall_millis = start.elapsed().as_secs_f64() * 1000.0;

            let per_widget = widget_mean_nanos(&bench, &ids);
            let aggregate = per_widget.saturating_mul(u64::try_from(ids.len()).unwrap_or(0));
            let affordable = FRAME_BUDGET_NANOS
                .checked_div(per_widget)
                .unwrap_or(u64::MAX);

            eprintln!(
                "{count:>8}  {profile:>8}  {per_widget:>16}  {aggregate:>18}  \
                 {wall_millis:>12.2}  {affordable:>14}",
            );
        }
    }

    eprintln!();
    eprintln!(
        "verdict inputs: `affordable@60` is budget / per_widget_nanos per profile; the naive→cached \
         per_widget delta is the per-frame boundary + emit cost host-cached replay removes. A true \
         host-replay widget pays ~0 guest nanos/frame (the host replays the retained batch), so the \
         affordable actor-backed count is bounded by host replay cost, not the guest crossing.",
    );
}

/// Splits the naive per-frame cost into its fixed and variable parts by
/// sweeping the draw weight (quads per batch) at a fixed widget count and
/// fitting a line. The intercept is the fixed boundary + dispatch + empty-send
/// floor that optimizing the draw cannot remove; the slope is the marginal
/// per-draw-item cost (guest batch build + host mail encode), the part that
/// grows with widget complexity and where wasm JIT/AOT tuning moves the number.
#[test]
#[ignore = "on-demand --release measurement; run with --ignored --nocapture"]
fn widget_cost_vs_draw_weight() {
    let Some(wasm_path) = require_runtime("ui_widget") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read ui_widget wasm");

    let ticks = env_or("AETHER_UI_COST_TICKS", 600);
    let weights = env_counts("AETHER_UI_COST_QUAD_SWEEP", &[0, 4, 16, 64, 256], 0);

    eprintln!();
    eprintln!(
        "widget-actor cost vs draw weight (issue 1793): {ticks} ticks, {FIT_WIDGET_COUNT} naive \
         widgets/cell, sweeping quads/draw to split fixed boundary cost from variable per-draw cost",
    );
    eprintln!("{:>12}  {:>16}", "quads/draw", "per_widget_nanos");

    let mut samples: Vec<(u32, u64)> = Vec::with_capacity(weights.len());
    for &weight in &weights {
        let quads = u32::try_from(weight).unwrap_or(u32::MAX);
        let mut bench = TestBench::start_with_size(64, 48).expect("boot");
        let ids = load_widgets(&mut bench, &wasm, FIT_WIDGET_COUNT, true, quads);
        bench
            .execute(vec![("advance", BenchOp::advance(ticks))])
            .expect("advance");
        let per_widget = widget_mean_nanos(&bench, &ids);
        samples.push((quads, per_widget));
        eprintln!("{quads:>12}  {per_widget:>16}");
    }

    let (intercept, slope) = linear_fit(&samples);
    eprintln!();
    eprintln!("linear fit  cost(quads) = {intercept:.0} + {slope:.1} * quads  (nanos)");
    eprintln!(
        "intercept {intercept:.0} nanos is the fixed per-frame floor (boundary crossing + dispatch + \
         empty-batch send) that optimizing the draw cannot remove. slope {slope:.1} nanos/quad is the \
         marginal per-draw-item cost (guest batch build + host mail encode); the guest-build share is \
         where wasm JIT/AOT tuning and a compute-heavy real widget move the number, the host-encode \
         share is native already. At the default 8 quads the fixed floor dominates, so a trivial \
         widget is mostly boundary cost and the slope is what grows with widget complexity.",
    );
}
