//! Lifecycle admission coverage for the public native flags and the two mail
//! routes. Protected trampoline behavior is exercised with a real wasm fixture
//! by the runtime's unit tests, which can construct its private stable state.

use std::fs;

use aether_component::LifecycleFlags;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{DropComponent, DropResult, LoadComponent, LoadResult, ReplaceComponent, ReplaceResult};

#[test]
fn lifecycle_flags_combine_without_overlap() {
    let mut prohibit = LifecycleFlags::DROP;
    prohibit |= LifecycleFlags::REPLACE;
    assert_eq!(prohibit, LifecycleFlags::DROP | LifecycleFlags::REPLACE);
    assert!(prohibit.contains(LifecycleFlags::DROP));
    assert!(prohibit.contains(LifecycleFlags::REPLACE));
    assert!(!LifecycleFlags::NONE.contains(LifecycleFlags::DROP));
    assert_eq!(LifecycleFlags::default(), LifecycleFlags::NONE);
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
