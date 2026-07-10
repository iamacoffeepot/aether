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
//! Skipped when no wgpu adapter is available or the `aether_kit` wasm has not
//! been pre-built (the shared `require_runtime` gate). CI sets
//! `AETHER_REQUIRE_RUNTIME=1` to turn either skip into a hard failure.

// Integration-test skip diagnostic: emit via stderr so `cargo test` surfaces
// "skipping: ..." alongside `test ... ok` (issue 891).
#![allow(clippy::print_stderr)]
// Pixel-rect layout constants read clearest as float literals inline.
#![allow(clippy::cast_precision_loss)]

use std::fs;

use aether_actor::Addressable;
use aether_data::Kind;
use aether_kinds::keycode::{KEY_DOWN, KEY_ENTER, KEY_SPACE, KEY_TAB, KEY_UP};
use aether_kinds::mouse_button::LEFT;
use aether_kinds::{
    Key, KeyRelease, LoadComponent, LoadResult, LogTailResult, Modifiers, MouseButton,
    MouseButtonRelease, MouseMove, TextInput, Tick,
};
use aether_kit::{
    ButtonConfig, PanelConfig, SetWidgetState, SliderConfig, TextFieldConfig, Theme,
    WidgetChildSpec, WidgetControlState, WidgetKind,
};
use aether_substrate_bundle::test_bench::{BenchOp, TestBench, test_helpers::require_runtime};

/// The full trampoline address the loaded panel registers at (ADR-0099 §4).
fn panel_address() -> String {
    format!(
        "aether.component/{}:panel",
        aether_capabilities::WasmTrampoline::NAMESPACE,
    )
}

fn child_address(subname: &str) -> String {
    format!(
        "{}/{}:{}",
        panel_address(),
        aether_capabilities::WasmTrampoline::NAMESPACE,
        subname,
    )
}

/// Load the `WidgetPanel` root under the name `panel` (export
/// `aether.kit.widget.panel`) with a config that places its stack at
/// `(10, 10)` 200px wide, no font (`font_path` empty, so no `aether.text`
/// dependency), and the default theme.
fn load_panel(bench: &mut TestBench, wasm: &[u8]) {
    let config = PanelConfig {
        x: 10.0,
        y: 10.0,
        width: 200.0,
        font_namespace: String::new(),
        font_path: String::new(),
        theme: Theme::DEFAULT,
        children: Vec::new(),
    };
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
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
    match loaded
        .reply::<LoadResult>("load")
        .expect("decode LoadResult")
    {
        LoadResult::Ok { name, .. } => assert!(
            name.ends_with(":panel"),
            "the panel root should register under :panel; got {name}",
        ),
        LoadResult::Err { error } => panic!("load WidgetPanel root: {error}"),
    }
}

/// Every log message in the panel's ring, oldest first.
fn panel_log_messages(bench: &mut TestBench) -> Vec<String> {
    match bench.log_tail(&panel_address(), None) {
        LogTailResult::Ok { entries, .. } => entries.into_iter().map(|e| e.message).collect(),
        LogTailResult::Err { error } => panic!("log_tail on the panel failed: {error}"),
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
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut bench = TestBench::start_with_size(240, 220).expect("boot");
    load_panel(&mut bench, &wasm);

    let panel = panel_address();
    // The first tick spawns the widget stack and assigns each child its frame;
    // every later step drives one input event, settling its whole in-cluster
    // chain before the next.
    // Each input kind is fire-and-forget (no reply), so `send_mail` — which
    // still blocks until the whole dispatched chain settles — is the op;
    // `send_and_await` would hang waiting for a reply that never comes.
    bench
        .execute(vec![
            ("spawn", BenchOp::send_mail(&panel, &Tick)),
            // Slider drag: press mid-track, drag right, release — the release
            // commits at the dragged value (x=160 → 75% of 0..255 ≈ 191). The
            // press also focuses the slider.
            (
                "drag_press",
                BenchOp::send_mail(&panel, &press(110.0, 52.0)),
            ),
            (
                "drag_move",
                BenchOp::send_mail(&panel, &MouseMove { x: 160.0, y: 52.0 }),
            ),
            (
                "drag_release",
                BenchOp::send_mail(&panel, &release(160.0, 52.0)),
            ),
            // Tab moves focus off the slider to the radio group; Down then
            // routes to the focused radio, moving its selection to index 1.
            ("tab", BenchOp::send_mail(&panel, &Key { code: KEY_TAB })),
            (
                "radio_key",
                BenchOp::send_mail(&panel, &Key { code: KEY_DOWN }),
            ),
            // A click on the third radio row (y 118..142) selects index 2.
            (
                "radio_press",
                BenchOp::send_mail(&panel, &press(30.0, 125.0)),
            ),
            (
                "radio_release",
                BenchOp::send_mail(&panel, &release(30.0, 125.0)),
            ),
            // Focus the text field (y 148..172), type into it, and commit.
            (
                "text_focus",
                BenchOp::send_mail(&panel, &press(50.0, 160.0)),
            ),
            (
                "text_focus_up",
                BenchOp::send_mail(&panel, &release(50.0, 160.0)),
            ),
            (
                "type",
                BenchOp::send_mail(
                    &panel,
                    &TextInput {
                        text: "hi".to_owned(),
                    },
                ),
            ),
            (
                "commit",
                BenchOp::send_mail(&panel, &Key { code: KEY_ENTER }),
            ),
        ])
        .expect("input session");

    let log = panel_log_messages(&mut bench);
    let joined = log.join("\n");

    assert!(
        log.iter()
            .any(|m| m.contains("widget slider changed") && m.contains("committed=true")),
        "the slider drag-release should log a committed change; log was:\n{joined}",
    );
    assert!(
        log.iter().any(|m| m.contains("widget slider changed")
            && m.contains("widget=slider")
            && m.contains("committed=false")),
        "the drag should stream at least one uncommitted change; log was:\n{joined}",
    );
    assert!(
        log.iter()
            .any(|m| m.contains("widget radio selected") && m.contains("index=1")),
        "Tab-then-Down should route to the radio and select index 1 — proving the \
         focus cycle and keyboard routing; log was:\n{joined}",
    );
    assert!(
        log.iter()
            .any(|m| m.contains("widget radio selected") && m.contains("index=2")),
        "the radio row click should select index 2; log was:\n{joined}",
    );
    assert!(
        log.iter()
            .any(|m| m.contains("widget text committed") && m.contains("text=hi")),
        "the text entry then Enter should commit \"hi\" — proving pointer focus \
         and text routing; log was:\n{joined}",
    );
}

/// A slider child spec for the declarative-children scenario: full `0..=255`
/// range, unit step, seeded at `initial`, default theme.
fn slider_spec(subname: &str, initial: f32) -> WidgetChildSpec {
    slider_spec_with_state(subname, initial, WidgetControlState::default())
}

fn slider_spec_with_state(
    subname: &str,
    initial: f32,
    state: WidgetControlState,
) -> WidgetChildSpec {
    WidgetChildSpec {
        subname: subname.to_owned(),
        kind: WidgetKind::Slider,
        origin: [0.0, 0.0],
        clip: None,
        config: SliderConfig {
            min: 0.0,
            max: 255.0,
            step: 1.0,
            initial,
            theme: Theme::DEFAULT,
            state,
        }
        .encode_into_bytes(),
    }
}

fn button_spec(subname: &str, state: WidgetControlState) -> WidgetChildSpec {
    WidgetChildSpec {
        subname: subname.to_owned(),
        kind: WidgetKind::Button,
        origin: [0.0, 0.0],
        clip: None,
        config: ButtonConfig {
            label: "Run".to_owned(),
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
        config: TextFieldConfig {
            initial: initial.to_owned(),
            max_chars: 0,
            theme: Theme::DEFAULT,
            state,
        }
        .encode_into_bytes(),
    }
}

/// Load the reference `WidgetPanel` root with an explicit `children` list (so
/// it stacks exactly those specs rather than its built-in reference stack) at
/// `(10, 10)` 200px wide, no font, default theme.
fn load_panel_with(bench: &mut TestBench, wasm: &[u8], children: Vec<WidgetChildSpec>) {
    let config = PanelConfig {
        x: 10.0,
        y: 10.0,
        width: 200.0,
        font_namespace: String::new(),
        font_path: String::new(),
        theme: Theme::DEFAULT,
        children,
    };
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
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
    match loaded
        .reply::<LoadResult>("load")
        .expect("decode LoadResult")
    {
        LoadResult::Ok { name, .. } => assert!(
            name.ends_with(":panel"),
            "the panel root should register under :panel; got {name}",
        ),
        LoadResult::Err { error } => panic!("load WidgetPanel root: {error}"),
    }
}

/// The widget name a `widget slider changed` log line attributes its value to
/// (the `widget=NAME` field the panel logs) — `None` for a non-slider line.
fn slider_changed_widget(message: &str) -> Option<String> {
    message.split("widget=").nth(1).map(|rest| {
        rest.split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned()
    })
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
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut bench = TestBench::start_with_size(240, 220).expect("boot");
    load_panel_with(
        &mut bench,
        &wasm,
        vec![slider_spec("first", 40.0), slider_spec("second", 40.0)],
    );

    let panel = panel_address();
    bench
        .execute(vec![
            // First tick spawns + lays out the declared stack.
            ("spawn", BenchOp::send_mail(&panel, &Tick)),
            // Tab from no focus lands on the first focusable child (index 0);
            // an arrow nudge on the focused slider commits + logs it.
            (
                "tab_first",
                BenchOp::send_mail(&panel, &Key { code: KEY_TAB }),
            ),
            (
                "nudge_first",
                BenchOp::send_mail(&panel, &Key { code: KEY_UP }),
            ),
            // Tab again advances to the second child; nudge + log it.
            (
                "tab_second",
                BenchOp::send_mail(&panel, &Key { code: KEY_TAB }),
            ),
            (
                "nudge_second",
                BenchOp::send_mail(&panel, &Key { code: KEY_UP }),
            ),
        ])
        .expect("declared-children session");

    let log = panel_log_messages(&mut bench);
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

fn drive_state_and_keyboard_session(bench: &mut TestBench) {
    let panel = panel_address();
    let value = child_address("value");
    let run = child_address("run");
    bench
        .execute(vec![
            ("spawn", BenchOp::send_mail(&panel, &Tick)),
            // Forward Tab skips the disabled first slider and focuses the
            // read-only value. Its arrow input must not mutate.
            (
                "tab_value",
                BenchOp::send_mail(&panel, &Key { code: KEY_TAB }),
            ),
            (
                "blocked_nudge",
                BenchOp::send_mail(&panel, &Key { code: KEY_UP }),
            ),
            // Runtime state changes preserve the value while enabling mutation.
            (
                "make_mutable",
                BenchOp::send_mail(
                    &value,
                    &SetWidgetState {
                        state: WidgetControlState::default(),
                    },
                ),
            ),
            (
                "allowed_nudge",
                BenchOp::send_mail(&panel, &Key { code: KEY_UP }),
            ),
            // Shift+Tab wraps backward to the Button, skipping the disabled
            // first entry. Space fires on release.
            (
                "shift",
                BenchOp::send_mail(
                    &panel,
                    &Modifiers {
                        shift: true,
                        ..Modifiers::default()
                    },
                ),
            ),
            (
                "reverse_tab",
                BenchOp::send_mail(&panel, &Key { code: KEY_TAB }),
            ),
            (
                "space",
                BenchOp::send_mail(&panel, &Key { code: KEY_SPACE }),
            ),
            (
                "space_release",
                BenchOp::send_mail(&panel, &KeyRelease { code: KEY_SPACE }),
            ),
            // Enter fires immediately and suppresses repeated key-down mail
            // until its matching release.
            (
                "enter",
                BenchOp::send_mail(&panel, &Key { code: KEY_ENTER }),
            ),
            (
                "enter_repeat",
                BenchOp::send_mail(&panel, &Key { code: KEY_ENTER }),
            ),
            (
                "enter_release",
                BenchOp::send_mail(&panel, &KeyRelease { code: KEY_ENTER }),
            ),
            // Hiding the focused button moves focus forward to the live slider;
            // no stale keyboard arm or focus remains on the button.
            (
                "hide_button",
                BenchOp::send_mail(
                    &run,
                    &SetWidgetState {
                        state: WidgetControlState {
                            visible: false,
                            ..WidgetControlState::default()
                        },
                    },
                ),
            ),
            (
                "nudge_after_hide",
                BenchOp::send_mail(&panel, &Key { code: KEY_UP }),
            ),
        ])
        .expect("state and keyboard session");
}

/// Initial and runtime control state must agree with panel routing: unavailable
/// children leave their layout slot but exit the focus ring, read-only values
/// focus without mutation, reverse Tab skips unavailable entries, and keyboard
/// activation fires Button exactly once per key pair.
#[test]
fn panel_routes_availability_read_only_reverse_tab_and_button_keys() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut bench = TestBench::start_with_size(240, 140).expect("boot");

    let disabled = WidgetControlState {
        enabled: false,
        ..WidgetControlState::default()
    };
    let read_only = WidgetControlState {
        read_only: true,
        ..WidgetControlState::default()
    };
    load_panel_with(
        &mut bench,
        &wasm,
        vec![
            slider_spec_with_state("disabled", 40.0, disabled),
            slider_spec_with_state("value", 40.0, read_only),
            button_spec("run", WidgetControlState::default()),
        ],
    );

    drive_state_and_keyboard_session(&mut bench);

    let log = panel_log_messages(&mut bench);
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
        .filter(|message| {
            message.contains("widget button clicked") && message.contains("widget=run")
        })
        .count();
    assert_eq!(
        clicks, 2,
        "Space release and the first Enter press click exactly once each; log was:\n{joined}",
    );
}

/// A read-only text field remains focusable but cannot commit. Enabling the
/// same live actor then permits exactly one commit, proving the negative path
/// is not an input-routing or log-observation false positive.
#[test]
fn read_only_text_field_blocks_activation_until_enabled() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut bench = TestBench::start_with_size(240, 80).expect("boot");
    let read_only = WidgetControlState {
        read_only: true,
        ..WidgetControlState::default()
    };
    load_panel_with(
        &mut bench,
        &wasm,
        vec![text_field_spec("locked", "locked", read_only)],
    );

    let panel = panel_address();
    bench
        .execute(vec![
            ("spawn", BenchOp::send_mail(&panel, &Tick)),
            ("focus", BenchOp::send_mail(&panel, &Key { code: KEY_TAB })),
            (
                "blocked_text",
                BenchOp::send_mail(
                    &panel,
                    &TextInput {
                        text: " mutation".to_owned(),
                    },
                ),
            ),
            (
                "blocked_enter",
                BenchOp::send_mail(&panel, &Key { code: KEY_ENTER }),
            ),
            (
                "enable",
                BenchOp::send_mail(
                    child_address("locked"),
                    &SetWidgetState {
                        state: WidgetControlState::default(),
                    },
                ),
            ),
            (
                "allowed_enter",
                BenchOp::send_mail(&panel, &Key { code: KEY_ENTER }),
            ),
        ])
        .expect("read-only text activation session");

    let log = panel_log_messages(&mut bench);
    let joined = log.join("\n");
    let commits: Vec<_> = log
        .iter()
        .filter(|message| {
            message.contains("widget text committed") && message.contains("widget=locked")
        })
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
