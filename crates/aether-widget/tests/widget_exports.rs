//! Export and replace-reconstruct coverage for the widget module's `export!`
//! lists (issue 5538, issue 7206).
//!
//! The widget wasm build is a grab-bag defaultless module (ADR-0138). Only its
//! roots are `public`; the set widgets are `private`, because a root-loaded
//! one has no in-cluster parent to send its draw list to. ADR-0114 §5
//! reconstructs inline children from both lists — so a panel-spawned
//! Dropdown, `TabStrip`, or `MenuBar` missing from `private` would vanish
//! across `replace_component` even though typed spawn still works on a cold
//! Tick.
//!
//! Reconstruction assertions send input to the original child aliases
//! *without* a post-replace Tick: `WidgetPanel` does not persist `spawned`,
//! so a later Tick would re-run typed `spawn_inline_child` and mask a
//! reconstruct miss. The stock widgets declare no `type Persist`, so these
//! tests prove post-replace behavior at the original aliases, not that a
//! live selection survived the swap.
//!
//! Skipped when no wgpu adapter is available or the matching wasm has not been
//! pre-built (the shared `require_runtime` gate): the widget module declares
//! render, and only a real render serves it. CI sets
//! `AETHER_REQUIRE_RUNTIME=1` to turn either skip into a hard failure.
//! The parent builds `aether_widget`.

mod support;

use std::fs;

use aether_actor::ActorRef;
use aether_component::ComponentHostCapability;
use aether_data::{Kind, LoadName};
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_harness_substrate_capture::test_helpers::{init_save_sandbox, require_runtime, test_namespace_roots};
use aether_kinds::keycode::{KEY_DOWN, KEY_ENTER, KEY_RIGHT};
use aether_kinds::mouse_button::LEFT;
use aether_kinds::{Key, KeyRelease, LoadComponent, LoadResult, LogTailResult, MouseButton, MouseButtonRelease, Tick};
use aether_substrate::testing::successor_wasm;
use aether_widget::set::{DropdownWidget, MenuBarWidget, TabStripWidget};
use aether_widget::{
    DropdownConfig, Menu, MenuBarConfig, MenuItem, PanelConfig, TabStripConfig, Theme, WidgetChildSpec,
    WidgetControlState, WidgetFrame, WidgetKind, WidgetPanel,
};
use support::widget_caps;

const DEFAULT_STEM: &str = "aether_widget";

/// The window the injected input events name.
fn test_window() -> aether_data::ErasedActorPath {
    aether_window::window_path(&LoadName::new("main").expect("a valid window name"))
}

/// A GPU bench with the component host and everything the widget module
/// declares: the real render, text (its fs from the sandbox roots) and the
/// in-memory clipboard.
fn bench(width: u32, height: u32) -> SubstrateHarness {
    widget_caps(
        SubstrateHarness::builder()
            .size(width, height)
            .namespace_roots(test_namespace_roots(init_save_sandbox("widget-exports")))
            .with_render()
            .with_component_host(),
    )
    .build()
    .expect("boot")
}

/// NAMESPACEs of the five roots the `public` list exports.
const PUBLIC_ROOTS: [&str; 5] = [
    "aether.widget",
    "aether.widget.scroll",
    "aether.widget.editor",
    "aether.widget.editor_region",
    "aether.widget.panel",
];

fn wasm_or_skip(stem: &str) -> Option<Vec<u8>> {
    let path = require_runtime(stem)?;
    Some(fs::read(&path).unwrap_or_else(|error| panic!("read {stem} wasm: {error}")))
}

fn key(subname: &str) -> LoadName {
    LoadName::new(subname).expect("a valid child subname")
}

fn panel_log_messages(harness: &mut SubstrateHarness, panel: ActorRef<WidgetPanel>) -> Vec<String> {
    match harness.log_tail(&panel, None, None) {
        LogTailResult::Ok { entries, .. } => entries.into_iter().map(|entry| entry.message).collect(),
        LogTailResult::Err { error } => panic!("log_tail on the panel failed: {error}"),
    }
}

fn press(x: f32, y: f32) -> MouseButton {
    MouseButton { window: test_window(), button: LEFT, x, y }
}

fn release(x: f32, y: f32) -> MouseButtonRelease {
    MouseButtonRelease { window: test_window(), button: LEFT, x, y }
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
        editor_region: String::new(),
    }
}

/// ADR-0138: a bare load of this grab-bag must error and name every public
/// root a caller can select, and no set widget, which is listed only so a
/// replace rebuilds it.
fn assert_selectors(wasm: &[u8], stem: &str) {
    let mut harness = bench(64, 48);

    let bare = harness
        .execute(vec![(
            "bare",
            HarnessOp::send_and_await_reply(
                &harness.actor_ref::<ComponentHostCapability>(),
                &LoadComponent { wasm: wasm.to_vec(), name: None, config: Vec::new(), export: None },
            ),
        )])
        .expect("bare load sequence");
    match bare.reply::<LoadResult>("bare").expect("decode bare LoadResult") {
        LoadResult::Err { error } => {
            for export in PUBLIC_ROOTS {
                assert!(
                    error.contains(export),
                    "{stem}: bare-load error must name {export} so a caller can select it; got {error}"
                );
            }
            assert!(
                !error.contains("aether.widget.dropdown"),
                "{stem}: a private set widget is not a selectable export; got {error}"
            );
        }
        LoadResult::Ok { path: name, .. } => {
            panic!("{stem}: a bare load of the widget module must error, not instantiate {name}")
        }
    }
}

/// ADR-0114 §5: panel-reachable Dropdown / `TabStrip` / `MenuBar` children keep
/// handling their kinds at the original aliases after replace, without a
/// post-replace Tick that would re-spawn them through typed spawn.
fn assert_panel_children_reconstruct(wasm: &[u8], stem: &str) {
    let config = panel_config();
    let config_bytes = config.encode_into_bytes();
    let mut harness = bench(240, 220);

    let panel = harness
        .load::<WidgetPanel>(LoadComponent {
            wasm: wasm.to_vec(),
            name: Some("panel".to_owned()),
            config: config_bytes,
            export: None,
        })
        .unwrap_or_else(|error| panic!("{stem}: load WidgetPanel: {error}"));

    harness
        .execute(vec![("spawn", HarnessOp::send_and_settle(&panel, &Tick::default()))])
        .expect("first tick spawns the declared children");

    // Identical code under a new hash, so the republish swaps the panel rather
    // than answering unchanged (ADR-0241 §7). The panel keeps its stored spawn
    // config.
    if let Err(error) = harness.publish(successor_wasm(wasm, 1)) {
        panic!("{stem}: publish successor: {error}");
    }

    // No Tick after replace. Mail the reconstructed children at the aliases
    // typed spawn assigned before the swap; a child reconstruct dropped has no
    // live alias to look up.
    let tabs = harness.child::<WidgetPanel, TabStripWidget>(&panel, key("tabs")).expect("tabs reconstructed");
    let dropdown =
        harness.child::<WidgetPanel, DropdownWidget>(&panel, key("dropdown")).expect("dropdown reconstructed");
    let menu = harness.child::<WidgetPanel, MenuBarWidget>(&panel, key("menu")).expect("menu reconstructed");
    harness
        .execute(vec![
            ("tabs_right", HarnessOp::send_and_settle(&tabs, &Key { window: test_window(), code: KEY_RIGHT })),
            ("dropdown_enter", HarnessOp::send_and_settle(&dropdown, &Key { window: test_window(), code: KEY_ENTER })),
            (
                "dropdown_enter_up",
                HarnessOp::send_and_settle(&dropdown, &KeyRelease { window: test_window(), code: KEY_ENTER }),
            ),
            ("dropdown_down", HarnessOp::send_and_settle(&dropdown, &Key { window: test_window(), code: KEY_DOWN })),
            ("dropdown_commit", HarnessOp::send_and_settle(&dropdown, &Key { window: test_window(), code: KEY_ENTER })),
            (
                "menu_frame",
                HarnessOp::send_and_settle(&menu, &WidgetFrame { x: 10.0, y: 10.0, width: 200.0, height: 24.0 }),
            ),
            ("menu_press", HarnessOp::send_and_settle(&menu, &press(20.0, 20.0))),
            ("menu_release", HarnessOp::send_and_settle(&menu, &release(20.0, 20.0))),
            ("menu_enter", HarnessOp::send_and_settle(&menu, &Key { window: test_window(), code: KEY_ENTER })),
        ])
        .unwrap_or_else(|error| panic!("{stem}: post-replace mail to original child aliases must dispatch: {error}"));

    let log = panel_log_messages(&mut harness, panel);
    let joined = log.join("\n");
    assert!(
        log.iter().any(|message| message.contains("widget tab strip selected") && message.contains("index=1")),
        "{stem}: reconstructed TabStrip at the original alias must still step and report to the panel; log was:\n{joined}"
    );
    assert!(
        log.iter().any(|message| message.contains("widget dropdown selected") && message.contains("index=1")),
        "{stem}: reconstructed Dropdown at the original alias must still open, step, and commit; log was:\n{joined}"
    );
    assert!(
        log.iter().any(|message| message.contains("widget menu bar activated")
            && message.contains("menu=0")
            && message.contains("item=0")),
        "{stem}: reconstructed MenuBar at the original alias must still open and activate; log was:\n{joined}"
    );
}

#[test]
fn default_wasm_exports_only_the_roots_by_selector() {
    let Some(wasm) = wasm_or_skip(DEFAULT_STEM) else {
        return;
    };
    assert_selectors(&wasm, DEFAULT_STEM);
}

#[test]
fn default_wasm_reconstructs_panel_children_across_replace() {
    let Some(wasm) = wasm_or_skip(DEFAULT_STEM) else {
        return;
    };
    assert_panel_children_reconstruct(&wasm, DEFAULT_STEM);
}
