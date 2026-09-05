//! Strict acceptance for ADR-0141 editor-wide input routing.

use std::fs;
use std::path::Path;

use aether_data::{Kind, MailboxId};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::keycode::{KEY_BACKQUOTE, KEY_TAB};
use aether_kinds::{
    ImePreedit, Key, KeyRelease, LoadComponent, LoadResult, Modifiers, MouseButton, MouseButtonRelease, MouseMove,
    MouseWheel, TextInput, WindowId,
};
use aether_kit_widget::{EditorConfig, EditorKeyChord, EditorRegionRect, RegionInputLanes, RegionSpec};
use aether_test_fixtures_kinds::{
    DrainEditorInputs, DrainEditorInputsResult, EditorRegionProbeConfig, ObservedEditorInput,
};

const TEST_WINDOW_ID: WindowId = WindowId(1);

struct LoadedActor {
    mailbox_id: MailboxId,
    address: String,
}

fn load_actor<K: Kind>(
    harness: &mut SubstrateHarness,
    wasm_path: &Path,
    export: &str,
    name: &str,
    config: &K,
) -> LoadedActor {
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                "aether.component",
                &LoadComponent {
                    wasm: fs::read(wasm_path).expect("read wasm component"),
                    name: Some(name.to_owned()),
                    config: config.encode_into_bytes(),
                    export: Some(export.to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { mailbox_id, name: address, .. } => LoadedActor { mailbox_id, address },
        LoadResult::Err { error } => panic!("load {export} as {name}: {error}"),
    }
}

fn region(name: &str, target: MailboxId, x_pixels: f32, input_lanes: RegionInputLanes) -> RegionSpec {
    RegionSpec {
        name: name.to_owned(),
        rect: EditorRegionRect { x_pixels, y_pixels: 0.0, width_pixels: 100.0, height_pixels: 100.0 },
        target,
        keyboard_focus_eligible: true,
        input_lanes,
        activation_chord: None,
    }
}

fn load_probe(harness: &mut SubstrateHarness, wasm_path: &Path, name: &str) -> LoadedActor {
    load_actor(harness, wasm_path, "test.editor_region_probe", name, &EditorRegionProbeConfig { name: name.to_owned() })
}

fn load_shell(harness: &mut SubstrateHarness, wasm_path: &Path, regions: Vec<RegionSpec>) {
    let _shell = load_actor(harness, wasm_path, "aether.kit.widget.editor", "editor", &EditorConfig { regions });
}

fn drain(harness: &mut SubstrateHarness, actor: &LoadedActor, label: &'static str) -> DrainEditorInputsResult {
    harness
        .execute(vec![(label, HarnessOp::send_and_await_reply(actor.address.as_str(), &DrainEditorInputs))])
        .expect("drain sequence")
        .reply::<DrainEditorInputsResult>(label)
        .expect("decode DrainEditorInputsResult")
}

fn input<K: Kind>(mail: &K) -> HarnessOp {
    HarnessOp::window_event(TEST_WINDOW_ID, mail)
}

#[test]
fn first_press_owns_cross_region_drag_and_lanes_filter_at_the_hit_region() {
    let (Some(kit_wasm), Some(fixtures_wasm)) =
        (require_wasm("aether_kit_widget"), require_wasm("aether_test_fixtures_bundle"))
    else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(200, 100).with_component_host().build().expect("boot");
    let region_a = load_probe(&mut harness, &fixtures_wasm, "region-a");
    let region_b = load_probe(&mut harness, &fixtures_wasm, "region-b");
    let mut b_lanes = RegionInputLanes::ALL;
    b_lanes.wheel = false;
    load_shell(
        &mut harness,
        &kit_wasm,
        vec![
            region("region-a", region_a.mailbox_id, 0.0, RegionInputLanes::ALL),
            region("region-b", region_b.mailbox_id, 100.0, b_lanes),
        ],
    );

    harness
        .execute(vec![
            ("press-a", input(&MouseButton { window: TEST_WINDOW_ID, button: 0, x: 20.0, y: 20.0 })),
            ("drag-b", input(&MouseMove { window: TEST_WINDOW_ID, x: 140.0, y: 25.0 })),
            ("release-other-b", input(&MouseButtonRelease { window: TEST_WINDOW_ID, button: 1, x: 140.0, y: 25.0 })),
            ("release-owner-b", input(&MouseButtonRelease { window: TEST_WINDOW_ID, button: 0, x: 140.0, y: 25.0 })),
            ("move-b", input(&MouseMove { window: TEST_WINDOW_ID, x: 150.0, y: 30.0 })),
            (
                "release-without-owner-b",
                input(&MouseButtonRelease { window: TEST_WINDOW_ID, button: 0, x: 150.0, y: 30.0 }),
            ),
            (
                "filtered-wheel-b",
                input(&MouseWheel { window: TEST_WINDOW_ID, delta_x: 0.0, delta_y: -12.0, x: 150.0, y: 30.0 }),
            ),
        ])
        .expect("route pointer sequence");

    assert_eq!(
        drain(&mut harness, &region_a, "drain-a"),
        DrainEditorInputsResult {
            region_name: "region-a".to_owned(),
            inputs: vec![
                ObservedEditorInput::Modifiers { shift: false, ctrl: false, alt: false, meta: false },
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
        drain(&mut harness, &region_b, "drain-b"),
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
    let (Some(kit_wasm), Some(fixtures_wasm)) =
        (require_wasm("aether_kit_widget"), require_wasm("aether_test_fixtures_bundle"))
    else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(200, 100).with_component_host().build().expect("boot");
    let region_a = load_probe(&mut harness, &fixtures_wasm, "focus-a");
    let region_b = load_probe(&mut harness, &fixtures_wasm, "focus-b");
    let a = region("focus-a", region_a.mailbox_id, 0.0, RegionInputLanes::ALL);
    let mut b = region("focus-b", region_b.mailbox_id, 100.0, RegionInputLanes::ALL);
    b.activation_chord =
        Some(EditorKeyChord { key_code: KEY_BACKQUOTE, shift: false, ctrl: false, alt: false, meta: false });
    load_shell(&mut harness, &kit_wasm, vec![a, b]);

    harness
        .execute(vec![
            ("focus-a", input(&MouseButton { window: TEST_WINDOW_ID, button: 0, x: 20.0, y: 20.0 })),
            ("release-a", input(&MouseButtonRelease { window: TEST_WINDOW_ID, button: 0, x: 20.0, y: 20.0 })),
        ])
        .expect("prime focus");
    let _initial_a = drain(&mut harness, &region_a, "drain-initial-a");

    harness
        .execute(vec![
            ("key-a", input(&Key { window: TEST_WINDOW_ID, code: 65 })),
            ("text-a", input(&TextInput { window: TEST_WINDOW_ID, text: "a".to_owned() })),
            ("activate-b", input(&Key { window: TEST_WINDOW_ID, code: KEY_BACKQUOTE })),
            (
                "ime-b",
                input(&ImePreedit {
                    window: TEST_WINDOW_ID,
                    text: "composition".to_owned(),
                    cursor_begin: Some(1),
                    cursor_end: Some(3),
                }),
            ),
            ("text-b", input(&TextInput { window: TEST_WINDOW_ID, text: "b".to_owned() })),
            ("ctrl-b", input(&Modifiers { window: TEST_WINDOW_ID, shift: false, ctrl: true, alt: false, meta: false })),
            ("cycle-a", input(&Key { window: TEST_WINDOW_ID, code: KEY_TAB })),
            ("cycle-release", input(&KeyRelease { window: TEST_WINDOW_ID, code: KEY_TAB })),
            ("clear-modifiers", input(&Modifiers { window: TEST_WINDOW_ID, ..Modifiers::default() })),
            ("plain-tab", input(&Key { window: TEST_WINDOW_ID, code: KEY_TAB })),
            ("plain-tab-release", input(&KeyRelease { window: TEST_WINDOW_ID, code: KEY_TAB })),
        ])
        .expect("route keyboard sequence");

    assert_eq!(
        drain(&mut harness, &region_a, "drain-focus-a"),
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
        drain(&mut harness, &region_b, "drain-focus-b"),
        DrainEditorInputsResult {
            region_name: "focus-b".to_owned(),
            inputs: vec![
                ObservedEditorInput::Modifiers { shift: false, ctrl: false, alt: false, meta: false },
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
