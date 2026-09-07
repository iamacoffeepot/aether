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
use aether_kinds::keycode::{
    KEY_A, KEY_DOWN, KEY_ENTER, KEY_HOME, KEY_PAGE_DOWN, KEY_RIGHT, KEY_SPACE, KEY_TAB, KEY_UP,
};
use aether_kinds::mouse_button::LEFT;
use aether_kinds::{
    Key, KeyRelease, LoadComponent, LoadResult, LogTailResult, Modifiers, MouseButton, MouseButtonRelease, MouseMove,
    TextInput, Tick, WindowId,
};
use aether_kit_widget::{
    BehaviorHostSpec, ButtonConfig, PanelConfig, RadioConfig, ScriptRef, SetWidgetState, SliderConfig, TextFieldConfig,
    Theme, VirtualListConfig, VirtualListRow, WidgetChildSpec, WidgetControlState, WidgetKind,
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

fn populated_rows() -> Vec<VirtualListRow> {
    vec![VirtualListRow::from("Alpha"), VirtualListRow::from("Beta"), VirtualListRow::from("Gamma")]
}

fn live_list_config(
    items: Vec<VirtualListRow>,
    initial_selected_index: Option<u32>,
    state: WidgetControlState,
) -> VirtualListConfig {
    VirtualListConfig {
        items,
        initial_selected_index,
        visible_row_count: 5,
        theme: Theme::DEFAULT,
        state,
        ..VirtualListConfig::default()
    }
}

fn live_list_spec(
    subname: &str,
    items: Vec<VirtualListRow>,
    initial_selected_index: Option<u32>,
    state: WidgetControlState,
) -> WidgetChildSpec {
    WidgetChildSpec {
        subname: subname.to_owned(),
        kind: WidgetKind::VirtualList,
        origin: [0.0, 0.0],
        clip: None,
        config: live_list_config(items, initial_selected_index, state).encode_into_bytes(),
    }
}

fn behavior_host_list_spec(
    subname: &str,
    items: Vec<VirtualListRow>,
    initial_selected_index: Option<u32>,
    state: WidgetControlState,
) -> WidgetChildSpec {
    WidgetChildSpec {
        subname: subname.to_owned(),
        kind: WidgetKind::BehaviorHost,
        origin: [0.0, 0.0],
        clip: None,
        config: BehaviorHostSpec {
            wrapped: WidgetKind::VirtualList,
            wrapped_config: live_list_config(items, initial_selected_index, state).encode_into_bytes(),
            script: ScriptRef::None,
            fuel_per_call: 0,
            disable_after_traps: 0,
        }
        .encode_into_bytes(),
    }
}

fn field<'a>(message: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key}=");
    message.split_whitespace().find_map(|token| token.strip_prefix(&prefix))
}

fn virtual_list_hovers(log: &[String]) -> Vec<(Option<&str>, Option<&str>)> {
    log.iter()
        .filter(|message| message.contains("widget virtual list hover"))
        .map(|message| (field(message, "widget"), field(message, "row")))
        .collect()
}

fn virtual_list_selections(log: &[String]) -> Vec<(Option<&str>, u32)> {
    log.iter()
        .filter(|message| message.contains("widget virtual list selected"))
        .map(|message| {
            (field(message, "widget"), virtual_list_selected_index(message).expect("selection log carries an index"))
        })
        .collect()
}

fn button_click_widgets(log: &[String]) -> Vec<&str> {
    log.iter()
        .filter(|message| message.contains("widget button clicked"))
        .filter_map(|message| field(message, "widget"))
        .collect()
}

fn take_log_delta(harness: &mut SubstrateHarness, cursor: &mut usize) -> Vec<String> {
    let log = panel_log_messages(harness);
    let delta = log
        .get(*cursor..)
        .expect("take_log_delta cursor exceeds current panel log length; history was lost")
        .to_vec();
    *cursor = log.len();
    delta
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

/// A focused three-option radio must move on actual panel-routed Up/Down and
/// stay put at the ends. The unit `step` helper already clamps; this scenario
/// is the production path those helper tests cannot see: Tick/Tab, then Up at
/// index 0, Down to 1, Down to 2, Down at 2, Up to 1, Up to 0, Up at 0.
/// Endpoint keys must not log a selection, and the ordered events must be
/// exactly `[1, 2, 1, 0]`.
#[test]
fn radio_up_down_clamps_at_the_ends_without_endpoint_events() {
    let Some(wasm_path) = require_wasm("aether_kit_widget") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut harness = SubstrateHarness::builder().size(240, 100).with_component_host().build().expect("boot");
    load_panel_with(&mut harness, &wasm, vec![radio_spec("choice", WidgetControlState::default())]);

    let panel = panel_address();
    let choice_selections = |harness: &mut SubstrateHarness| -> (Vec<u32>, String) {
        let log = panel_log_messages(harness);
        let joined = log.join("\n");
        let selections = log
            .iter()
            .filter(|message| message.contains("widget radio selected") && message.contains("widget=choice"))
            .map(|message| radio_selected_index(message).expect("radio log line carries an index"))
            .collect();
        (selections, joined)
    };

    harness
        .execute(vec![
            ("spawn", HarnessOp::send_and_settle(&panel, &Tick::default())),
            ("focus", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_TAB })),
            ("up_at_top", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_UP })),
        ])
        .expect("radio focus and top-end Up");

    let (selections, joined) = choice_selections(&mut harness);
    assert!(selections.is_empty(), "Up at the first option must emit no selection event; log was:\n{joined}");

    harness
        .execute(vec![
            ("down_to_1", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_DOWN })),
            ("down_to_2", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_DOWN })),
            ("down_at_bottom", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_DOWN })),
        ])
        .expect("radio Down through the last option");

    let (selections, joined) = choice_selections(&mut harness);
    assert_eq!(
        selections,
        vec![1, 2],
        "Down must select 1 then 2 and emit nothing at the last option; log was:\n{joined}",
    );

    harness
        .execute(vec![
            ("up_to_1", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_UP })),
            ("up_to_0", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_UP })),
            ("up_at_top", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_UP })),
        ])
        .expect("radio Up through the first option");

    let (selections, joined) = choice_selections(&mut harness);
    assert_eq!(
        selections,
        vec![1, 2, 1, 0],
        "Up must select 1 then 0 and emit nothing at the first option; log was:\n{joined}",
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
    assert_eq!(
        commits[0].as_str(),
        "widget=locked text=locked widget text committed",
        "blocked read-only TextInput must not alter the later committed value; log was:\n{joined}",
    );
}

/// A field that loses focus while Shift is held must not keep that Shift after
/// the release is delivered to another field and Tab returns. Home at caret 0,
/// then Right + insert, must yield `axb`; a stale Shift would replace `a`.
#[test]
fn tab_cycle_does_not_leave_stale_shift_on_refocused_field() {
    let Some(wasm_path) = require_wasm("aether_kit_widget") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut harness = SubstrateHarness::builder().size(240, 80).with_component_host().build().expect("boot");
    load_panel_with(
        &mut harness,
        &wasm,
        vec![
            text_field_spec("first", "ab", WidgetControlState::default()),
            text_field_spec("second", "", WidgetControlState::default()),
        ],
    );

    let panel = panel_address();
    harness
        .execute(vec![
            ("spawn", HarnessOp::send_and_settle(&panel, &Tick::default())),
            ("focus_first", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_TAB })),
            ("home", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_HOME })),
            (
                "shift_down",
                HarnessOp::send_and_settle(
                    &panel,
                    &Modifiers { window: TEST_WINDOW_ID, shift: true, ..Modifiers::default() },
                ),
            ),
            ("reverse_tab", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_TAB })),
            (
                "shift_up",
                HarnessOp::send_and_settle(&panel, &Modifiers { window: TEST_WINDOW_ID, ..Modifiers::default() }),
            ),
            ("refocus_first", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_TAB })),
            ("right", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_RIGHT })),
            ("type", HarnessOp::send_and_settle(&panel, &TextInput { window: TEST_WINDOW_ID, text: "x".to_owned() })),
            ("commit", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_ENTER })),
        ])
        .expect("stale-shift tab cycle session");

    let log = panel_log_messages(&mut harness);
    let joined = log.join("\n");
    let commits: Vec<_> =
        log.iter().filter(|message| message.contains("widget text committed")).map(String::as_str).collect();
    assert_eq!(
        commits,
        ["widget=first text=axb widget text committed"],
        "Right after refocus must insert at caret 1, not replace a Shift-selected `a`; log was:\n{joined}",
    );
}

/// Ctrl already held on the panel must reach a never-focused field on pointer
/// gain so Ctrl+A then a replacement insert replaces the whole prior value.
#[test]
fn pointer_focus_inherits_already_held_ctrl() {
    let Some(wasm_path) = require_wasm("aether_kit_widget") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut harness = SubstrateHarness::builder().size(240, 80).with_component_host().build().expect("boot");
    load_panel_with(&mut harness, &wasm, vec![text_field_spec("field", "prior", WidgetControlState::default())]);

    let panel = panel_address();
    harness
        .execute(vec![
            ("spawn", HarnessOp::send_and_settle(&panel, &Tick::default())),
            (
                "ctrl_down",
                HarnessOp::send_and_settle(
                    &panel,
                    &Modifiers { window: TEST_WINDOW_ID, ctrl: true, ..Modifiers::default() },
                ),
            ),
            ("focus_press", HarnessOp::send_and_settle(&panel, &press(50.0, 22.0))),
            ("focus_release", HarnessOp::send_and_settle(&panel, &release(50.0, 22.0))),
            ("select_all", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_A })),
            (
                "replace",
                HarnessOp::send_and_settle(&panel, &TextInput { window: TEST_WINDOW_ID, text: "new".to_owned() }),
            ),
            ("commit", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_ENTER })),
        ])
        .expect("pointer-gain ctrl session");

    let log = panel_log_messages(&mut harness);
    let joined = log.join("\n");
    let commits: Vec<_> =
        log.iter().filter(|message| message.contains("widget text committed")).map(String::as_str).collect();
    assert_eq!(
        commits,
        ["widget=field text=new widget text committed"],
        "Ctrl held before pointer focus must SelectAll so the insert replaces `prior`; log was:\n{joined}",
    );
}

/// Hiding the focused field while Ctrl is held must hand that chord to the
/// next available field so `SelectAll` + replacement works without a later Tab.
#[test]
fn availability_focus_move_inherits_already_held_ctrl() {
    let Some(wasm_path) = require_wasm("aether_kit_widget") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut harness = SubstrateHarness::builder().size(240, 80).with_component_host().build().expect("boot");
    load_panel_with(
        &mut harness,
        &wasm,
        vec![
            text_field_spec("first", "gone", WidgetControlState::default()),
            text_field_spec("second", "keep", WidgetControlState::default()),
        ],
    );

    let panel = panel_address();
    harness
        .execute(vec![
            ("spawn", HarnessOp::send_and_settle(&panel, &Tick::default())),
            ("focus_first", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_TAB })),
            (
                "ctrl_down",
                HarnessOp::send_and_settle(
                    &panel,
                    &Modifiers { window: TEST_WINDOW_ID, ctrl: true, ..Modifiers::default() },
                ),
            ),
            (
                "hide_first",
                HarnessOp::send_and_settle(
                    child_address("first"),
                    &SetWidgetState { state: WidgetControlState { visible: false, ..WidgetControlState::default() } },
                ),
            ),
            ("select_all", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_A })),
            (
                "replace",
                HarnessOp::send_and_settle(&panel, &TextInput { window: TEST_WINDOW_ID, text: "new".to_owned() }),
            ),
            ("commit", HarnessOp::send_and_settle(&panel, &Key { window: TEST_WINDOW_ID, code: KEY_ENTER })),
        ])
        .expect("availability-gain ctrl session");

    let log = panel_log_messages(&mut harness);
    let joined = log.join("\n");
    let commits: Vec<_> =
        log.iter().filter(|message| message.contains("widget text committed")).map(String::as_str).collect();
    assert_eq!(
        commits,
        ["widget=second text=new widget text committed"],
        "hiding the focused field must inherit Ctrl so SelectAll replaces `keep`; log was:\n{joined}",
    );
}

fn space() -> Key {
    Key { window: TEST_WINDOW_ID, code: KEY_SPACE }
}

fn space_up() -> KeyRelease {
    KeyRelease { window: TEST_WINDOW_ID, code: KEY_SPACE }
}

fn tab() -> Key {
    Key { window: TEST_WINDOW_ID, code: KEY_TAB }
}

fn down() -> Key {
    Key { window: TEST_WINDOW_ID, code: KEY_DOWN }
}

fn hover_at(x: f32, y: f32) -> MouseMove {
    MouseMove { window: TEST_WINDOW_ID, x, y }
}

fn hover_row_zero() -> MouseMove {
    hover_at(30.0, 22.0)
}

fn empty_list_config() -> VirtualListConfig {
    live_list_config(Vec::new(), None, WidgetControlState::default())
}

fn populated_list_config(initial_selected_index: Option<u32>) -> VirtualListConfig {
    live_list_config(populated_rows(), initial_selected_index, WidgetControlState::default())
}

fn assert_list_phase(
    phase: &str,
    delta: &[String],
    hovers: &[(Option<&str>, Option<&str>)],
    selections: &[(Option<&str>, u32)],
    buttons: &[&str],
) {
    let joined = delta.join("\n");
    assert_eq!(virtual_list_hovers(delta), hovers, "{phase} hover; log was:\n{joined}");
    assert_eq!(virtual_list_selections(delta), selections, "{phase} selection; log was:\n{joined}");
    assert_eq!(button_click_widgets(delta), buttons, "{phase} button; log was:\n{joined}");
}

/// An initially empty positive-height list must not take pointer or Tab until a
/// re-sent config populates it; the sibling button is the empty-phase control.
#[test]
fn empty_virtual_list_becomes_eligible_when_populated() {
    let Some(wasm_path) = require_wasm("aether_kit_widget") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut harness = SubstrateHarness::builder().size(240, 180).with_component_host().build().expect("boot");
    load_panel_with(
        &mut harness,
        &wasm,
        vec![
            live_list_spec("inventory", Vec::new(), None, WidgetControlState::default()),
            button_spec("run", WidgetControlState::default()),
        ],
    );
    let panel = panel_address();
    let list = child_address("inventory");
    let mut cursor = 0;

    harness
        .execute(vec![
            ("spawn", HarnessOp::send_and_settle(&panel, &Tick::default())),
            ("empty_hover", HarnessOp::send_and_settle(&panel, &hover_row_zero())),
            ("empty_press", HarnessOp::send_and_settle(&panel, &press(30.0, 22.0))),
            ("empty_release", HarnessOp::send_and_settle(&panel, &release(30.0, 22.0))),
            ("empty_tab", HarnessOp::send_and_settle(&panel, &tab())),
            ("empty_space", HarnessOp::send_and_settle(&panel, &space())),
            ("empty_space_up", HarnessOp::send_and_settle(&panel, &space_up())),
        ])
        .expect("empty list baseline");
    assert_list_phase("empty baseline", &take_log_delta(&mut harness, &mut cursor), &[], &[], &["run"]);

    harness
        .execute(vec![
            ("unchanged_empty", HarnessOp::send_and_settle(&list, &empty_list_config())),
            ("unchanged_empty_hover", HarnessOp::send_and_settle(&panel, &hover_row_zero())),
            ("unchanged_empty_press", HarnessOp::send_and_settle(&panel, &press(30.0, 22.0))),
            ("unchanged_empty_release", HarnessOp::send_and_settle(&panel, &release(30.0, 22.0))),
        ])
        .expect("unchanged empty config");
    assert_list_phase("unchanged empty config", &take_log_delta(&mut harness, &mut cursor), &[], &[], &[]);

    harness
        .execute(vec![
            ("populate", HarnessOp::send_and_settle(&list, &populated_list_config(Some(0)))),
            ("hover", HarnessOp::send_and_settle(&panel, &hover_row_zero())),
        ])
        .expect("populate hover");
    assert_list_phase(
        "populate hover",
        &take_log_delta(&mut harness, &mut cursor),
        &[(Some("inventory"), Some("0"))],
        &[],
        &[],
    );

    harness
        .execute(vec![
            ("tab_to_list", HarnessOp::send_and_settle(&panel, &tab())),
            ("down", HarnessOp::send_and_settle(&panel, &down())),
        ])
        .expect("tab and down");
    assert_list_phase("tab+down", &take_log_delta(&mut harness, &mut cursor), &[], &[(Some("inventory"), 1)], &[]);

    harness
        .execute(vec![
            ("unchanged_live", HarnessOp::send_and_settle(&list, &populated_list_config(Some(0)))),
            ("down_without_tab", HarnessOp::send_and_settle(&panel, &down())),
        ])
        .expect("unchanged live config");
    assert_list_phase(
        "unchanged live config keeps focus",
        &take_log_delta(&mut harness, &mut cursor),
        &[],
        &[(Some("inventory"), 1)],
        &[],
    );

    harness
        .execute(vec![
            ("click_row_two", HarnessOp::send_and_settle(&panel, &press(30.0, 70.0))),
            ("click_row_two_up", HarnessOp::send_and_settle(&panel, &release(30.0, 70.0))),
        ])
        .expect("row click");
    assert_list_phase(
        "click row 2",
        &take_log_delta(&mut harness, &mut cursor),
        &[(Some("inventory"), Some("2"))],
        &[(Some("inventory"), 2)],
        &[],
    );
}

/// Emptying a focused, captured list must drop routing and activation so the
/// sibling can take keyboard and pointer; repopulating must not resurrect the
/// old capture or armed press.
#[test]
fn emptying_a_live_virtual_list_drops_routing_and_does_not_rearm() {
    let Some(wasm_path) = require_wasm("aether_kit_widget") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut harness = SubstrateHarness::builder().size(240, 180).with_component_host().build().expect("boot");
    load_panel_with(
        &mut harness,
        &wasm,
        vec![
            live_list_spec("inventory", populated_rows(), Some(0), WidgetControlState::default()),
            button_spec("run", WidgetControlState::default()),
        ],
    );
    let panel = panel_address();
    let list = child_address("inventory");
    let mut cursor = 0;

    harness
        .execute(vec![
            ("spawn", HarnessOp::send_and_settle(&panel, &Tick::default())),
            ("hover", HarnessOp::send_and_settle(&panel, &hover_row_zero())),
            ("focus", HarnessOp::send_and_settle(&panel, &tab())),
            ("select_one", HarnessOp::send_and_settle(&panel, &down())),
            ("press_capture", HarnessOp::send_and_settle(&panel, &press(30.0, 22.0))),
        ])
        .expect("arm list");
    assert_list_phase(
        "arm",
        &take_log_delta(&mut harness, &mut cursor),
        &[(Some("inventory"), Some("0"))],
        &[(Some("inventory"), 1), (Some("inventory"), 0)],
        &[],
    );

    harness.execute(vec![("empty", HarnessOp::send_and_settle(&list, &empty_list_config()))]).expect("empty the list");
    assert_list_phase("empty", &take_log_delta(&mut harness, &mut cursor), &[(Some("inventory"), None)], &[], &[]);

    harness
        .execute(vec![
            ("sibling_space", HarnessOp::send_and_settle(&panel, &space())),
            ("sibling_space_up", HarnessOp::send_and_settle(&panel, &space_up())),
        ])
        .expect("sibling keyboard while empty");
    assert_list_phase("sibling keyboard while empty", &take_log_delta(&mut harness, &mut cursor), &[], &[], &["run"]);

    harness
        .execute(vec![
            ("populate", HarnessOp::send_and_settle(&list, &populated_list_config(Some(0)))),
            ("stale_release", HarnessOp::send_and_settle(&panel, &release(30.0, 22.0))),
            ("stale_move", HarnessOp::send_and_settle(&panel, &hover_at(30.0, 148.0))),
        ])
        .expect("stale release and move after repopulate");
    assert_list_phase(
        "stale release/move before any fresh press",
        &take_log_delta(&mut harness, &mut cursor),
        &[],
        &[],
        &[],
    );

    harness
        .execute(vec![
            ("sibling_press", HarnessOp::send_and_settle(&panel, &press(30.0, 148.0))),
            ("sibling_release", HarnessOp::send_and_settle(&panel, &release(30.0, 148.0))),
        ])
        .expect("sibling pointer");
    assert_list_phase("sibling pointer", &take_log_delta(&mut harness, &mut cursor), &[], &[], &["run"]);

    harness
        .execute(vec![
            ("hover", HarnessOp::send_and_settle(&panel, &hover_row_zero())),
            ("fresh_press", HarnessOp::send_and_settle(&panel, &press(30.0, 70.0))),
            ("fresh_release", HarnessOp::send_and_settle(&panel, &release(30.0, 70.0))),
        ])
        .expect("fresh list input");
    assert_list_phase(
        "fresh list input",
        &take_log_delta(&mut harness, &mut cursor),
        &[(Some("inventory"), Some("0")), (Some("inventory"), Some("2"))],
        &[(Some("inventory"), 2)],
        &[],
    );
}

/// Populating a disabled or hidden list must not enter routing until an
/// explicit state enable; the sibling button is the positive control.
#[test]
fn populating_disabled_or_hidden_virtual_list_stays_out_of_routing() {
    let Some(wasm_path) = require_wasm("aether_kit_widget") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut harness = SubstrateHarness::builder().size(240, 320).with_component_host().build().expect("boot");
    let disabled = WidgetControlState { enabled: false, ..WidgetControlState::default() };
    let hidden = WidgetControlState { visible: false, ..WidgetControlState::default() };
    load_panel_with(
        &mut harness,
        &wasm,
        vec![
            live_list_spec("blocked", Vec::new(), None, disabled.clone()),
            live_list_spec("ghost", Vec::new(), None, hidden.clone()),
            button_spec("run", WidgetControlState::default()),
        ],
    );
    let panel = panel_address();
    let mut cursor = 0;

    harness
        .execute(vec![
            ("spawn", HarnessOp::send_and_settle(&panel, &Tick::default())),
            (
                "populate_blocked",
                HarnessOp::send_and_settle(
                    child_address("blocked"),
                    &live_list_config(populated_rows(), Some(0), disabled),
                ),
            ),
            (
                "populate_ghost",
                HarnessOp::send_and_settle(
                    child_address("ghost"),
                    &live_list_config(populated_rows(), Some(0), hidden),
                ),
            ),
            ("blocked_hover", HarnessOp::send_and_settle(&panel, &hover_row_zero())),
            ("blocked_press", HarnessOp::send_and_settle(&panel, &press(30.0, 22.0))),
            ("blocked_release", HarnessOp::send_and_settle(&panel, &release(30.0, 22.0))),
            ("ghost_hover", HarnessOp::send_and_settle(&panel, &hover_at(30.0, 148.0))),
            ("ghost_press", HarnessOp::send_and_settle(&panel, &press(30.0, 148.0))),
            ("ghost_release", HarnessOp::send_and_settle(&panel, &release(30.0, 148.0))),
            ("tab", HarnessOp::send_and_settle(&panel, &tab())),
            ("space", HarnessOp::send_and_settle(&panel, &space())),
            ("space_up", HarnessOp::send_and_settle(&panel, &space_up())),
        ])
        .expect("unavailable populate session");
    assert_list_phase("unavailable populate", &take_log_delta(&mut harness, &mut cursor), &[], &[], &["run"]);

    harness
        .execute(vec![
            (
                "enable_blocked",
                HarnessOp::send_and_settle(
                    child_address("blocked"),
                    &SetWidgetState { state: WidgetControlState::default() },
                ),
            ),
            ("blocked_hover_live", HarnessOp::send_and_settle(&panel, &hover_row_zero())),
            ("tab_blocked", HarnessOp::send_and_settle(&panel, &tab())),
            ("down_blocked", HarnessOp::send_and_settle(&panel, &down())),
        ])
        .expect("enable blocked");
    assert_list_phase(
        "enable blocked",
        &take_log_delta(&mut harness, &mut cursor),
        &[(Some("blocked"), Some("0"))],
        &[(Some("blocked"), 1)],
        &[],
    );

    harness
        .execute(vec![
            (
                "show_ghost",
                HarnessOp::send_and_settle(
                    child_address("ghost"),
                    &SetWidgetState { state: WidgetControlState::default() },
                ),
            ),
            ("ghost_hover_live", HarnessOp::send_and_settle(&panel, &hover_at(30.0, 148.0))),
            ("ghost_press_live", HarnessOp::send_and_settle(&panel, &press(30.0, 172.0))),
            ("ghost_release_live", HarnessOp::send_and_settle(&panel, &release(30.0, 172.0))),
        ])
        .expect("show ghost");
    assert_list_phase(
        "show ghost",
        &take_log_delta(&mut harness, &mut cursor),
        &[(Some("blocked"), None), (Some("ghost"), Some("0")), (Some("ghost"), Some("1"))],
        &[(Some("ghost"), 1)],
        &[],
    );
}

/// A read-only populated list stays hoverable and focusable but rejects
/// selection mutation until an explicit mutable state update.
#[test]
fn read_only_populated_virtual_list_hovers_and_focuses_without_mutating() {
    let Some(wasm_path) = require_wasm("aether_kit_widget") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut harness = SubstrateHarness::builder().size(240, 180).with_component_host().build().expect("boot");
    let read_only = WidgetControlState { read_only: true, ..WidgetControlState::default() };
    load_panel_with(
        &mut harness,
        &wasm,
        vec![
            live_list_spec("inventory", Vec::new(), None, read_only.clone()),
            button_spec("run", WidgetControlState::default()),
        ],
    );
    let panel = panel_address();
    let list = child_address("inventory");
    let mut cursor = 0;

    harness
        .execute(vec![
            ("spawn", HarnessOp::send_and_settle(&panel, &Tick::default())),
            ("populate", HarnessOp::send_and_settle(&list, &live_list_config(populated_rows(), Some(0), read_only))),
            ("hover", HarnessOp::send_and_settle(&panel, &hover_row_zero())),
            ("focus", HarnessOp::send_and_settle(&panel, &tab())),
            ("blocked_down", HarnessOp::send_and_settle(&panel, &down())),
            ("blocked_press", HarnessOp::send_and_settle(&panel, &press(30.0, 70.0))),
            ("blocked_release", HarnessOp::send_and_settle(&panel, &release(30.0, 70.0))),
        ])
        .expect("read-only input");
    assert_list_phase(
        "read-only hover/focus/click",
        &take_log_delta(&mut harness, &mut cursor),
        &[(Some("inventory"), Some("0")), (Some("inventory"), Some("2"))],
        &[],
        &[],
    );

    harness
        .execute(vec![
            ("tab_button", HarnessOp::send_and_settle(&panel, &tab())),
            ("space", HarnessOp::send_and_settle(&panel, &space())),
            ("space_up", HarnessOp::send_and_settle(&panel, &space_up())),
        ])
        .expect("tab away");
    assert_list_phase(
        "tab away does not emit HoverLost",
        &take_log_delta(&mut harness, &mut cursor),
        &[],
        &[],
        &["run"],
    );

    harness
        .execute(vec![
            ("enable", HarnessOp::send_and_settle(&list, &SetWidgetState { state: WidgetControlState::default() })),
            ("tab_list", HarnessOp::send_and_settle(&panel, &tab())),
            ("allowed_down", HarnessOp::send_and_settle(&panel, &down())),
        ])
        .expect("mutable down");
    assert_list_phase("mutable down", &take_log_delta(&mut harness, &mut cursor), &[], &[(Some("inventory"), 1)], &[]);
}

/// A behavior-wrapped initially empty list must become eligible through the
/// host slot once its config is populated.
#[test]
fn behavior_host_empty_virtual_list_becomes_eligible_when_populated() {
    let Some(wasm_path) = require_wasm("aether_kit_widget_behavior") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut harness = SubstrateHarness::builder().size(240, 180).with_component_host().build().expect("boot");
    load_panel_with(
        &mut harness,
        &wasm,
        vec![
            behavior_host_list_spec("inventory", Vec::new(), None, WidgetControlState::default()),
            button_spec("run", WidgetControlState::default()),
        ],
    );
    let panel = panel_address();
    let host = child_address("inventory");
    let mut cursor = 0;

    harness
        .execute(vec![
            ("spawn", HarnessOp::send_and_settle(&panel, &Tick::default())),
            ("empty_hover", HarnessOp::send_and_settle(&panel, &hover_row_zero())),
            ("empty_press", HarnessOp::send_and_settle(&panel, &press(30.0, 22.0))),
            ("empty_release", HarnessOp::send_and_settle(&panel, &release(30.0, 22.0))),
            ("empty_tab", HarnessOp::send_and_settle(&panel, &tab())),
            ("empty_space", HarnessOp::send_and_settle(&panel, &space())),
            ("empty_space_up", HarnessOp::send_and_settle(&panel, &space_up())),
        ])
        .expect("empty host baseline");
    assert_list_phase("empty host baseline", &take_log_delta(&mut harness, &mut cursor), &[], &[], &["run"]);

    harness
        .execute(vec![
            ("populate", HarnessOp::send_and_settle(&host, &populated_list_config(Some(0)))),
            ("hover", HarnessOp::send_and_settle(&panel, &hover_row_zero())),
        ])
        .expect("populate host hover");
    assert_list_phase(
        "host populate hover",
        &take_log_delta(&mut harness, &mut cursor),
        &[(Some("inventory"), Some("0"))],
        &[],
        &[],
    );

    harness
        .execute(vec![
            ("tab_to_list", HarnessOp::send_and_settle(&panel, &tab())),
            ("down", HarnessOp::send_and_settle(&panel, &down())),
        ])
        .expect("host tab and down");
    assert_list_phase("host tab+down", &take_log_delta(&mut harness, &mut cursor), &[], &[(Some("inventory"), 1)], &[]);

    harness
        .execute(vec![
            ("click_row_two", HarnessOp::send_and_settle(&panel, &press(30.0, 70.0))),
            ("click_row_two_up", HarnessOp::send_and_settle(&panel, &release(30.0, 70.0))),
        ])
        .expect("host row click");
    assert_list_phase(
        "host click row 2",
        &take_log_delta(&mut harness, &mut cursor),
        &[(Some("inventory"), Some("2"))],
        &[(Some("inventory"), 2)],
        &[],
    );
}
