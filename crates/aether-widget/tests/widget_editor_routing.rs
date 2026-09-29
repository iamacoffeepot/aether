//! Strict acceptance for ADR-0141 editor-wide input routing.
//!
//! Assembly is shell-first: the shell is loaded under its default name with an
//! ordered, targetless region table, and each probe then announces itself under
//! its region name. Nothing routes until that announcement lands, so every
//! assertion below also proves the attach handshake by construction.

mod support;

use std::fs;
use std::path::Path;

use aether_actor::{ActorRef, ErasedActorRef};
use aether_data::Kind;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_harness_substrate_capture::test_helpers::{
    init_save_sandbox, require_runtime, require_wasm, test_namespace_roots,
};
use aether_kinds::keycode::{KEY_BACKQUOTE, KEY_TAB};
use aether_kinds::{
    ImePreedit, Key, KeyRelease, LoadComponent, Modifiers, MouseButton, MouseButtonRelease, MouseMove, MouseWheel,
    TextInput,
};
use aether_test_fixtures_bundle::EditorRegionProbe;
use aether_test_fixtures_kinds::{
    DrainEditorInputs, DrainEditorInputsResult, EditorRegionProbeConfig, ObservedEditorInput,
};
use aether_widget::{EditorConfig, EditorKeyChord, EditorRegionRect, RegionInputLanes, RegionSpec};
use aether_window::WindowCapability;
use support::widget_caps;

/// The window the injected input events name.
fn test_window() -> aether_data::ErasedActorPath {
    aether_window::window_path(&aether_data::LoadName::new("main").expect("a valid window name"))
}

/// A GPU bench with the component host and everything the widget module
/// declares: the real render, text (its fs from the sandbox roots) and the
/// in-memory clipboard.
fn bench(width: u32, height: u32) -> SubstrateHarness {
    widget_caps(
        SubstrateHarness::builder()
            .size(width, height)
            .namespace_roots(test_namespace_roots(init_save_sandbox("widget-editor-routing")))
            .with_render()
            .with_component_host(),
    )
    .build()
    .expect("boot")
}

/// Load one in-bundle actor and return its erased reference. `name` is the
/// load name, or `None` to load under the actor's own namespace — which is
/// what the shell needs, since a region names it by bare type.
fn load_actor<K: Kind>(
    harness: &mut SubstrateHarness,
    wasm_path: &Path,
    export: &str,
    name: Option<&str>,
    config: &K,
) -> ErasedActorRef {
    harness
        .load_any(&LoadComponent {
            wasm: fs::read(wasm_path).expect("read wasm component"),
            name: name.map(str::to_owned),
            config: config.encode_into_bytes(),
            export: Some(export.to_owned()),
        })
        .unwrap_or_else(|error| panic!("load {export} as {name:?}: {error}"))
        .0
}

fn region(name: &str, x_pixels: f32, input_lanes: RegionInputLanes) -> RegionSpec {
    RegionSpec {
        name: name.to_owned(),
        rect: EditorRegionRect { x_pixels, y_pixels: 0.0, width_pixels: 100.0, height_pixels: 100.0 },
        keyboard_focus_eligible: true,
        input_lanes,
        activation_chord: None,
    }
}

fn load_probe(harness: &mut SubstrateHarness, wasm_path: &Path, name: &str) -> ActorRef<EditorRegionProbe> {
    let (probe, _) = harness
        .load::<EditorRegionProbe>(LoadComponent {
            wasm: fs::read(wasm_path).expect("read wasm component"),
            name: Some(name.to_owned()),
            config: EditorRegionProbeConfig { name: name.to_owned() }.encode_into_bytes(),
            export: None,
        })
        .unwrap_or_else(|error| panic!("load the region probe as {name}: {error}"));
    probe
}

fn load_shell(harness: &mut SubstrateHarness, wasm_path: &Path, regions: Vec<RegionSpec>) {
    let _shell = load_actor(harness, wasm_path, "aether.widget.editor", None, &EditorConfig { regions });
}

fn drain(
    harness: &mut SubstrateHarness,
    probe: ActorRef<EditorRegionProbe>,
    label: &'static str,
) -> DrainEditorInputsResult {
    harness
        .execute(vec![(label, HarnessOp::send_and_await_reply(&probe, &DrainEditorInputs))])
        .expect("drain sequence")
        .reply::<DrainEditorInputsResult>(label)
        .expect("decode DrainEditorInputsResult")
}

fn input<K: Kind>(synthetic: ActorRef<WindowCapability>, mail: &K) -> HarnessOp {
    HarnessOp::window_event(&synthetic, test_window(), mail)
}

#[test]
fn first_press_owns_cross_region_drag_and_lanes_filter_at_the_hit_region() {
    let (Some(widget_wasm), Some(fixtures_wasm)) =
        (require_runtime("aether_widget"), require_wasm("aether_test_fixtures_bundle"))
    else {
        return;
    };
    let mut harness = bench(200, 100);
    let mut b_lanes = RegionInputLanes::ALL;
    b_lanes.wheel = false;
    load_shell(
        &mut harness,
        &widget_wasm,
        vec![region("region-a", 0.0, RegionInputLanes::ALL), region("region-b", 100.0, b_lanes)],
    );

    let region_a = load_probe(&mut harness, &fixtures_wasm, "region-a");
    let region_b = load_probe(&mut harness, &fixtures_wasm, "region-b");
    let synthetic = harness.actor_ref::<WindowCapability>();

    harness
        .execute(vec![
            ("press-a", input(synthetic, &MouseButton { window: test_window(), button: 0, x: 20.0, y: 20.0 })),
            ("drag-b", input(synthetic, &MouseMove { window: test_window(), x: 140.0, y: 25.0 })),
            (
                "release-other-b",
                input(synthetic, &MouseButtonRelease { window: test_window(), button: 1, x: 140.0, y: 25.0 }),
            ),
            (
                "release-owner-b",
                input(synthetic, &MouseButtonRelease { window: test_window(), button: 0, x: 140.0, y: 25.0 }),
            ),
            ("move-b", input(synthetic, &MouseMove { window: test_window(), x: 150.0, y: 30.0 })),
            (
                "release-without-owner-b",
                input(synthetic, &MouseButtonRelease { window: test_window(), button: 0, x: 150.0, y: 30.0 }),
            ),
            (
                "filtered-wheel-b",
                input(
                    synthetic,
                    &MouseWheel { window: test_window(), delta_x: 0.0, delta_y: -12.0, x: 150.0, y: 30.0 },
                ),
            ),
        ])
        .expect("route pointer sequence");

    assert_eq!(
        drain(&mut harness, region_a, "drain-a"),
        DrainEditorInputsResult {
            region_name: "region-a".to_owned(),
            // No `Modifiers` has arrived yet, so focusing region-a primes it
            // with none: the shell caches no modifier state before the first.
            inputs: vec![
                ObservedEditorInput::PointerPress { button: 0, x_pixels: 20.0, y_pixels: 20.0 },
                ObservedEditorInput::PointerMotion { x_pixels: 140.0, y_pixels: 25.0 },
                ObservedEditorInput::PointerRelease { button: 1, x_pixels: 140.0, y_pixels: 25.0 },
                ObservedEditorInput::PointerRelease { button: 0, x_pixels: 140.0, y_pixels: 25.0 },
                // `move-b` routes to region-b, so region-a is handed the same
                // motion as the region the pointer left: it re-derives its own
                // hover against a position outside itself and lets go of the
                // child it had lit. Without it that child stays hovered for as
                // long as the pointer is in the other pane.
                ObservedEditorInput::PointerMotion { x_pixels: 150.0, y_pixels: 30.0 },
            ],
        },
    );
    assert_eq!(
        drain(&mut harness, region_b, "drain-b"),
        DrainEditorInputsResult {
            region_name: "region-b".to_owned(),
            inputs: vec![
                ObservedEditorInput::PointerMotion { x_pixels: 150.0, y_pixels: 30.0 },
                ObservedEditorInput::PointerRelease { button: 0, x_pixels: 150.0, y_pixels: 30.0 },
            ],
        },
    );
}

#[test]
fn focus_activation_and_reserved_cycle_route_each_keyboard_lane_once() {
    let (Some(widget_wasm), Some(fixtures_wasm)) =
        (require_runtime("aether_widget"), require_wasm("aether_test_fixtures_bundle"))
    else {
        return;
    };
    let mut harness = bench(200, 100);
    let a = region("focus-a", 0.0, RegionInputLanes::ALL);
    let mut b = region("focus-b", 100.0, RegionInputLanes::ALL);
    b.activation_chord =
        Some(EditorKeyChord { key_code: KEY_BACKQUOTE, shift: false, ctrl: false, alt: false, meta: false });
    load_shell(&mut harness, &widget_wasm, vec![a, b]);

    let region_a = load_probe(&mut harness, &fixtures_wasm, "focus-a");
    let region_b = load_probe(&mut harness, &fixtures_wasm, "focus-b");
    let synthetic = harness.actor_ref::<WindowCapability>();

    harness
        .execute(vec![
            ("focus-a", input(synthetic, &MouseButton { window: test_window(), button: 0, x: 20.0, y: 20.0 })),
            ("release-a", input(synthetic, &MouseButtonRelease { window: test_window(), button: 0, x: 20.0, y: 20.0 })),
        ])
        .expect("prime focus");
    let _initial_a = drain(&mut harness, region_a, "drain-initial-a");

    harness
        .execute(vec![
            ("key-a", input(synthetic, &Key { window: test_window(), code: 65 })),
            ("text-a", input(synthetic, &TextInput { window: test_window(), text: "a".to_owned() })),
            ("activate-b", input(synthetic, &Key { window: test_window(), code: KEY_BACKQUOTE })),
            (
                "ime-b",
                input(
                    synthetic,
                    &ImePreedit {
                        window: test_window(),
                        text: "composition".to_owned(),
                        cursor_begin: Some(1),
                        cursor_end: Some(3),
                    },
                ),
            ),
            ("text-b", input(synthetic, &TextInput { window: test_window(), text: "b".to_owned() })),
            (
                "ctrl-b",
                input(
                    synthetic,
                    &Modifiers { window: test_window(), shift: false, ctrl: true, alt: false, meta: false },
                ),
            ),
            ("cycle-a", input(synthetic, &Key { window: test_window(), code: KEY_TAB })),
            ("cycle-release", input(synthetic, &KeyRelease { window: test_window(), code: KEY_TAB })),
            (
                "clear-modifiers",
                input(
                    synthetic,
                    &Modifiers { window: test_window(), shift: false, ctrl: false, alt: false, meta: false },
                ),
            ),
            ("plain-tab", input(synthetic, &Key { window: test_window(), code: KEY_TAB })),
            ("plain-tab-release", input(synthetic, &KeyRelease { window: test_window(), code: KEY_TAB })),
        ])
        .expect("route keyboard sequence");

    assert_eq!(
        drain(&mut harness, region_a, "drain-focus-a"),
        DrainEditorInputsResult {
            region_name: "focus-a".to_owned(),
            inputs: vec![
                ObservedEditorInput::KeyPress { code: 65 },
                ObservedEditorInput::TextInput { text: "a".to_owned() },
                ObservedEditorInput::Modifiers { shift: false, ctrl: true, alt: false, meta: false },
                ObservedEditorInput::Modifiers { shift: false, ctrl: false, alt: false, meta: false },
                ObservedEditorInput::KeyPress { code: KEY_TAB },
                ObservedEditorInput::KeyRelease { code: KEY_TAB },
            ],
        },
    );
    assert_eq!(
        drain(&mut harness, region_b, "drain-focus-b"),
        DrainEditorInputsResult {
            region_name: "focus-b".to_owned(),
            // Activation focuses focus-b before any `Modifiers` has arrived,
            // so there is no cached state to prime it with.
            inputs: vec![
                ObservedEditorInput::KeyPress { code: KEY_BACKQUOTE },
                ObservedEditorInput::ImePreedit {
                    text: "composition".to_owned(),
                    cursor_begin: Some(1),
                    cursor_end: Some(3),
                },
                ObservedEditorInput::TextInput { text: "b".to_owned() },
                ObservedEditorInput::Modifiers { shift: false, ctrl: true, alt: false, meta: false },
            ],
        },
    );
}
