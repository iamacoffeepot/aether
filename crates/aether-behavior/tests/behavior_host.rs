//! Behavior-script host end-to-end gate (issue 2688, ADR-0137).
//!
//! The whole loop the sibling PRs (#2685/#2686/#2687) each covered only in
//! isolation: a real behavior wasm — authored against the `#[behavior]` SDK and
//! cross-built to `wasm32-unknown-unknown` — loaded into a `BehaviorHost` that
//! wraps a real slider, transforming the lane mail the slider produces, and
//! surviving a live swap with its authored state intact. The panel embeds the
//! host as an inline child (`WidgetKind::BehaviorHost`); the host spawns the
//! wrapped slider in `wire` (the #2746 inline-child lifecycle), so a drag flows
//! slider → host → panel and the script interposes in the middle.
//!
//! Observation is the panel's per-actor log ring (ADR-0081): the transformed
//! mail flows child → host → panel *inside the cluster*, never crossing the
//! broadcast sink `count_observed` watches. Each phase reads the ring with a
//! `since` cursor so one phase's entries never bleed into another's assertions.
//!
//! Minimal composition (issue #3764): the component host on the harness basics —
//! every assertion reads the log ring, so no render cap (and no wgpu gate) is
//! composed; the widgets' draw mail warn-drops harmlessly. Skipped when the
//! `behavior`-feature widget wasm / the fixture script wasm has not been pre-built
//! (the `require_wasm` gate). CI sets `AETHER_REQUIRE_RUNTIME=1` to turn the
//! skip into a hard failure.

use std::fs;

use aether_actor::Addressable;
use aether_data::Kind;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::mouse_button::LEFT;
use aether_kinds::{LoadComponent, LoadResult, LogTailResult, MouseButton, MouseButtonRelease, MouseMove, Tick};
use aether_kit_widget::{BehaviorHostSpec, PanelConfig, ScriptRef, SliderConfig, Theme, WidgetChildSpec, WidgetKind};
use serde::{Deserialize, Serialize};

/// Local twin of `aether_behavior::host::SetScript` (`aether.behavior.set_script`),
/// so the swap steps (S4/S5) drive the host without a dev-dependency on the
/// `aether-behavior` `host` feature — which would pull the interpreter subtree
/// into the workspace build. Same `#[kind(name)]` and shape, so the `KindId`
/// and wire bytes match the host's real kind; drift is caught by this scenario
/// (a swap that fails to decode). Mirrors the fixture crate's `SliderChanged`
/// twin strategy.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.behavior.set_script")]
struct SetScript {
    bytes: Vec<u8>,
}

/// Local twin of `aether_behavior::host::LoadScriptResult`
/// (`aether.behavior.load_script_result`) — the `SetScript` reply.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.behavior.load_script_result")]
enum LoadScriptResult {
    Ok { resident_bytes: u64 },
    Err { error: String },
}

/// The behavior-host slot's subname in the panel's child stack.
const SLOT: &str = "knob";

/// The clamp cap `intercept_slider` authors (`examples/intercept_slider.rs`).
/// The scenario is co-designed with the fixture, so it knows the authored cap.
const CAP: f32 = 20.0;

/// Slack for the float comparisons against the clamp cap.
const EPS: f32 = 0.5;

/// The full trampoline address the loaded panel registers at (ADR-0099 §4).
fn panel_address() -> String {
    format!("aether.component/{}:panel", aether_component::WasmTrampoline::NAMESPACE)
}

/// The host's registered inline-child lineage address: the panel's address,
/// then the trampoline scope and the slot subname (the `host_fns` `alias_name`
/// fold). Sending `SetScript` here swaps the script and gets the reply.
fn host_address() -> String {
    format!("{}/{}:{}", panel_address(), aether_component::WasmTrampoline::NAMESPACE, SLOT)
}

/// Load the reference panel with a single `BehaviorHost` slot wrapping a slider
/// over `0..=255`, its initial script inline. The host spawns the wrapped
/// slider in `wire`, so the first tick brings the whole slot up.
fn load_panel_with_host(harness: &mut SubstrateHarness, kit_wasm: &[u8], script: Vec<u8>) {
    let wrapped_config = SliderConfig {
        min: 0.0,
        max: 255.0,
        step: 1.0,
        initial: 40.0,
        theme: Theme::DEFAULT,
        state: aether_kit_widget::WidgetControlState::default(),
    }
    .encode_into_bytes();
    let host_spec = BehaviorHostSpec {
        wrapped: WidgetKind::Slider,
        wrapped_config,
        script: ScriptRef::Inline(script),
        // Zero ⇒ the host defaults (fuel ~1M, disable after 3 traps).
        fuel_per_call: 0,
        disable_after_traps: 0,
    };
    let config = PanelConfig {
        x: 10.0,
        y: 10.0,
        width: 200.0,
        font_namespace: String::new(),
        font_path: String::new(),
        theme: Theme::DEFAULT,
        children: vec![WidgetChildSpec {
            subname: SLOT.to_owned(),
            kind: WidgetKind::BehaviorHost,
            origin: [0.0, 0.0],
            clip: None,
            config: host_spec.encode_into_bytes(),
        }],
        owns_input: true,
    };
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm: kit_wasm.to_vec(),
                    name: Some("panel".to_owned()),
                    config: config.encode_into_bytes(),
                    export: Some("aether.kit.widget.panel".to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { name, .. } => {
            assert!(name.ends_with(":panel"), "the panel root should register under :panel; got {name}");
        }
        LoadResult::Err { error } => panic!("load WidgetPanel root: {error}"),
    }
}

/// A left mouse-button press at `(x, y)`.
fn press(x: f32, y: f32) -> MouseButton {
    MouseButton { button: LEFT, x, y }
}

/// A left mouse-button release at `(x, y)`.
fn release(x: f32, y: f32) -> MouseButtonRelease {
    MouseButtonRelease { button: LEFT, x, y }
}

/// One slider drag session: press mid-track, drag to the far right, release.
/// The single child sits at row `y 10..34`; a release at `x = 200` on the
/// `0..=255` slider commits a raw value well above `CAP`, so a working
/// interpose clamps it. Each drag rides its own `execute` call, so the labels
/// need only be unique within the batch.
fn drag(panel: &str) -> Vec<(&'static str, HarnessOp)> {
    vec![
        ("press", HarnessOp::send_mail(panel, &press(110.0, 22.0))),
        ("move", HarnessOp::send_mail(panel, &MouseMove { x: 200.0, y: 22.0 })),
        ("release", HarnessOp::send_mail(panel, &release(200.0, 22.0))),
    ]
}

/// Read the panel's log ring from `since`, returning the new messages plus the
/// next cursor so a later phase reads only its own entries.
fn read_panel_log(harness: &mut SubstrateHarness, since: Option<u64>) -> (Vec<String>, u64) {
    match harness.log_tail(&panel_address(), since, None) {
        LogTailResult::Ok { entries, next_since, .. } => (entries.into_iter().map(|e| e.message).collect(), next_since),
        LogTailResult::Err { error } => panic!("log_tail on the panel failed: {error}"),
    }
}

/// The value of a `key=` field in a rendered log line (`None` if absent).
fn field<'a>(message: &'a str, key: &str) -> Option<&'a str> {
    message.split_whitespace().find_map(|token| token.strip_prefix(key))
}

/// Whether a log line is a slider-changed value-up attributed to the slot.
fn slider_line(message: &str) -> bool {
    message.contains("widget slider changed") && field(message, "widget=") == Some(SLOT)
}

/// The committed slider values (`committed=true`) attributed to the slot.
fn committed_values(messages: &[String]) -> Vec<f32> {
    messages
        .iter()
        .filter(|m| slider_line(m) && m.contains("committed=true"))
        .filter_map(|m| field(m, "value=").and_then(|v| v.parse::<f32>().ok()))
        .collect()
}

/// The `index=` values of radio-selected lines attributed to the slot — the
/// scripts' `ctx.panel().emit` effect carrying their running `count`.
fn emitted_counts(messages: &[String]) -> Vec<u32> {
    messages
        .iter()
        .filter(|m| m.contains("widget radio selected") && field(m, "widget=") == Some(SLOT))
        .filter_map(|m| field(m, "index=").and_then(|v| v.parse::<u32>().ok()))
        .collect()
}

/// Swap the running script for `bytes` via `aether.behavior.set_script`,
/// asserting the host replies `LoadScriptResult::Ok`.
fn swap_script(harness: &mut SubstrateHarness, label: &str, bytes: Vec<u8>) {
    let host = host_address();
    let swapped = harness
        .execute(vec![(label, HarnessOp::send_and_await(&host, &SetScript { bytes }))])
        .unwrap_or_else(|error| panic!("{label} swap: {error:?}"));
    match swapped.reply::<LoadScriptResult>(label).expect("decode LoadScriptResult") {
        LoadScriptResult::Ok { .. } => {}
        LoadScriptResult::Err { error } => panic!("{label} set_script failed: {error}"),
    }
}

/// Drive the whole scripted-behavior loop end to end: interpose, consume,
/// effect drain, swap-state-carry, and fail-open — each phase catching its own
/// owned-bug class, none re-asserting the base slider routing `widget_set`
/// already covers.
#[test]
fn behavior_host_intercepts_consumes_carries_state_and_fails_open() {
    // The host-carrying widget variant (`--features behavior`, wasmi linked in),
    // built to its own stem by `cargo xtask dist` so the stock
    // `aether_kit_widget.wasm` the other scenarios load stays lean (issue 2688).
    let Some(kit_path) = require_wasm("aether_kit_widget_behavior") else {
        return;
    };
    let Some(intercept_path) = require_wasm("intercept_slider") else {
        return;
    };
    let Some(v2_path) = require_wasm("intercept_slider_v2") else {
        return;
    };
    let Some(trap_path) = require_wasm("trap_script") else {
        return;
    };
    let kit_wasm = fs::read(&kit_path).expect("read kit wasm");
    let intercept = fs::read(&intercept_path).expect("read intercept_slider wasm");
    let v2 = fs::read(&v2_path).expect("read intercept_slider_v2 wasm");
    let trap = fs::read(&trap_path).expect("read trap_script wasm");

    let mut harness = SubstrateHarness::builder().with_component_host().build().expect("boot");
    load_panel_with_host(&mut harness, &kit_wasm, intercept);
    let panel = panel_address();

    // First tick spawns the host, which spawns + frames the wrapped slider.
    // Then S1/S2/S3: one drag through the `intercept_slider` script.
    let mut ops = vec![("spawn", HarnessOp::send_mail(&panel, &Tick))];
    ops.extend(drag(&panel));
    harness.execute(ops).expect("spawn + S1 drag");
    let (phase1, cursor) = read_panel_log(&mut harness, None);
    let joined1 = phase1.join("\n");

    // S1 — the intercept mutates and forwards: the committed value reaching the
    // panel is clamped, not the raw ~242 the far-right drag produced. Catches
    // interposition up-lane routing + the `&mut K` re-encode/forward.
    let committed = committed_values(&phase1);
    assert!(!committed.is_empty(), "the drag-release should forward one committed change; log was:\n{joined1}");
    assert!(
        committed.iter().all(|v| *v <= CAP + EPS),
        "every committed value the script forwards must be clamped to {CAP}; \
         got {committed:?}; log was:\n{joined1}",
    );

    // S2 — consume drops: the uncommitted drag stream is consumed, so no
    // `committed=false` slider line reaches the panel, while the committed
    // release still lands (S1). Catches `ctx.consume()` — the same kind takes
    // both consume and forward in one session.
    assert!(
        !phase1.iter().any(|m| slider_line(m) && m.contains("committed=false")),
        "consumed uncommitted changes must not forward up-lane; log was:\n{joined1}",
    );

    // S3 — effect drains: the clamp's `ctx.panel().emit` reaches the panel as a
    // radio-selected line attributed to the slot (a slider-wrapped host can
    // only log one via the script's effect). The first clamp carries count 1.
    assert!(
        emitted_counts(&phase1).contains(&1),
        "the clamp effect should drain to the panel as count 1; log was:\n{joined1}",
    );

    // S4 — swap preserves authored state: swap to `intercept_slider_v2` and
    // drive another change. The carried `count` continues to 2 (not reset to
    // 1), and the carried `cap` (20, not v2's fresh 1000 default) still clamps.
    // Catches the `state_save` → `state_load` carry across the swap seam.
    swap_script(&mut harness, "swap_v2", v2);
    harness.execute(drag(&panel)).expect("S4 drag after swap");
    let (phase2, cursor) = read_panel_log(&mut harness, Some(cursor));
    let joined2 = phase2.join("\n");
    assert!(
        emitted_counts(&phase2).contains(&2),
        "the swapped script must continue the carried count to 2, not reset; \
         log was:\n{joined2}",
    );
    let committed_v2 = committed_values(&phase2);
    assert!(
        !committed_v2.is_empty() && committed_v2.iter().all(|v| *v <= CAP + EPS),
        "the carried cap ({CAP}) must still clamp after the swap; got {committed_v2:?}; \
         log was:\n{joined2}",
    );

    // S5 — trap fails open through the full stack: swap to a script that spins
    // to fuel exhaustion, then drive a change. The host must forward the raw,
    // untransformed value (> cap) rather than the lane wedging. Catches
    // integration-level fail-open — a trap wedging the lane, not the filter
    // call #2687's host-unit already drives directly.
    swap_script(&mut harness, "swap_trap", trap);
    harness.execute(drag(&panel)).expect("S5 drag after trap swap");
    let (phase3, _) = read_panel_log(&mut harness, Some(cursor));
    let joined3 = phase3.join("\n");
    let committed_trap = committed_values(&phase3);
    assert!(
        committed_trap.iter().any(|v| *v > CAP + EPS),
        "a trapping script must fail open — the raw unclamped value forwards; \
         got {committed_trap:?}; log was:\n{joined3}",
    );
}
