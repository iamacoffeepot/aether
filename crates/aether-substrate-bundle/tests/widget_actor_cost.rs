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

/// Parse a `u32` env override, falling back to `default`.
fn env_or(key: &str, default: u32) -> u32 {
    env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// Parse a comma-separated widget-count sweep from the env, dropping
/// non-positive entries; falls back to `default` when unset or empty.
fn env_widget_counts(key: &str, default: &[usize]) -> Vec<usize> {
    let parsed: Vec<usize> = env::var(key)
        .ok()
        .map(|v| {
            v.split(',')
                .filter_map(|s| s.trim().parse::<usize>().ok())
                .filter(|&n| n > 0)
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
    let counts = env_widget_counts("AETHER_UI_COST_WIDGETS", &[1, 8, 32, 64]);

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

            let loaded = u64::try_from(ids.len()).unwrap_or(0);
            let total: u64 = ids.iter().map(|&m| tick_mean_nanos(&bench, m)).sum();
            let per_widget = total.checked_div(loaded).unwrap_or(0);
            let aggregate = per_widget.saturating_mul(loaded);
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
