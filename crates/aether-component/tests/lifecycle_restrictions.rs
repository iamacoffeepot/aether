//! Lifecycle admission coverage for the public native flags and the two mail
//! routes. Protected trampoline behavior is exercised with a real wasm fixture
//! by the runtime's unit tests, which can construct its private stable state.

use std::fs;

use aether_component::ComponentRestrictions;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{DropComponent, DropResult, LoadComponent, LoadResult, ReplaceComponent, ReplaceResult};

#[test]
fn restriction_flags_combine_without_overlap() {
    let mut prohibit = ComponentRestrictions::DROP;
    prohibit |= ComponentRestrictions::REPLACE;
    assert_eq!(prohibit, ComponentRestrictions::DROP | ComponentRestrictions::REPLACE);
    assert!(prohibit.contains(ComponentRestrictions::DROP));
    assert!(prohibit.contains(ComponentRestrictions::REPLACE));
    assert!(!ComponentRestrictions::NONE.contains(ComponentRestrictions::DROP));
    assert_eq!(ComponentRestrictions::default(), ComponentRestrictions::NONE);
}

#[test]
fn unrestricted_components_accept_direct_and_forwarded_lifecycle_mail() {
    let Some(path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let wasm = fs::read(path).expect("read fixture");
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");

    for (label, replace_address, drop_address) in [("direct-replace", true, false), ("forwarded-replace", false, true)]
    {
        let loaded = harness
            .execute(vec![(
                "load",
                HarnessOp::send_and_await_reply(
                    "aether.component",
                    &LoadComponent {
                        wasm: wasm.clone(),
                        name: Some(label.to_owned()),
                        config: Vec::new(),
                        export: None,
                    },
                ),
            )])
            .expect("load sequence");
        let (mailbox_id, name) = match loaded.reply::<LoadResult>("load").expect("decode load reply") {
            LoadResult::Ok { mailbox_id, name, .. } => (mailbox_id, name),
            LoadResult::Err { error } => panic!("load failed: {error}"),
        };

        let replace_target = if replace_address {
            name.as_str()
        } else {
            "aether.component"
        };
        let replaced = harness
            .execute(vec![(
                "replace",
                HarnessOp::send_and_await_reply(
                    replace_target,
                    &ReplaceComponent {
                        mailbox_id,
                        wasm: wasm.clone(),
                        drain_timeout_ms: None,
                        config: Vec::new(),
                        export: None,
                    },
                ),
            )])
            .expect("replace sequence");
        assert!(matches!(
            replaced.reply::<ReplaceResult>("replace").expect("decode replace reply"),
            ReplaceResult::Ok { .. }
        ));

        let drop_target = if drop_address {
            name.as_str()
        } else {
            "aether.component"
        };
        let dropped = harness
            .execute(vec![("drop", HarnessOp::send_and_await_reply(drop_target, &DropComponent { mailbox_id }))])
            .expect("drop sequence");
        assert!(matches!(dropped.reply::<DropResult>("drop").expect("decode drop reply"), DropResult::Ok));
    }
}
