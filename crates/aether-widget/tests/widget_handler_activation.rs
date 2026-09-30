//! Activation of `WidgetDefaults` adopters' override bodies (issue 5671).
//!
//! A local `#[handler]` that redeclares a set kind concatenates the same
//! kind into the trampoline manifest twice, which `CostTable::prepare`
//! rejects. The set widgets are spawned only inline under a panel or a
//! scroll (issue 7206), so these scenarios load a panel that spawns the
//! adopters and drive the override bodies that the other panel tests do not
//! cover.
//!
//! Skips when no wgpu adapter is available or the stem wasm has not been
//! pre-built (the shared `require_runtime` gate): the widget module declares
//! render, and only a real render serves it. CI sets `AETHER_REQUIRE_RUNTIME=1`
//! to turn either skip into a hard failure.

mod support;

use std::fs;

use aether_actor::{ActorRef, ChildOf, Instanced};
use aether_data::{Kind, LoadName};
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_harness_substrate_capture::test_helpers::{init_save_sandbox, require_runtime, test_namespace_roots};
use aether_kinds::{LoadComponent, LogTailResult, MouseMove, TextInput, Tick};
use aether_widget::set::{NumericWidget, VirtualListWidget};
use aether_widget::{
    FocusLost, HoverLost, NumericConfig, PanelConfig, Theme, VirtualListConfig, VirtualListRow, WidgetChildSpec,
    WidgetKind, WidgetPanel,
};
use support::widget_caps;

/// The window the injected input events name.
fn test_window() -> aether_data::ErasedActorPath {
    aether_window::window_path(&LoadName::new("main").expect("a valid window name"))
}
const WASM_STEMS: [&str; 1] = ["aether_widget"];

/// A GPU bench with the component host and everything the widget module
/// declares: the real render, text (its fs from the sandbox roots) and the
/// in-memory clipboard.
fn bench(width: u32, height: u32) -> SubstrateHarness {
    widget_caps(
        SubstrateHarness::builder()
            .size(width, height)
            .namespace_roots(test_namespace_roots(init_save_sandbox("widget-activation")))
            .with_render()
            .with_component_host(),
    )
    .build()
    .expect("boot")
}

/// The `C` child the panel spawned under `subname`.
fn panel_child<C: ChildOf<WidgetPanel> + Instanced>(
    harness: &SubstrateHarness,
    panel: ActorRef<WidgetPanel>,
    subname: &str,
) -> ActorRef<C> {
    harness
        .child::<WidgetPanel, C>(&panel, LoadName::new(subname).expect("a valid child subname"))
        .unwrap_or_else(|error| panic!("the panel's {subname} child is live: {error}"))
}

fn load_panel_with(
    harness: &mut SubstrateHarness,
    wasm: &[u8],
    children: Vec<WidgetChildSpec>,
) -> ActorRef<WidgetPanel> {
    let config = PanelConfig {
        x: 10.0,
        y: 10.0,
        width: 200.0,
        font_namespace: String::new(),
        font_path: String::new(),
        theme: Theme::DEFAULT,
        children,
        owns_input: true,
        editor_region: String::new(),
    };
    let panel = harness
        .load::<WidgetPanel>(LoadComponent {
            wasm: wasm.to_vec(),
            name: Some("panel".to_owned()),
            config: config.encode_into_bytes(),
            export: Some("aether.widget.panel".to_owned()),
        })
        .unwrap_or_else(|error| panic!("load WidgetPanel root: {error}"));
    let path = harness.actor_path(&panel);
    assert!(path.to_string().ends_with(":panel"), "the panel root should register under :panel; got {path}");
    panel
}

fn panel_log_messages(harness: &mut SubstrateHarness, panel: ActorRef<WidgetPanel>) -> Vec<String> {
    match harness.log_tail(&panel, None, None) {
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

/// Numeric blur commits the typed buffer. The shared default only cancels
/// activation, so a dropped override would preview and then lose the value.
#[test]
fn numeric_focus_lost_commits_the_typed_buffer() {
    for stem in WASM_STEMS {
        let Some(wasm_path) = require_runtime(stem) else {
            continue;
        };
        let wasm = fs::read(&wasm_path).expect("read widget wasm");
        let mut harness = bench(240, 80);
        let panel = load_panel_with(
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
        harness
            .execute(vec![("spawn", HarnessOp::send_and_settle(&panel, &Tick::default()))])
            .expect("spawn the numeric child");
        let numeric = panel_child::<NumericWidget>(&harness, panel, "numeric");
        harness
            .execute(vec![(
                "type",
                HarnessOp::send_and_settle(&numeric, &TextInput { window: test_window(), text: "7".to_owned() }),
            )])
            .expect("numeric type session");
        let before = panel_log_messages(&mut harness, panel);
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
        let after = panel_log_messages(&mut harness, panel);
        let after_numeric: Vec<&String> =
            after.iter().filter(|message| message.contains("widget numeric changed")).collect();
        assert_eq!(after_numeric.len(), 2, "{stem}: FocusLost must append the commit; log was:\n{}", after.join("\n"));
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
        let Some(wasm_path) = require_runtime(stem) else {
            continue;
        };
        let wasm = fs::read(&wasm_path).expect("read widget wasm");
        let mut harness = bench(240, 120);
        let panel = load_panel_with(
            &mut harness,
            &wasm,
            vec![WidgetChildSpec {
                subname: "inventory".to_owned(),
                kind: WidgetKind::VirtualList,
                origin: [0.0, 0.0],
                clip: None,
                config: VirtualListConfig {
                    items: vec![VirtualListRow::from("Row 0"), VirtualListRow::from("Row 1")],
                    initial: Some(0),
                    visible_row_count: 2,
                    theme: Theme::DEFAULT,
                    ..VirtualListConfig::default()
                }
                .encode_into_bytes(),
            }],
        );
        harness
            .execute(vec![("spawn", HarnessOp::send_and_settle(&panel, &Tick::default()))])
            .expect("spawn the list child");
        let list = panel_child::<VirtualListWidget>(&harness, panel, "inventory");
        harness
            .execute(vec![(
                "hover_row",
                HarnessOp::send_and_settle(&list, &MouseMove { window: test_window(), x: 30.0, y: 22.0 }),
            )])
            .expect("virtual-list hover session");
        let before = panel_log_messages(&mut harness, panel);
        let before_hover: Vec<&String> =
            before.iter().filter(|message| message.contains("widget virtual list hover")).collect();
        assert_eq!(
            before_hover.len(),
            1,
            "{stem}: a move over the first row must report it once; log was:\n{}",
            before.join("\n"),
        );
        assert_eq!(field(before_hover[0], "widget"), Some("inventory"));
        // tracing records Option<u32> as the inner number, or omits the field when None;
        // a missing `row` is the leave only on this newly emitted second hover event.
        assert_eq!(field(before_hover[0], "index"), Some("0"));
        harness
            .execute(vec![("leave", HarnessOp::send_and_settle(&list, &HoverLost))])
            .expect("virtual-list hover-lost session");
        let after = panel_log_messages(&mut harness, panel);
        let after_hover: Vec<&String> =
            after.iter().filter(|message| message.contains("widget virtual list hover")).collect();
        assert_eq!(after_hover.len(), 2, "{stem}: HoverLost must append the leave; log was:\n{}", after.join("\n"));
        assert_eq!(field(after_hover[1], "widget"), Some("inventory"));
        assert_eq!(field(after_hover[1], "index"), None);
    }
}
