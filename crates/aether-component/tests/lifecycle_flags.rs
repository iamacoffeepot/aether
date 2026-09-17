//! Lifecycle admission coverage for native slot flags and both mail routes.
//! The host load path is exercised with real wasm fixtures; trampoline unit
//! tests cover private stable-state transitions.

use std::collections::HashMap;
use std::fs;

use aether_actor::Addressable;
use aether_component::{LifecycleFlags, WasmTrampoline, resolve_embedded};
use aether_data::MailboxId;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{DropComponent, DropResult, LoadComponent, LoadResult, Ping, ReplaceComponent, ReplaceResult};

fn load(
    harness: &mut SubstrateHarness,
    wasm: &[u8],
    name: Option<&str>,
    export: Option<&str>,
    parent: Option<&str>,
) -> (MailboxId, String) {
    let request = LoadComponent {
        wasm: wasm.to_vec(),
        name: name.map(str::to_owned),
        config: Vec::new(),
        export: export.map(str::to_owned),
    };
    let operation = match parent {
        Some(parent) => HarnessOp::load_component_under(parent, request),
        None => HarnessOp::send_and_await_reply("aether.component", &request),
    };
    let result = harness.execute(vec![("load", operation)]).expect("load sequence");
    match result.reply::<LoadResult>("load").expect("decode load reply") {
        LoadResult::Ok { mailbox_id, name, .. } => (mailbox_id, name),
        LoadResult::Err { error } => panic!("load failed: {error}"),
    }
}

fn drop_result(harness: &mut SubstrateHarness, address: &str, mailbox_id: MailboxId) -> DropResult {
    harness
        .execute(vec![("drop", HarnessOp::send_and_await_reply(address, &DropComponent { mailbox_id }))])
        .expect("drop sequence")
        .reply::<DropResult>("drop")
        .expect("decode drop reply")
}

fn assert_drop_prohibited(result: DropResult) {
    let DropResult::Err { error } = result else {
        panic!("protected component accepted individual drop");
    };
    assert!(error.contains("prohibited"), "unexpected drop failure: {error}");
}

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

#[test]
fn configured_slot_rejects_direct_and_forwarded_drop_after_replace() {
    let Some(path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let wasm = fs::read(path).expect("read fixture");
    let protected_slot = resolve_embedded("protected");
    let mut harness = SubstrateHarness::builder()
        .size(64, 48)
        .with_component_host_restrictions(HashMap::from([(protected_slot, LifecycleFlags::DROP)]))
        .build()
        .expect("boot");

    let (mailbox_id, name) = load(&mut harness, &wasm, Some("protected"), None, None);
    assert_eq!(mailbox_id, protected_slot);
    assert_drop_prohibited(drop_result(&mut harness, &name, mailbox_id));
    assert_drop_prohibited(drop_result(&mut harness, "aether.component", mailbox_id));

    let replacement = harness
        .execute(vec![(
            "replace",
            HarnessOp::send_and_await_reply(
                &name,
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
        replacement.reply::<ReplaceResult>("replace").expect("decode replace reply"),
        ReplaceResult::Ok { .. }
    ));
    assert_drop_prohibited(drop_result(&mut harness, &name, mailbox_id));
    assert_drop_prohibited(drop_result(&mut harness, "aether.component", mailbox_id));

    let (ordinary_mailbox, ordinary_name) = load(&mut harness, &wasm, Some("ordinary"), None, None);
    assert_ne!(ordinary_mailbox, protected_slot);
    assert!(matches!(drop_result(&mut harness, &ordinary_name, ordinary_mailbox), DropResult::Ok));
}

#[test]
fn resolved_default_name_is_protected_but_guest_sibling_is_not() {
    let Some(path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let wasm = fs::read(path).expect("read fixture");
    let protected_slot = resolve_embedded("test.ui.root");
    let mut harness = SubstrateHarness::builder()
        .size(64, 48)
        .with_component_host_restrictions(HashMap::from([(protected_slot, LifecycleFlags::DROP)]))
        .build()
        .expect("boot");

    let (mailbox_id, name) = load(&mut harness, &wasm, None, Some("test.ui.root"), None);
    assert_eq!(mailbox_id, protected_slot);
    assert_drop_prohibited(drop_result(&mut harness, &name, mailbox_id));

    harness.execute(vec![("spawn", HarnessOp::send_and_settle(&name, &Ping { seq: 0 }))]).expect("spawn sibling");
    let sibling_mailbox = WasmTrampoline::resolve(mailbox_id.0, "0");
    let sibling_name = format!("{name}/aether.embedded:0");
    assert!(matches!(drop_result(&mut harness, &sibling_name, sibling_mailbox), DropResult::Ok));
}

#[test]
fn scoped_load_uses_its_actual_parent_for_slot_restrictions() {
    let Some(path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let wasm = fs::read(path).expect("read fixture");
    let protected_slot = resolve_embedded("scoped");
    let mut harness = SubstrateHarness::builder()
        .size(64, 48)
        .with_component_host_restrictions(HashMap::from([(protected_slot, LifecycleFlags::DROP)]))
        .build()
        .expect("boot");

    let (_, parent_name) = load(&mut harness, &wasm, Some("parent"), None, None);
    let (nested_mailbox, nested_name) = load(&mut harness, &wasm, Some("scoped"), None, Some(&parent_name));
    assert_ne!(nested_mailbox, protected_slot);
    assert!(matches!(drop_result(&mut harness, &nested_name, nested_mailbox), DropResult::Ok));

    let (root_mailbox, root_name) = load(&mut harness, &wasm, Some("scoped"), None, Some("aether.component"));
    assert_eq!(root_mailbox, protected_slot);
    assert_drop_prohibited(drop_result(&mut harness, &root_name, root_mailbox));
}

#[test]
fn module_boot_actor_does_not_inherit_requested_slot_restrictions() {
    let Some(path) = require_wasm("aether_test_fixtures_boot") else {
        return;
    };
    let wasm = fs::read(path).expect("read boot fixture");
    let protected_slot = resolve_embedded("protected-widget");
    let mut harness = SubstrateHarness::builder()
        .size(64, 48)
        .with_component_host_restrictions(HashMap::from([(protected_slot, LifecycleFlags::DROP)]))
        .build()
        .expect("boot");

    let (mailbox_id, name) =
        load(&mut harness, &wasm, Some("protected-widget"), Some("aether.test.boot.widget_a"), None);
    assert_eq!(mailbox_id, protected_slot);
    assert_drop_prohibited(drop_result(&mut harness, &name, mailbox_id));

    let boot_mailbox = resolve_embedded("aether.test.boot.boot");
    assert_ne!(boot_mailbox, protected_slot);
    assert!(matches!(
        drop_result(&mut harness, "aether.component/aether.embedded:aether.test.boot.boot", boot_mailbox),
        DropResult::Ok
    ));
}
