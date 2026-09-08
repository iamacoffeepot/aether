//! Named-export and replace-reconstruct coverage for the widget module's
//! `export!` lists (issue 5538).
//!
//! Both the default and `behavior` wasm builds are grab-bag defaultless
//! modules (ADR-0138). A type missing from `export!` cannot be loaded by
//! `module@actor` selector, and ADR-0114 §5 reconstructs inline children
//! from that same list — so a panel-spawned Dropdown, `TabStrip`, or `MenuBar`
//! vanishes across `replace_component` even though typed spawn still works
//! on a cold Tick.
//!
//! Reconstruction assertions send input to the original child aliases
//! *without* a post-replace Tick: `WidgetPanel` does not persist `spawned`,
//! so a later Tick would re-run typed `spawn_inline_child` and mask a
//! reconstruct miss. The stock widgets declare no `type Persist`, so these
//! tests prove post-replace behavior at the original aliases, not that a
//! live selection survived the swap.
//!
//! Skipped when the matching wasm has not been pre-built (`require_wasm`).
//! CI sets `AETHER_REQUIRE_RUNTIME=1` to turn that skip into a hard failure.
//! The parent builds `aether_kit_widget` and `aether_kit_widget_behavior`.

use std::fs;

use aether_actor::Addressable;
use aether_data::Kind;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::keycode::{KEY_DOWN, KEY_ENTER, KEY_RIGHT};
use aether_kinds::mouse_button::LEFT;
use aether_kinds::{
    Key, KeyRelease, LoadComponent, LoadResult, LogTailResult, MouseButton, MouseButtonRelease, ReplaceComponent,
    ReplaceResult, Tick, WindowId,
};
use aether_kit_widget::{
    DialogConfig, DropdownConfig, Menu, MenuBarConfig, MenuItem, PanelConfig, SplitterAxis, SplitterConfig,
    TabStripConfig, Theme, ToastConfig, TooltipConfig, TooltipSection, WidgetChildSpec, WidgetControlState,
    WidgetFrame, WidgetKind,
};

const DEFAULT_STEM: &str = "aether_kit_widget";
const BEHAVIOR_STEM: &str = "aether_kit_widget_behavior";
const TEST_WINDOW_ID: WindowId = WindowId(1);

/// NAMESPACEs of the seven actors both `export!` lists omitted.
const SEVEN: [&str; 7] = [
    "aether.kit.widget.dropdown",
    "aether.kit.widget.tab_strip",
    "aether.kit.widget.menu_bar",
    "aether.kit.widget.tooltip",
    "aether.kit.widget.toast",
    "aether.kit.widget.dialog",
    "aether.kit.widget.splitter",
];

fn wasm_or_skip(stem: &str) -> Option<Vec<u8>> {
    let path = require_wasm(stem)?;
    Some(fs::read(&path).unwrap_or_else(|error| panic!("read {stem} wasm: {error}")))
}

fn panel_address() -> String {
    format!("aether.component/{}:panel", aether_component::WasmTrampoline::NAMESPACE)
}

fn child_address(subname: &str) -> String {
    format!("{}/{}:{subname}", panel_address(), aether_component::WasmTrampoline::NAMESPACE)
}

fn panel_log_messages(harness: &mut SubstrateHarness) -> Vec<String> {
    match harness.log_tail(&panel_address(), None, None) {
        LogTailResult::Ok { entries, .. } => entries.into_iter().map(|entry| entry.message).collect(),
        LogTailResult::Err { error } => panic!("log_tail on the panel failed: {error}"),
    }
}

fn press(x: f32, y: f32) -> MouseButton {
    MouseButton { window: TEST_WINDOW_ID, button: LEFT, x, y }
}

fn release(x: f32, y: f32) -> MouseButtonRelease {
    MouseButtonRelease { window: TEST_WINDOW_ID, button: LEFT, x, y }
}

fn dropdown_config() -> DropdownConfig {
    DropdownConfig {
        options: vec!["Alpha".into(), "Beta".into()],
        initial: Some(0),
        placeholder: String::new(),
        open_row_count: 4,
        theme: Theme::DEFAULT,
        state: WidgetControlState::default(),
    }
}

fn tab_strip_config() -> TabStripConfig {
    TabStripConfig {
        labels: vec!["One".to_owned(), "Two".to_owned()],
        initial: 0,
        theme: Theme::DEFAULT,
        ..TabStripConfig::default()
    }
}

fn menu_bar_config() -> MenuBarConfig {
    MenuBarConfig {
        menus: vec![Menu {
            title: "File".to_owned(),
            items: vec![MenuItem { label: "Open".to_owned(), ..MenuItem::default() }],
        }],
        theme: Theme::DEFAULT,
        state: WidgetControlState::default(),
    }
}

fn tooltip_config() -> TooltipConfig {
    TooltipConfig { sections: vec![TooltipSection::new(["Hint"])], theme: Theme::DEFAULT, ..TooltipConfig::default() }
}

fn toast_config() -> ToastConfig {
    ToastConfig { theme: Theme::DEFAULT, ..ToastConfig::default() }
}

fn dialog_config() -> DialogConfig {
    DialogConfig { title: "Confirm".to_owned(), theme: Theme::DEFAULT, ..DialogConfig::default() }
}

fn splitter_config() -> SplitterConfig {
    SplitterConfig {
        axis: SplitterAxis::Horizontal,
        min_pixels: 40.0,
        max_pixels: 400.0,
        position_pixels: 120.0,
        theme: Theme::DEFAULT,
        ..SplitterConfig::default()
    }
}

/// Typed `Config` bytes for one of the seven named exports. Empty raw payload
/// is not a typed-config guest's init (ADR-0090): the host decodes
/// `Self::Config` from these bytes.
fn encoded_config_for(export: &str) -> Vec<u8> {
    let bytes = match export {
        "aether.kit.widget.dropdown" => dropdown_config().encode_into_bytes(),
        "aether.kit.widget.tab_strip" => tab_strip_config().encode_into_bytes(),
        "aether.kit.widget.menu_bar" => menu_bar_config().encode_into_bytes(),
        "aether.kit.widget.tooltip" => tooltip_config().encode_into_bytes(),
        "aether.kit.widget.toast" => toast_config().encode_into_bytes(),
        "aether.kit.widget.dialog" => dialog_config().encode_into_bytes(),
        "aether.kit.widget.splitter" => splitter_config().encode_into_bytes(),
        other => panic!("named-load table is missing a Config for {other}"),
    };
    assert!(!bytes.is_empty(), "{export}: encoded Config must be a typed payload, not empty raw bytes");
    bytes
}

fn panel_config() -> PanelConfig {
    PanelConfig {
        x: 10.0,
        y: 10.0,
        width: 200.0,
        font_namespace: String::new(),
        font_path: String::new(),
        theme: Theme::DEFAULT,
        children: vec![
            WidgetChildSpec {
                subname: "dropdown".to_owned(),
                kind: WidgetKind::Dropdown,
                origin: [0.0, 0.0],
                clip: None,
                config: dropdown_config().encode_into_bytes(),
            },
            WidgetChildSpec {
                subname: "tabs".to_owned(),
                kind: WidgetKind::TabStrip,
                origin: [0.0, 0.0],
                clip: None,
                config: tab_strip_config().encode_into_bytes(),
            },
            WidgetChildSpec {
                subname: "menu".to_owned(),
                kind: WidgetKind::MenuBar,
                origin: [0.0, 0.0],
                clip: None,
                config: menu_bar_config().encode_into_bytes(),
            },
        ],
        owns_input: true,
    }
}

/// ADR-0138: a bare load of this grab-bag must error and name every omitted
/// actor, while each of those seven NAMESPACEs must resolve as a named export.
fn assert_selectors(wasm: &[u8], stem: &str) {
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");

    let bare = harness
        .execute(vec![(
            "bare",
            HarnessOp::send_and_await_reply(
                "aether.component",
                &LoadComponent { wasm: wasm.to_vec(), name: None, config: Vec::new(), export: None },
            ),
        )])
        .expect("bare load sequence");
    match bare.reply::<LoadResult>("bare").expect("decode bare LoadResult") {
        LoadResult::Err { error } => {
            assert!(
                error.contains("no default"),
                "{stem}: a defaultless bare load must stay an ADR-0138 no-default error; got {error}"
            );
            for export in SEVEN {
                assert!(
                    error.contains(export),
                    "{stem}: bare-load error must name {export} so a caller can select it; got {error}"
                );
            }
        }
        LoadResult::Ok { name, .. } => {
            panic!("{stem}: a bare load of the widget module must error, not instantiate {name}")
        }
    }

    for export in SEVEN {
        let loaded = harness
            .execute(vec![(
                "named",
                HarnessOp::send_and_await_reply(
                    "aether.component",
                    &LoadComponent {
                        wasm: wasm.to_vec(),
                        name: None,
                        config: encoded_config_for(export),
                        export: Some(export.to_owned()),
                    },
                ),
            )])
            .unwrap_or_else(|error| panic!("{stem}: named load of {export}: {error}"));
        match loaded.reply::<LoadResult>("named").expect("decode named LoadResult") {
            LoadResult::Ok { name, .. } => {
                assert!(
                    name.ends_with(&format!(":{export}")),
                    "{stem}: named load of {export} must instantiate that NAMESPACE; got {name}"
                );
            }
            LoadResult::Err { error } => {
                panic!("{stem}: named load of {export} must succeed; got err {error}")
            }
        }
    }
}

/// ADR-0114 §5: panel-reachable Dropdown / `TabStrip` / `MenuBar` children keep
/// handling their kinds at the original aliases after replace, without a
/// post-replace Tick that would re-spawn them through typed spawn.
fn assert_panel_children_reconstruct(wasm: &[u8], stem: &str) {
    let config = panel_config();
    let config_bytes = config.encode_into_bytes();
    let mut harness = SubstrateHarness::builder().size(240, 220).with_component_host().build().expect("boot");

    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                "aether.component",
                &LoadComponent {
                    wasm: wasm.to_vec(),
                    name: Some("panel".to_owned()),
                    config: config_bytes.clone(),
                    export: Some("aether.kit.widget.panel".to_owned()),
                },
            ),
        )])
        .expect("load panel");
    let mailbox_id = match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { mailbox_id, .. } => mailbox_id,
        LoadResult::Err { error } => panic!("{stem}: load WidgetPanel: {error}"),
    };

    let panel = panel_address();
    harness
        .execute(vec![("spawn", HarnessOp::send_and_settle(&panel, &Tick::default()))])
        .expect("first tick spawns the declared children");

    let swapped = harness
        .execute(vec![(
            "swap",
            HarnessOp::send_and_await_reply(
                "aether.component",
                &ReplaceComponent {
                    mailbox_id,
                    wasm: wasm.to_vec(),
                    drain_timeout_ms: None,
                    config: config_bytes,
                    export: None,
                },
            ),
        )])
        .expect("replace sequence");
    match swapped.reply::<ReplaceResult>("swap").expect("decode ReplaceResult") {
        ReplaceResult::Ok { .. } => {}
        ReplaceResult::Err { error } => panic!("{stem}: replace_component: {error}"),
    }

    // No Tick after replace. Mail the reconstructed children at the aliases
    // typed spawn assigned before the swap.
    let tabs = child_address("tabs");
    let dropdown = child_address("dropdown");
    let menu = child_address("menu");
    harness
        .execute(vec![
            ("tabs_right", HarnessOp::send_and_settle(&tabs, &Key { window: TEST_WINDOW_ID, code: KEY_RIGHT })),
            ("dropdown_enter", HarnessOp::send_and_settle(&dropdown, &Key { window: TEST_WINDOW_ID, code: KEY_ENTER })),
            (
                "dropdown_enter_up",
                HarnessOp::send_and_settle(&dropdown, &KeyRelease { window: TEST_WINDOW_ID, code: KEY_ENTER }),
            ),
            ("dropdown_down", HarnessOp::send_and_settle(&dropdown, &Key { window: TEST_WINDOW_ID, code: KEY_DOWN })),
            (
                "dropdown_commit",
                HarnessOp::send_and_settle(&dropdown, &Key { window: TEST_WINDOW_ID, code: KEY_ENTER }),
            ),
            (
                "menu_frame",
                HarnessOp::send_and_settle(&menu, &WidgetFrame { x: 10.0, y: 10.0, width: 200.0, height: 24.0 }),
            ),
            ("menu_press", HarnessOp::send_and_settle(&menu, &press(20.0, 20.0))),
            ("menu_release", HarnessOp::send_and_settle(&menu, &release(20.0, 20.0))),
            ("menu_enter", HarnessOp::send_and_settle(&menu, &Key { window: TEST_WINDOW_ID, code: KEY_ENTER })),
        ])
        .unwrap_or_else(|error| {
            panic!(
                "{stem}: post-replace mail to original child aliases must dispatch; unknown mailbox here means \
                 reconstruct dropped the child: {error}"
            )
        });

    let log = panel_log_messages(&mut harness);
    let joined = log.join("\n");
    assert!(
        log.iter().any(|message| message.contains("widget tab selected") && message.contains("index=1")),
        "{stem}: reconstructed TabStrip at the original alias must still step and report to the panel; log was:\n{joined}"
    );
    assert!(
        log.iter().any(|message| message.contains("widget dropdown selected") && message.contains("index=1")),
        "{stem}: reconstructed Dropdown at the original alias must still open, step, and commit; log was:\n{joined}"
    );
    assert!(
        log.iter().any(|message| message.contains("widget menu item activated")
            && message.contains("menu=0")
            && message.contains("item=0")),
        "{stem}: reconstructed MenuBar at the original alias must still open and activate; log was:\n{joined}"
    );
}

#[test]
fn default_wasm_exports_the_seven_by_selector() {
    let Some(wasm) = wasm_or_skip(DEFAULT_STEM) else {
        return;
    };
    assert_selectors(&wasm, DEFAULT_STEM);
}

#[test]
fn behavior_wasm_exports_the_seven_by_selector() {
    let Some(wasm) = wasm_or_skip(BEHAVIOR_STEM) else {
        return;
    };
    assert_selectors(&wasm, BEHAVIOR_STEM);
}

#[test]
fn default_wasm_reconstructs_panel_children_across_replace() {
    let Some(wasm) = wasm_or_skip(DEFAULT_STEM) else {
        return;
    };
    assert_panel_children_reconstruct(&wasm, DEFAULT_STEM);
}

#[test]
fn behavior_wasm_reconstructs_panel_children_across_replace() {
    let Some(wasm) = wasm_or_skip(BEHAVIOR_STEM) else {
        return;
    };
    assert_panel_children_reconstruct(&wasm, BEHAVIOR_STEM);
}
