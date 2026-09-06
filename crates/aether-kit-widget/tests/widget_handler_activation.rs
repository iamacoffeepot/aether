//! Named-load activation of `WidgetDefaults` adopters (issue 5671).
//!
//! A local `#[handler]` that redeclares a set kind concatenates the same
//! kind into the trampoline manifest twice. `CostTable::prepare` then
//! rejects the duplicate and named `LoadComponent` fails with
//! `ActivationRejected`, while inline panel reconstruction still succeeds.
//! These scenarios load the already-exported adopters by typed config and
//! drive the override bodies that existing panel tests do not cover.
//!
//! Skips when the stem wasm has not been pre-built (`require_wasm`). CI sets
//! `AETHER_REQUIRE_RUNTIME=1` to turn that skip into a hard failure.

use std::fs;

use aether_actor::Addressable;
use aether_data::Kind;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{LoadComponent, LoadResult, LogTailResult, MouseMove, TextInput, Tick, WindowId};
use aether_kit_widget::{
    ButtonConfig, FocusLost, HoverLost, NumericConfig, PanelConfig, SegmentedConfig, TextAreaConfig, TextFieldConfig,
    Theme, VirtualListConfig, VirtualListRow, WidgetChildSpec, WidgetKind,
};

const TEST_WINDOW_ID: WindowId = WindowId(1);
const WASM_STEMS: [&str; 2] = ["aether_kit_widget", "aether_kit_widget_behavior"];

fn trampoline_address(name: &str) -> String {
    format!("aether.component/{}:{name}", aether_component::WasmTrampoline::NAMESPACE)
}

fn panel_child_address(subname: &str) -> String {
    format!("{}/{}:{subname}", trampoline_address("panel"), aether_component::WasmTrampoline::NAMESPACE)
}

struct NamedLoad {
    export: &'static str,
    name: &'static str,
    config: Vec<u8>,
}

fn exported_adopters() -> [NamedLoad; 6] {
    [
        NamedLoad {
            export: "aether.kit.widget.button",
            name: "button",
            config: ButtonConfig { label: "Go".to_owned(), theme: Theme::DEFAULT, ..ButtonConfig::default() }
                .encode_into_bytes(),
        },
        NamedLoad {
            export: "aether.kit.widget.text_field",
            name: "text_field",
            config: TextFieldConfig {
                initial: String::new(),
                max_chars: 0,
                theme: Theme::DEFAULT,
                ..TextFieldConfig::default()
            }
            .encode_into_bytes(),
        },
        NamedLoad {
            export: "aether.kit.widget.text_area",
            name: "text_area",
            config: TextAreaConfig {
                initial: String::new(),
                max_chars: 0,
                rows: 3,
                theme: Theme::DEFAULT,
                ..TextAreaConfig::default()
            }
            .encode_into_bytes(),
        },
        NamedLoad {
            export: "aether.kit.widget.numeric",
            name: "numeric",
            config: NumericConfig { min: 0.0, max: 100.0, step: 1.0, initial: 0.0, theme: Theme::DEFAULT, ..NumericConfig::default() }
                .encode_into_bytes(),
        },
        NamedLoad {
            export: "aether.kit.widget.segmented",
            name: "segmented",
            config: SegmentedConfig {
                options: vec!["Raise".to_owned(), "Lower".to_owned()],
                initial_index: 0,
                theme: Theme::DEFAULT,
                ..SegmentedConfig::default()
            }
            .encode_into_bytes(),
        },
        NamedLoad {
            export: "aether.kit.widget.virtual_list",
            name: "virtual_list",
            config: VirtualListConfig {
                items: vec![VirtualListRow::from("Row 0"), VirtualListRow::from("Row 1")],
                initial_selected_index: Some(0),
                visible_row_count: 2,
                theme: Theme::DEFAULT,
                ..VirtualListConfig::default()
            }
            .encode_into_bytes(),
        },
    ]
}

fn load_named(harness: &mut SubstrateHarness, wasm: &[u8], case: &NamedLoad) -> String {
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                "aether.component",
                &LoadComponent {
                    wasm: wasm.to_vec(),
                    name: Some(case.name.to_owned()),
                    config: case.config.clone(),
                    export: Some(case.export.to_owned()),
                },
            ),
        )])
        .expect("named load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { name, .. } => name,
        LoadResult::Err { error } => panic!("named load {} ({}) failed: {error}", case.name, case.export),
    }
}

fn load_panel_with(harness: &mut SubstrateHarness, wasm: &[u8], children: Vec<WidgetChildSpec>) {
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                "aether.component",
                &LoadComponent {
                    wasm: wasm.to_vec(),
                    name: Some("panel".to_owned()),
                    config: PanelConfig {
                        x: 10.0,
                        y: 10.0,
                        width: 200.0,
                        font_namespace: String::new(),
                        font_path: String::new(),
                        theme: Theme::DEFAULT,
                        children,
                        owns_input: true,
                    }
                    .encode_into_bytes(),
                    export: Some("aether.kit.widget.panel".to_owned()),
                },
            ),
        )])
        .expect("load panel sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { name, .. } => {
            assert!(name.ends_with(":panel"), "the panel root should register under :panel; got {name}");
        }
        LoadResult::Err { error } => panic!("load WidgetPanel root: {error}"),
    }
}

fn panel_log_messages(harness: &mut SubstrateHarness) -> Vec<String> {
    match harness.log_tail(&trampoline_address("panel"), None, None) {
        LogTailResult::Ok { entries, .. } => entries.into_iter().map(|entry| entry.message).collect(),
        LogTailResult::Err { error } => panic!("log_tail on the panel failed: {error}"),
    }
}

fn field<'a>(message: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key}=");
    message.split_whitespace().find_map(|token| token.strip_prefix(&prefix))
}

fn numeric_value(message: &str) -> Option<f32> {
    field(message, "value")?.parse().ok()
}

/// Duplicate local+set kinds used to reject `CostTable::prepare` at named
/// load. Each already-exported adopter must activate under both the stock
/// and behavior-host wasm artifacts.
#[test]
fn named_load_exported_widget_defaults_adopters_succeeds() {
    for stem in WASM_STEMS {
        let Some(wasm_path) = require_wasm(stem) else {
            continue;
        };
        let wasm = fs::read(&wasm_path).expect("read kit wasm");
        let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
        for case in exported_adopters() {
            let name = load_named(&mut harness, &wasm, &case);
            assert_eq!(
                name,
                trampoline_address(case.name),
                "{stem} named load of {} must register under {}; got {name}",
                case.export,
                trampoline_address(case.name),
            );
        }
    }
}

/// Numeric blur commits the typed buffer. The shared default only cancels
/// activation, so a dropped override would preview and then lose the value.
#[test]
fn numeric_focus_lost_commits_the_typed_buffer() {
    for stem in WASM_STEMS {
        let Some(wasm_path) = require_wasm(stem) else {
            continue;
        };
        let wasm = fs::read(&wasm_path).expect("read kit wasm");
        let mut harness = SubstrateHarness::builder().size(240, 80).with_component_host().build().expect("boot");
        load_panel_with(
            &mut harness,
            &wasm,
            vec![WidgetChildSpec {
                subname: "numeric".to_owned(),
                kind: WidgetKind::Numeric,
                origin: [0.0, 0.0],
                clip: None,
                config: NumericConfig {
                    min: 0.0,
                    max: 100.0,
                    step: 1.0,
                    initial: 0.0,
                    theme: Theme::DEFAULT,
                    ..NumericConfig::default()
                }
                .encode_into_bytes(),
            }],
        );
        let panel = trampoline_address("panel");
        let numeric = panel_child_address("numeric");
        harness
            .execute(vec![
                ("spawn", HarnessOp::send_and_settle(&panel, &Tick::default())),
                (
                    "type",
                    HarnessOp::send_and_settle(&numeric, &TextInput { window: TEST_WINDOW_ID, text: "7".to_owned() }),
                ),
            ])
            .expect("numeric type session");
        let before = panel_log_messages(&mut harness);
        let before_numeric: Vec<&String> =
            before.iter().filter(|message| message.contains("widget numeric changed")).collect();
        assert_eq!(
            before_numeric.len(),
            1,
            "{stem}: typing must preview exactly once; log was:\n{}",
            before.join("\n"),
        );
        assert_eq!(field(before_numeric[0], "widget"), Some("numeric"));
        assert_eq!(numeric_value(before_numeric[0]), Some(7.0));
        assert_eq!(field(before_numeric[0], "committed"), Some("false"));
        harness
            .execute(vec![("blur", HarnessOp::send_and_settle(&numeric, &FocusLost))])
            .expect("numeric focus-lost session");
        let after = panel_log_messages(&mut harness);
        let after_numeric: Vec<&String> =
            after.iter().filter(|message| message.contains("widget numeric changed")).collect();
        assert_eq!(
            after_numeric.len(),
            2,
            "{stem}: FocusLost must append the commit; log was:\n{}",
            after.join("\n"),
        );
        assert_eq!(field(after_numeric[1], "widget"), Some("numeric"));
        assert_eq!(numeric_value(after_numeric[1]), Some(7.0));
        assert_eq!(field(after_numeric[1], "committed"), Some("true"));
    }
}

/// Virtual-list `HoverLost` clears the hovered row. The shared default only
/// flips the widget-wide hover bit, so a dropped override would leave the
/// last row reported as still under the pointer.
#[test]
fn virtual_list_hover_lost_clears_the_hovered_row() {
    for stem in WASM_STEMS {
        let Some(wasm_path) = require_wasm(stem) else {
            continue;
        };
        let wasm = fs::read(&wasm_path).expect("read kit wasm");
        let mut harness = SubstrateHarness::builder().size(240, 120).with_component_host().build().expect("boot");
        load_panel_with(
            &mut harness,
            &wasm,
            vec![WidgetChildSpec {
                subname: "inventory".to_owned(),
                kind: WidgetKind::VirtualList,
                origin: [0.0, 0.0],
                clip: None,
                config: VirtualListConfig {
                    items: vec![VirtualListRow::from("Row 0"), VirtualListRow::from("Row 1")],
                    initial_selected_index: Some(0),
                    visible_row_count: 2,
                    theme: Theme::DEFAULT,
                    ..VirtualListConfig::default()
                }
                .encode_into_bytes(),
            }],
        );
        let panel = trampoline_address("panel");
        let list = panel_child_address("inventory");
        harness
            .execute(vec![
                ("spawn", HarnessOp::send_and_settle(&panel, &Tick::default())),
                (
                    "hover_row",
                    HarnessOp::send_and_settle(&list, &MouseMove { window: TEST_WINDOW_ID, x: 30.0, y: 22.0 }),
                ),
            ])
            .expect("virtual-list hover session");
        let before = panel_log_messages(&mut harness);
        let before_hover: Vec<&String> =
            before.iter().filter(|message| message.contains("widget virtual list hover")).collect();
        assert_eq!(
            before_hover.len(),
            1,
            "{stem}: a move over the first row must report it once; log was:\n{}",
            before.join("\n"),
        );
        assert_eq!(field(before_hover[0], "widget"), Some("inventory"));
        assert_eq!(field(before_hover[0], "row"), Some("Some(0)"));
        harness
            .execute(vec![("leave", HarnessOp::send_and_settle(&list, &HoverLost))])
            .expect("virtual-list hover-lost session");
        let after = panel_log_messages(&mut harness);
        let after_hover: Vec<&String> =
            after.iter().filter(|message| message.contains("widget virtual list hover")).collect();
        assert_eq!(
            after_hover.len(),
            2,
            "{stem}: HoverLost must append the leave; log was:\n{}",
            after.join("\n"),
        );
        assert_eq!(field(after_hover[1], "widget"), Some("inventory"));
        assert_eq!(field(after_hover[1], "row"), Some("None"));
    }
}
