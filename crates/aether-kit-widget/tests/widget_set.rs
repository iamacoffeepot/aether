//! Widget-set end-to-end scenario (issue 2660).
//!
//! Load the reference `WidgetPanel` root, drive it with synthetic pointer and
//! keyboard input, and assert the value-up events reach the panel end-to-end —
//! the kit-owned routing (`Focus` hit-test / drag-capture / Tab cycle), the
//! slider drag-value math, radio selection, and text editing all working
//! through the real inline-cluster FIFO drain, not just the unit tests over
//! the helper structs. The panel logs each value-up into its per-actor log
//! ring (ADR-0081); the scenario tails the ring and reads the attributed
//! events back.
//!
//! Value-up events flow child → panel *inside the cluster*, so they never
//! cross the observable render / broadcast sink `count_observed` watches — the
//! log ring is the correct observation surface here. The rendered-output gate
//! (one root render sender per cluster) is issue 2659's `widget_compositing`
//! scenario and is not duplicated.
//!
//! Everything observable here is typed mail + the log ring, so the harness
//! composes only the component host — no render target, hence no wgpu gate:
//! the scenario skips only when the `aether_kit_widget` wasm has not been pre-built
//! (`require_wasm`). CI sets `AETHER_REQUIRE_RUNTIME=1` to turn that skip
//! into a hard failure.

// Integration-test skip diagnostic: emit via stderr so `cargo test` surfaces
// "skipping: ..." alongside `test ... ok` (issue 891).
#![allow(clippy::print_stderr)]
// Pixel-rect layout constants read clearest as float literals inline.
#![allow(clippy::cast_precision_loss)]

use std::fs;

use aether_actor::Addressable;
use aether_data::Kind;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::keycode::{KEY_DOWN, KEY_ENTER, KEY_PAGE_DOWN, KEY_SPACE, KEY_TAB, KEY_UP};
use aether_kinds::mouse_button::LEFT;
use aether_kinds::{
    Key, KeyRelease, LoadComponent, LoadResult, LogTailResult, Modifiers, MouseButton, MouseButtonRelease, MouseMove,
    TextInput, Tick, WindowId,
};
use aether_kit_widget::{
    ButtonConfig, PanelConfig, RadioConfig, SetWidgetState, SliderConfig, TextFieldConfig, Theme, VirtualListConfig,
    VirtualListRow, WidgetChildSpec, WidgetControlState, WidgetKind,
};

const TEST_WINDOW_ID: WindowId = WindowId(1);

/// The full trampoline address the loaded panel registers at (ADR-0099 §4).
fn panel_address() -> String {
    format!("aether.component/{}:panel", aether_component::WasmTrampoline::NAMESPACE)
}

fn child_address(subname: &str) -> String {
    format!("{}/{}:{}", panel_address(), aether_component::WasmTrampoline::NAMESPACE, subname)
}

/// Load the `WidgetPanel` root under the name `panel` (export
/// `aether.kit.widget.panel`) with a config that places its stack at
/// `(10, 10)` 200px wide, no font (`font_path` empty, so no `aether.text`
/// dependency), and the default theme.
fn load_panel(harness: &mut SubstrateHarness, wasm: &[u8]) -> String {
    let config = PanelConfig {
        x: 10.0,
        y: 10.0,
        width: 200.0,
        font_namespace: String::new(),
        font_path: String::new(),
        theme: Theme::DEFAULT,
        children: Vec::new(),
        owns_input: true,
    };
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                "aether.component",
                &LoadComponent {
                    wasm: wasm.to_vec(),
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
            name
        }
        LoadResult::Err { error } => panic!("load WidgetPanel root: {error}"),
    }
}

/// Every log message in the panel's ring, oldest first.
fn panel_log_messages(harness: &mut SubstrateHarness) -> Vec<String> {
    match harness.log_tail(&panel_address(), None, None) {
        LogTailResult::Ok { entries, .. } => entries.into_iter().map(|e| e.message).collect(),
        LogTailResult::Err { error } => panic!("log_tail on the panel failed: {error}"),
    }
}

/// A left mouse-button press at `(x, y)`.
fn press(x: f32, y: f32) -> MouseButton {
    MouseButton { window: TEST_WINDOW_ID, button: LEFT, x, y }
}

/// A left mouse-button release at `(x, y)`.
fn release(x: f32, y: f32) -> MouseButtonRelease {
    MouseButtonRelease { window: TEST_WINDOW_ID, button: LEFT, x, y }
}

/// Drive the reference panel through a full input session — a slider drag, a
/// Tab-then-arrow keyboard move, a radio click, and a text entry — and read
/// the attributed value-up events back off the panel's log ring.
///
/// Layout under the default theme (`row_height` 24, `gap` 6), stack at
/// `(10, 10)` 200px wide:
///   label   y 10..34   slider  y 40..64   radio y 70..142
///   text    y 148..172 button  y 178..202
#[test]
fn panel_routes_input_to_widgets_and_reports_values_up() {
    let Some(wasm_path) = require_wasm("aether_kit_widget") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut harness = SubstrateHarness::builder().size(240, 220).with_component_host().build().expect("boot");
    let panel = load_panel(&mut harness, &wasm);

    // The first tick spawns the widget stack and assigns each child its frame;
    // every later step drives one input event, settling its whole in-cluster
    // chain before the next.
    // Each input kind is fire-and-forget (no reply), so `send_and_settle` — which
    // waits out the whole dispatched chain without expecting an answer — is the op;
    // `send_and_await_reply` would hang waiting for a reply that never comes.
    harness
        .execute(vec![
            ("spawn", HarnessOp::send_and_settle(&panel, &Tick::default())),
            // Slider drag: press mid-track, drag right, release — the release
            // commits at the dragged value (x=160 → 75% of 0..255 ≈ 191). The
            // press also focuses the slider.
            ("drag_press", HarnessOp::send_and_settle(&panel, &press(110.0, 52.0))),
            ("drag_move", HarnessOp::send_and_settle(&panel, &MouseMove { window: TEST_WINDOW_ID, x: 160.0, y: 52.0 })),
            ("drag_release", HarnessOp::send_and_settle(&panel, &release(160.0, 52.0))),
            // Tab moves focus off the slider to the radio group; Down then
            // routes to the focused radio, moving its selection to index 1.
            ("tab", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_TAB })),
            ("radio_key", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_DOWN })),
            // A click on the third radio row (y 118..142) selects index 2.
            ("radio_press", HarnessOp::send_and_settle(&panel, &press(30.0, 125.0))),
            ("radio_release", HarnessOp::send_and_settle(&panel, &release(30.0, 125.0))),
            // Focus the text field (y 148..172), type into it, and commit.
            ("text_focus", HarnessOp::send_and_settle(&panel, &press(50.0, 160.0))),
            ("text_focus_up", HarnessOp::send_and_settle(&panel, &release(50.0, 160.0))),
            ("type", HarnessOp::send_and_settle(&panel, &TextInput { window: TEST_WINDOW_ID, text: "hi".to_owned() })),
            ("commit", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_ENTER })),
        ])
        .expect("input session");

    let log = panel_log_messages(&mut harness);
    let joined = log.join("\n");

    assert!(
        log.iter().any(|m| m.contains("widget slider changed") && m.contains("committed=true")),
        "the slider drag-release should log a committed change; log was:\n{joined}",
    );
    assert!(
        log.iter().any(|m| m.contains("widget slider changed")
            && m.contains("widget=slider")
            && m.contains("committed=false")),
        "the drag should stream at least one uncommitted change; log was:\n{joined}",
    );
    assert!(
        log.iter().any(|m| m.contains("widget radio selected") && m.contains("index=1")),
        "Tab-then-Down should route to the radio and select index 1 — proving the \
         focus cycle and keyboard routing; log was:\n{joined}",
    );
    assert!(
        log.iter().any(|m| m.contains("widget radio selected") && m.contains("index=2")),
        "the radio row click should select index 2; log was:\n{joined}",
    );
    assert!(
        log.iter().any(|m| m.contains("widget text committed") && m.contains("text=hi")),
        "the text entry then Enter should commit \"hi\" — proving pointer focus \
         and text routing; log was:\n{joined}",
    );
}

/// The `LoadResult.name` returned at the public component boundary is the
/// prefix for first-class inline-child names. Appending the built-in slot's
/// `aether.embedded:button` node must let an external name-addressed sender
/// change that live Button's state; a blocked then enabled click is the
/// positive/negative proof that the mail reached the child rather than being
/// warn-dropped at an unknown name.
#[test]
fn load_result_lineage_reaches_builtin_button_state_externally() {
    let Some(wasm_path) = require_wasm("aether_kit_widget") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut harness = SubstrateHarness::builder().size(240, 220).with_component_host().build().expect("boot");
    let panel = load_panel(&mut harness, &wasm);
    let button = format!("{panel}/{}:button", aether_component::WasmTrampoline::NAMESPACE);
    let unavailable = WidgetControlState { enabled: false, ..WidgetControlState::default() };

    harness
        .execute(vec![
            ("spawn", HarnessOp::send_and_settle(&panel, &Tick::default())),
            ("disable_by_lineage", HarnessOp::send_and_settle(&button, &SetWidgetState { state: unavailable })),
            ("blocked_press", HarnessOp::send_and_settle(&panel, &press(30.0, 190.0))),
            ("blocked_release", HarnessOp::send_and_settle(&panel, &release(30.0, 190.0))),
            (
                "enable_by_lineage",
                HarnessOp::send_and_settle(&button, &SetWidgetState { state: WidgetControlState::default() }),
            ),
            ("allowed_press", HarnessOp::send_and_settle(&panel, &press(30.0, 190.0))),
            ("allowed_release", HarnessOp::send_and_settle(&panel, &release(30.0, 190.0))),
        ])
        .expect("external inline-child lineage session");

    let log = match harness.log_tail(&panel, None, None) {
        LogTailResult::Ok { entries, .. } => entries,
        LogTailResult::Err { error } => panic!("log_tail on the loaded panel failed: {error}"),
    };
    let clicks = log
        .iter()
        .filter(|entry| entry.message.contains("widget button clicked") && entry.message.contains("widget=button"))
        .count();
    assert_eq!(clicks, 1, "lineage-addressed disable blocks the first click and re-enable permits the second");
}

/// A slider child spec for the declarative-children scenario: full `0..=255`
/// range, unit step, seeded at `initial`, default theme.
fn slider_spec(subname: &str, initial: f32) -> WidgetChildSpec {
    slider_spec_with_state(subname, initial, WidgetControlState::default())
}

fn slider_spec_with_state(subname: &str, initial: f32, state: WidgetControlState) -> WidgetChildSpec {
    WidgetChildSpec {
        subname: subname.to_owned(),
        kind: WidgetKind::Slider,
        origin: [0.0, 0.0],
        clip: None,
        config: SliderConfig { min: 0.0, max: 255.0, step: 1.0, initial, theme: Theme::DEFAULT, state }
            .encode_into_bytes(),
    }
}

fn button_spec(subname: &str, state: WidgetControlState) -> WidgetChildSpec {
    WidgetChildSpec {
        subname: subname.to_owned(),
        kind: WidgetKind::Button,
        origin: [0.0, 0.0],
        clip: None,
        config: ButtonConfig { label: "Run".to_owned(), theme: Theme::DEFAULT, state, ..ButtonConfig::default() }
            .encode_into_bytes(),
    }
}

fn radio_spec(subname: &str, state: WidgetControlState) -> WidgetChildSpec {
    WidgetChildSpec {
        subname: subname.to_owned(),
        kind: WidgetKind::Radio,
        origin: [0.0, 0.0],
        clip: None,
        config: RadioConfig {
            options: vec!["First".to_owned(), "Second".to_owned(), "Third".to_owned()],
            initial_index: 0,
            theme: Theme::DEFAULT,
            state,
        }
        .encode_into_bytes(),
    }
}

fn text_field_spec(subname: &str, initial: &str, state: WidgetControlState) -> WidgetChildSpec {
    WidgetChildSpec {
        subname: subname.to_owned(),
        kind: WidgetKind::TextField,
        origin: [0.0, 0.0],
        clip: None,
        config: TextFieldConfig { initial: initial.to_owned(), max_chars: 0, theme: Theme::DEFAULT, state }
            .encode_into_bytes(),
    }
}

/// Load the reference `WidgetPanel` root with an explicit `children` list (so
/// it stacks exactly those specs rather than its built-in reference stack) at
/// `(10, 10)` 200px wide, no font, default theme.
fn load_panel_with(harness: &mut SubstrateHarness, wasm: &[u8], children: Vec<WidgetChildSpec>) {
    let config = PanelConfig {
        x: 10.0,
        y: 10.0,
        width: 200.0,
        font_namespace: String::new(),
        font_path: String::new(),
        theme: Theme::DEFAULT,
        children,
        owns_input: true,
    };
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                "aether.component",
                &LoadComponent {
                    wasm: wasm.to_vec(),
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

/// The widget name a `widget slider changed` log line attributes its value to
/// (the `widget=NAME` field the panel logs) — `None` for a non-slider line.
fn slider_changed_widget(message: &str) -> Option<String> {
    message.split("widget=").nth(1).map(|rest| rest.split_whitespace().next().unwrap_or_default().to_owned())
}

/// The selected index from a `widget radio selected` log line.
fn radio_selected_index(message: &str) -> Option<u32> {
    message.split("index=").nth(1)?.split_whitespace().next()?.parse().ok()
}

fn virtual_list_selected_index(message: &str) -> Option<u32> {
    if !message.contains("widget virtual list selected") {
        return None;
    }
    message.split("selected_index=").nth(1)?.split_whitespace().next()?.parse().ok()
}

fn virtual_list_spec(subname: &str, state: WidgetControlState) -> WidgetChildSpec {
    WidgetChildSpec {
        subname: subname.to_owned(),
        kind: WidgetKind::VirtualList,
        origin: [0.0, 0.0],
        clip: None,
        config: VirtualListConfig {
            items: (0..200).map(|index| VirtualListRow::from(format!("Row {index:03}"))).collect(),
            initial_selected_index: Some(0),
            empty_text: String::new(),
            ruled: false,
            visible_row_count: 5,
            theme: Theme::DEFAULT,
            state,
            ..VirtualListConfig::default()
        }
        .encode_into_bytes(),
    }
}

/// A panel handed an explicit `children` list stacks exactly those widgets in
/// the declared order, and that order drives the focus (Tab) cycle — the
/// declarative-composition path the built-in reference stack can never
/// exercise (it only ever sees one fixed order). Two sliders named `first`
/// then `second`: from a fresh panel, Tab lands focus on `first` (focus
/// registration index 0), an arrow nudge commits and logs it, and a second
/// Tab-then-nudge logs `second`. The committed value-up events, read off the
/// log ring in arrival order, must spell out the declared order — a
/// spec→spawn dispatch fault or an order-derivation defect would reverse or
/// drop one.
#[test]
fn panel_stacks_declared_children_in_order() {
    let Some(wasm_path) = require_wasm("aether_kit_widget") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut harness = SubstrateHarness::builder().size(240, 220).with_component_host().build().expect("boot");
    load_panel_with(&mut harness, &wasm, vec![slider_spec("first", 40.0), slider_spec("second", 40.0)]);

    let panel = panel_address();
    harness
        .execute(vec![
            // First tick spawns + lays out the declared stack.
            ("spawn", HarnessOp::send_and_settle(&panel, &Tick::default())),
            // Tab from no focus lands on the first focusable child (index 0);
            // an arrow nudge on the focused slider commits + logs it.
            ("tab_first", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_TAB })),
            ("nudge_first", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_UP })),
            // Tab again advances to the second child; nudge + log it.
            ("tab_second", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_TAB })),
            ("nudge_second", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_UP })),
        ])
        .expect("declared-children session");

    let log = panel_log_messages(&mut harness);
    let joined = log.join("\n");
    let order: Vec<String> = log
        .iter()
        .filter(|m| m.contains("widget slider changed") && m.contains("committed=true"))
        .filter_map(|m| slider_changed_widget(m))
        .collect();
    assert_eq!(
        order,
        vec!["first".to_owned(), "second".to_owned()],
        "the declared child order must drive both the vertical stack and the Tab \
         focus cycle; committed slider events arrived as {order:?}; log was:\n{joined}",
    );
}

/// A five-row virtual viewport over 200 items must page and reveal through the
/// real panel routing path while read-only and disabled state block both input
/// lanes. The top-row click after two keyboard changes also proves hit testing
/// is relative to the realized window rather than the full item vector.
#[test]
fn virtual_list_pages_clicks_and_blocks_read_only_disabled_changes() {
    let Some(wasm_path) = require_wasm("aether_kit_widget") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut harness = SubstrateHarness::builder().size(240, 150).with_component_host().build().expect("boot");
    let read_only = WidgetControlState { read_only: true, ..WidgetControlState::default() };
    load_panel_with(&mut harness, &wasm, vec![virtual_list_spec("inventory", read_only)]);

    let panel = panel_address();
    let list = child_address("inventory");
    let disabled = WidgetControlState { enabled: false, ..WidgetControlState::default() };
    harness
        .execute(vec![
            ("spawn", HarnessOp::send_and_settle(&panel, &Tick::default())),
            ("focus_read_only", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_TAB })),
            (
                "blocked_read_only_page",
                HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_PAGE_DOWN }),
            ),
            ("blocked_read_only_press", HarnessOp::send_and_settle(&panel, &press(30.0, 118.0))),
            ("blocked_read_only_release", HarnessOp::send_and_settle(&panel, &release(30.0, 118.0))),
            (
                "make_mutable",
                HarnessOp::send_and_settle(&list, &SetWidgetState { state: WidgetControlState::default() }),
            ),
            ("page_to_five", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_PAGE_DOWN })),
            ("down_to_six", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_DOWN })),
            ("click_realized_top", HarnessOp::send_and_settle(&panel, &press(30.0, 22.0))),
            ("release_realized_top", HarnessOp::send_and_settle(&panel, &release(30.0, 22.0))),
            ("disable", HarnessOp::send_and_settle(&list, &SetWidgetState { state: disabled })),
            (
                "blocked_disabled_page",
                HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_PAGE_DOWN }),
            ),
            ("blocked_disabled_press", HarnessOp::send_and_settle(&panel, &press(30.0, 94.0))),
            ("blocked_disabled_release", HarnessOp::send_and_settle(&panel, &release(30.0, 94.0))),
            ("enable", HarnessOp::send_and_settle(&list, &SetWidgetState { state: WidgetControlState::default() })),
            ("refocus", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_TAB })),
            ("page_to_seven", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_PAGE_DOWN })),
        ])
        .expect("virtual-list state and selection session");

    let log = panel_log_messages(&mut harness);
    let selected_indices: Vec<u32> = log.iter().filter_map(|message| virtual_list_selected_index(message)).collect();
    assert_eq!(
        selected_indices,
        vec![5, 6, 2, 7],
        "only allowed actual changes should reach the attributed panel log; log was:\n{}",
        log.join("\n"),
    );
    assert!(
        log.iter()
            .filter(|message| message.contains("widget virtual list selected"))
            .all(|message| message.contains("widget=inventory")),
        "every virtual-list event must retain source attribution; log was:\n{}",
        log.join("\n"),
    );
}

fn drive_state_and_keyboard_session(harness: &mut SubstrateHarness) {
    let panel = panel_address();
    let value = child_address("value");
    let run = child_address("run");
    harness
        .execute(vec![
            ("spawn", HarnessOp::send_and_settle(&panel, &Tick::default())),
            // Forward Tab skips the disabled first slider and focuses the
            // read-only value. Its arrow input must not mutate.
            ("tab_value", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_TAB })),
            ("blocked_nudge", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_UP })),
            // Runtime state changes preserve the value while enabling mutation.
            (
                "make_mutable",
                HarnessOp::send_and_settle(&value, &SetWidgetState { state: WidgetControlState::default() }),
            ),
            ("allowed_nudge", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_UP })),
            // Shift+Tab wraps backward to the Button, skipping the disabled
            // first entry. Space fires on release.
            (
                "shift",
                HarnessOp::send_and_settle(
                    &panel,
                    &Modifiers { window: TEST_WINDOW_ID, shift: true, ..Modifiers::default() },
                ),
            ),
            ("reverse_tab", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_TAB })),
            ("space", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_SPACE })),
            (
                "space_release",
                HarnessOp::send_and_settle(&panel, &KeyRelease { window: TEST_WINDOW_ID, code: KEY_SPACE }),
            ),
            // Enter fires immediately and suppresses repeated key-down mail
            // until its matching release.
            ("enter", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_ENTER })),
            ("enter_repeat", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_ENTER })),
            (
                "enter_release",
                HarnessOp::send_and_settle(&panel, &KeyRelease { window: TEST_WINDOW_ID, code: KEY_ENTER }),
            ),
            // Hiding the focused button moves focus forward to the live slider;
            // no stale keyboard arm or focus remains on the button.
            (
                "hide_button",
                HarnessOp::send_and_settle(
                    &run,
                    &SetWidgetState { state: WidgetControlState { visible: false, ..WidgetControlState::default() } },
                ),
            ),
            ("nudge_after_hide", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_UP })),
        ])
        .expect("state and keyboard session");
}

/// Initial and runtime control state must agree with panel routing: unavailable
/// children leave their layout slot but exit the focus ring, read-only values
/// focus without mutation, reverse Tab skips unavailable entries, and keyboard
/// activation fires Button exactly once per key pair.
#[test]
fn panel_routes_availability_read_only_reverse_tab_and_button_keys() {
    let Some(wasm_path) = require_wasm("aether_kit_widget") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut harness = SubstrateHarness::builder().size(240, 140).with_component_host().build().expect("boot");

    let disabled = WidgetControlState { enabled: false, ..WidgetControlState::default() };
    let read_only = WidgetControlState { read_only: true, ..WidgetControlState::default() };
    load_panel_with(
        &mut harness,
        &wasm,
        vec![
            slider_spec_with_state("disabled", 40.0, disabled),
            slider_spec_with_state("value", 40.0, read_only),
            button_spec("run", WidgetControlState::default()),
        ],
    );

    drive_state_and_keyboard_session(&mut harness);

    let log = panel_log_messages(&mut harness);
    let joined = log.join("\n");
    let value_changes = log
        .iter()
        .filter(|message| {
            message.contains("widget slider changed")
                && message.contains("widget=value")
                && message.contains("committed=true")
        })
        .count();
    assert_eq!(
        value_changes, 2,
        "the read-only nudge is blocked, then mutable and post-hide nudges commit; log was:\n{joined}",
    );
    let clicks = log
        .iter()
        .filter(|message| message.contains("widget button clicked") && message.contains("widget=run"))
        .count();
    assert_eq!(clicks, 2, "Space release and the first Enter press click exactly once each; log was:\n{joined}");
}

/// Read-only must block both of Radio's value-changing paths. Re-enabling the
/// same actor makes keyboard and pointer selection live, proving the negative
/// phase was neither unrouted input nor an unobserved child event.
#[test]
fn read_only_radio_blocks_pointer_and_keyboard_until_enabled() {
    let Some(wasm_path) = require_wasm("aether_kit_widget") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut harness = SubstrateHarness::builder().size(240, 100).with_component_host().build().expect("boot");
    let read_only = WidgetControlState { read_only: true, ..WidgetControlState::default() };
    load_panel_with(&mut harness, &wasm, vec![radio_spec("choice", read_only)]);

    let panel = panel_address();
    harness
        .execute(vec![
            ("spawn", HarnessOp::send_and_settle(&panel, &Tick::default())),
            ("focus", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_TAB })),
            ("blocked_key", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_DOWN })),
            ("blocked_pointer", HarnessOp::send_and_settle(&panel, &press(30.0, 70.0))),
            ("blocked_pointer_release", HarnessOp::send_and_settle(&panel, &release(30.0, 70.0))),
            (
                "enable",
                HarnessOp::send_and_settle(
                    child_address("choice"),
                    &SetWidgetState { state: WidgetControlState::default() },
                ),
            ),
            ("allowed_key", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_DOWN })),
            ("allowed_pointer", HarnessOp::send_and_settle(&panel, &press(30.0, 70.0))),
            ("allowed_pointer_release", HarnessOp::send_and_settle(&panel, &release(30.0, 70.0))),
        ])
        .expect("read-only radio session");

    let log = panel_log_messages(&mut harness);
    let joined = log.join("\n");
    let selections: Vec<u32> = log
        .iter()
        .filter(|message| message.contains("widget radio selected") && message.contains("widget=choice"))
        .map(|message| radio_selected_index(message).expect("radio log line carries an index"))
        .collect();
    assert_eq!(
        selections,
        vec![1, 2],
        "read-only key/click input must emit nothing or alter the later enabled indexes; log was:\n{joined}",
    );
}

/// Arm, disable, and re-enable Button before its decisive stale release.
fn drive_button_cancellation_session(harness: &mut SubstrateHarness) {
    let run = child_address("run");
    let unavailable = WidgetControlState { enabled: false, ..WidgetControlState::default() };
    harness
        .execute(vec![
            // Address the live child directly so focus loss cannot mask a
            // failure to clear its pointer arm on the state transition.
            ("arm_button", HarnessOp::send_and_settle(&run, &press(30.0, 22.0))),
            ("disable_button", HarnessOp::send_and_settle(&run, &SetWidgetState { state: unavailable })),
            (
                "enable_button",
                HarnessOp::send_and_settle(&run, &SetWidgetState { state: WidgetControlState::default() }),
            ),
            ("stale_button_release", HarnessOp::send_and_settle(&run, &release(30.0, 22.0))),
            ("live_button_press", HarnessOp::send_and_settle(&run, &press(30.0, 22.0))),
            ("live_button_release", HarnessOp::send_and_settle(&run, &release(30.0, 22.0))),
        ])
        .expect("button state cancellation session");
}

fn drive_slider_cancellation_session(harness: &mut SubstrateHarness) {
    let panel = panel_address();
    let value = child_address("value");
    let read_only = WidgetControlState { read_only: true, ..WidgetControlState::default() };
    harness
        .execute(vec![
            // Read-only leaves root capture intact. Re-enable before moving;
            // only clearing Slider's own drag state prevents stale values.
            ("begin_drag", HarnessOp::send_and_settle(&panel, &press(60.0, 52.0))),
            ("make_slider_read_only", HarnessOp::send_and_settle(&value, &SetWidgetState { state: read_only })),
            (
                "enable_slider",
                HarnessOp::send_and_settle(&value, &SetWidgetState { state: WidgetControlState::default() }),
            ),
            (
                "stale_drag_move",
                HarnessOp::send_and_settle(&panel, &MouseMove { window: TEST_WINDOW_ID, x: 160.0, y: 52.0 }),
            ),
            ("stale_drag_release", HarnessOp::send_and_settle(&panel, &release(160.0, 52.0))),
            ("live_drag_press", HarnessOp::send_and_settle(&panel, &press(110.0, 52.0))),
            (
                "live_drag_move",
                HarnessOp::send_and_settle(&panel, &MouseMove { window: TEST_WINDOW_ID, x: 160.0, y: 52.0 }),
            ),
            ("live_drag_release", HarnessOp::send_and_settle(&panel, &release(160.0, 52.0))),
        ])
        .expect("slider state cancellation session");
}

/// A state change cancels child-owned transient input, not merely root
/// routing. Each actor is re-enabled before the stale release/move so failure
/// to clear its internal arm/drag would create an observable extra event.
#[test]
fn live_state_changes_cancel_button_arm_and_slider_drag() {
    let Some(wasm_path) = require_wasm("aether_kit_widget") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut harness = SubstrateHarness::builder().size(240, 90).with_component_host().build().expect("boot");
    load_panel_with(
        &mut harness,
        &wasm,
        vec![button_spec("run", WidgetControlState::default()), slider_spec("value", 0.0)],
    );

    let panel = panel_address();
    harness.execute(vec![("spawn", HarnessOp::send_and_settle(&panel, &Tick::default()))]).expect("spawn widget set");
    drive_button_cancellation_session(&mut harness);
    drive_slider_cancellation_session(&mut harness);

    let log = panel_log_messages(&mut harness);
    let joined = log.join("\n");
    let clicks = log
        .iter()
        .filter(|message| message.contains("widget button clicked") && message.contains("widget=run"))
        .count();
    assert_eq!(
        clicks, 1,
        "the stale release must not click; the enabled positive control clicks once; log was:\n{joined}",
    );

    let slider_changes: Vec<_> = log
        .iter()
        .filter(|message| message.contains("widget slider changed") && message.contains("widget=value"))
        .collect();
    let committed = slider_changes.iter().filter(|message| message.contains("committed=true")).count();
    assert_eq!(
        slider_changes.len(),
        4,
        "the cancelled drag emits only its initial press; the live drag emits press/move/release; log was:\n{joined}",
    );
    assert_eq!(committed, 1, "only the enabled positive-control drag may commit; log was:\n{joined}");
}

/// A read-only text field remains focusable but cannot commit. Enabling the
/// same live actor then permits exactly one commit, proving the negative path
/// is not an input-routing or log-observation false positive.
#[test]
fn read_only_text_field_blocks_activation_until_enabled() {
    let Some(wasm_path) = require_wasm("aether_kit_widget") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut harness = SubstrateHarness::builder().size(240, 80).with_component_host().build().expect("boot");
    let read_only = WidgetControlState { read_only: true, ..WidgetControlState::default() };
    load_panel_with(&mut harness, &wasm, vec![text_field_spec("locked", "locked", read_only)]);

    let panel = panel_address();
    harness
        .execute(vec![
            ("spawn", HarnessOp::send_and_settle(&panel, &Tick::default())),
            ("focus", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_TAB })),
            (
                "blocked_text",
                HarnessOp::send_and_settle(&panel, &TextInput { window: TEST_WINDOW_ID, text: " mutation".to_owned() }),
            ),
            ("blocked_enter", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_ENTER })),
            (
                "enable",
                HarnessOp::send_and_settle(
                    child_address("locked"),
                    &SetWidgetState { state: WidgetControlState::default() },
                ),
            ),
            ("allowed_enter", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_ENTER })),
        ])
        .expect("read-only text activation session");

    let log = panel_log_messages(&mut harness);
    let joined = log.join("\n");
    let commits: Vec<_> = log
        .iter()
        .filter(|message| message.contains("widget text committed") && message.contains("widget=locked"))
        .collect();
    assert_eq!(
        commits.len(),
        1,
        "read-only Enter must not commit, while enabled Enter commits once; log was:\n{joined}",
    );
    assert!(
        commits[0].contains("text=locked"),
        "blocked read-only TextInput must not alter the later committed value; log was:\n{joined}",
    );
}
