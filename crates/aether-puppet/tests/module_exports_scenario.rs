//! ADR-0096/ADR-0138 gate for the puppet subsystem's merged artifact.
//!
//! The wasm is deliberately defaultless: a caller must select Puppet,
//! Idle, or Turntable, and each selector must expose that actor's own
//! receive/config surface from the same compiled module.

use std::fs;

use aether_actor::Addressable;
use aether_data::Kind;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{LoadComponent, LoadResult};
use aether_puppet::{Expression, EyeArchetype, Gaze, Idle, IdleConfig, Puppet, Turntable, TurntableConfig, Viseme};

const PUPPET_EXPORT: &str = <Puppet as Addressable>::NAMESPACE;
const IDLE_EXPORT: &str = <Idle as Addressable>::NAMESPACE;
const TURNTABLE_EXPORT: &str = <Turntable as Addressable>::NAMESPACE;

#[test]
fn one_artifact_serves_all_three_explicit_exports() {
    let Some(wasm_path) = require_wasm("aether_puppet") else {
        return;
    };
    let wasm = fs::read(wasm_path).expect("read the puppet wasm");
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");

    let bare = harness
        .execute(vec![(
            "bare",
            HarnessOp::send_and_await_reply(
                "aether.component",
                &LoadComponent { wasm: wasm.clone(), name: None, config: Vec::new(), export: None },
            ),
        )])
        .expect("bare load sequence");
    match bare.reply::<LoadResult>("bare").expect("decode bare LoadResult") {
        LoadResult::Err { error } => {
            for export in [PUPPET_EXPORT, IDLE_EXPORT, TURNTABLE_EXPORT] {
                assert!(error.contains(export), "defaultless load error must name {export}; got {error}");
            }
        }
        LoadResult::Ok { name, .. } => panic!("the defaultless module unexpectedly loaded {name}"),
    }

    for (label, export, config) in [
        ("puppet", PUPPET_EXPORT, None),
        ("idle", IDLE_EXPORT, Some(<IdleConfig as Kind>::NAME)),
        ("turntable", TURNTABLE_EXPORT, Some(<TurntableConfig as Kind>::NAME)),
    ] {
        let loaded = harness
            .execute(vec![(
                label,
                HarnessOp::send_and_await_reply(
                    "aether.component",
                    &LoadComponent {
                        wasm: wasm.clone(),
                        name: None,
                        config: Vec::new(),
                        export: Some(export.to_owned()),
                    },
                ),
            )])
            .expect("selected load sequence");
        match loaded.reply::<LoadResult>(label).expect("decode selected LoadResult") {
            LoadResult::Ok { name, capabilities, .. } => {
                assert!(name.ends_with(&format!(":{export}")), "selector {export} registered as {name}");
                assert!(!capabilities.handlers.is_empty(), "selector {export} exposed no handlers");
                if export == PUPPET_EXPORT {
                    for (kind, id) in [
                        (Expression::NAME, Expression::ID),
                        (Gaze::NAME, Gaze::ID),
                        (Viseme::NAME, Viseme::ID),
                        (EyeArchetype::NAME, EyeArchetype::ID),
                    ] {
                        assert!(
                            capabilities.handlers.iter().any(|handler| handler.id == id),
                            "selector {export} did not expose {kind}; handlers: {:?}",
                            capabilities.handlers,
                        );
                    }
                }
                assert_eq!(
                    capabilities.config.as_ref().map(|declared| declared.name.as_str()),
                    config,
                    "selector {export} exposed the wrong config contract",
                );
            }
            LoadResult::Err { error } => panic!("selector {export} failed to load: {error}"),
        }
    }
}
