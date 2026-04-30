use super::*;
use aether_hub_protocol::{Primitive, SchemaType};
use aether_kinds::{HandlerCapability, Key, KeyRelease, MouseButton, MouseMove, Tick, WindowSize};
use aether_mail::KindId;

#[test]
fn load_payload_roundtrip() {
    let p = LoadComponent {
        wasm: vec![0, 1, 2, 3],
        name: Some("hello".into()),
    };
    let bytes = postcard::to_allocvec(&p).unwrap();
    let back: LoadComponent = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(back.wasm, p.wasm);
    assert_eq!(back.name.as_deref(), Some("hello"));
}

#[test]
fn load_result_roundtrip() {
    for r in [
        LoadResult::Ok {
            mailbox_id: MailboxId(7),
            name: "x".into(),
            capabilities: ComponentCapabilities::default(),
        },
        LoadResult::Err {
            error: "nope".into(),
        },
    ] {
        let bytes = postcard::to_allocvec(&r).unwrap();
        let _back: LoadResult = postcard::from_bytes(&bytes).unwrap();
    }
}

/// Minimal WAT module satisfying the substrate's component
/// contract: exports `memory`, a `receive(i32,i32,i32,i32) -> i32`
/// that returns 0, and no `init`.
const WAT: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                i32.const 0))
    "#;

/// WAT with lifecycle hooks. Each hook writes a marker to a
/// distinct offset in linear memory so tests can observe which
/// hook fired. `on_replace` writes 0x11 at offset 200;
/// `on_drop` writes 0x22 at offset 204.
const WAT_HOOKS: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                i32.const 0)
            (func (export "on_replace") (result i32)
                i32.const 200
                i32.const 0x11
                i32.store
                i32.const 0)
            (func (export "on_drop") (result i32)
                i32.const 204
                i32.const 0x22
                i32.store
                i32.const 0))
    "#;

/// WAT where `on_drop` traps via `unreachable`. Used to verify
/// that a panicking hook does not stall teardown.
const WAT_TRAPS_ON_DROP: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                i32.const 0)
            (func (export "on_drop") (result i32)
                unreachable))
    "#;

/// ADR-0016 save side: `on_replace` saves 4 bytes of 0xDEADBEEF
/// with schema version 7.
#[allow(dead_code)]
const WAT_SAVES_STATE: &str = r#"
        (module
            (import "aether" "save_state_p32"
                (func $save_state (param i32 i32 i32) (result i32)))
            (memory (export "memory") 1)
            (data (i32.const 300) "\de\ad\be\ef")
            (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                i32.const 0)
            (func (export "on_replace") (result i32)
                (drop (call $save_state
                    (i32.const 7)
                    (i32.const 300)
                    (i32.const 4)))
                i32.const 0))
    "#;

/// ADR-0016 save side: attempts a 2 MiB save, which the substrate
/// rejects over the 1 MiB cap. `save_state` returns status 3 and
/// the ctx error slot is populated.
const WAT_SAVES_TOO_LARGE: &str = r#"
        (module
            (import "aether" "save_state_p32"
                (func $save_state (param i32 i32 i32) (result i32)))
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                i32.const 0)
            (func (export "on_replace") (result i32)
                (drop (call $save_state
                    (i32.const 1)
                    (i32.const 0)
                    (i32.const 0x00200000)))
                i32.const 0))
    "#;

/// ADR-0016 load side: `on_rehydrate` copies the bundle bytes to
/// offset 400 and stores the version at offset 396. Used to prove
/// migration end-to-end when paired with `WAT_SAVES_STATE`.
#[allow(dead_code)]
const WAT_REHYDRATES: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                i32.const 0)
            (func (export "on_rehydrate_p32") (param i32 i32 i32) (result i32)
                i32.const 396
                local.get 0
                i32.store
                i32.const 400
                local.get 1
                local.get 2
                memory.copy
                i32.const 0))
    "#;

fn make_plane() -> ControlPlane {
    let engine = Arc::new(Engine::default());
    let mut linker: Linker<SubstrateCtx> = Linker::new(&engine);
    crate::host_fns::register(&mut linker).expect("register host fns");
    let registry = Arc::new(Registry::new());
    let queue = Arc::new(Mailer::new());
    let outbound = HubOutbound::disconnected();
    let components: ComponentTable = Arc::default();

    ControlPlane {
        engine,
        linker: Arc::new(linker),
        registry,
        queue,
        outbound,
        components,
        input_subscribers: input::new_subscribers(),
        default_name_counter: Arc::new(AtomicU64::new(0)),
        chassis_handler: None,
    }
}

#[test]
fn load_component_instantiates_and_registers() {
    let plane = make_plane();
    let wasm = wat::parse_str(WAT).expect("compile WAT");
    let payload = LoadComponent {
        wasm,
        name: Some("loaded".into()),
    };
    let result = plane.handle_load(&postcard::to_allocvec(&payload).unwrap());
    match result {
        LoadResult::Ok {
            mailbox_id,
            name,
            capabilities: _,
        } => {
            assert_eq!(name, "loaded");
            assert_eq!(plane.registry.lookup("loaded"), Some(mailbox_id));
            assert!(plane.components.read().unwrap().contains_key(&mailbox_id));
        }
        LoadResult::Err { error } => panic!("load should succeed: {error}"),
    }
}

#[test]
fn load_component_defaults_name_on_absent() {
    let plane = make_plane();
    let wasm = wat::parse_str(WAT).unwrap();
    let payload = LoadComponent { wasm, name: None };
    let result = plane.handle_load(&postcard::to_allocvec(&payload).unwrap());
    match result {
        LoadResult::Ok { name, .. } => {
            assert!(name.starts_with("component_"), "got {name:?}");
        }
        LoadResult::Err { error } => panic!("load should succeed: {error}"),
    }
}

#[test]
fn load_component_rejects_name_conflict() {
    let plane = make_plane();
    plane.registry.register_component("taken");
    let wasm = wat::parse_str(WAT).unwrap();
    let payload = LoadComponent {
        wasm,
        name: Some("taken".into()),
    };
    let result = plane.handle_load(&postcard::to_allocvec(&payload).unwrap());
    assert!(matches!(result, LoadResult::Err { .. }));
}

#[test]
fn load_component_rejects_invalid_wasm() {
    let plane = make_plane();
    let payload = LoadComponent {
        wasm: vec![0, 1, 2, 3],
        name: Some("bad_wasm".into()),
    };
    let result = plane.handle_load(&postcard::to_allocvec(&payload).unwrap());
    assert!(matches!(result, LoadResult::Err { .. }));
}

/// Issue 358: a wasm whose `init` traps must not leave a ghost
/// mailbox in the registry. The first load fails (instantiate
/// trapped); a second load with the *same* name is expected to
/// succeed because the registry was never published the failed
/// id.
#[test]
fn load_component_init_trap_does_not_reserve_name() {
    let plane = make_plane();
    // `init` traps via `unreachable`. Matches the legacy single-arg
    // init signature `init() -> i32` so wasmtime resolves the typed
    // func and runs the body — which then traps.
    const WAT_INIT_TRAPS: &str = r#"
            (module
                (memory (export "memory") 1)
                (func (export "init") (result i32)
                    unreachable)
                (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                    i32.const 0))
        "#;
    let wasm = wat::parse_str(WAT_INIT_TRAPS).unwrap();
    let first = plane.handle_load(
        &postcard::to_allocvec(&LoadComponent {
            wasm: wasm.clone(),
            name: Some("trap_init".into()),
        })
        .unwrap(),
    );
    assert!(
        matches!(first, LoadResult::Err { .. }),
        "first load should fail with init trap, got {first:?}",
    );
    assert!(
        plane.registry.lookup("trap_init").is_none(),
        "failed instantiate must not leave a ghost mailbox",
    );

    // Second load: same name, but a healthy module. Without the
    // fix, the failed first load would have reserved "trap_init"
    // and this would fail with a name conflict.
    let healthy = wat::parse_str(WAT).unwrap();
    let second = plane.handle_load(
        &postcard::to_allocvec(&LoadComponent {
            wasm: healthy,
            name: Some("trap_init".into()),
        })
        .unwrap(),
    );
    assert!(
        matches!(second, LoadResult::Ok { .. }),
        "second load with same name should succeed, got {second:?}",
    );
}

/// Issue 358: a wasm with a missing import fails at
/// `linker.instantiate`, before `init` even runs. Same invariant:
/// the registry must not retain a ghost mailbox.
#[test]
fn load_component_missing_import_does_not_reserve_name() {
    let plane = make_plane();
    // Import a host fn the substrate's linker does not provide.
    const WAT_MISSING_IMPORT: &str = r#"
            (module
                (import "nonexistent" "missing_fn" (func))
                (memory (export "memory") 1)
                (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                    i32.const 0))
        "#;
    let wasm = wat::parse_str(WAT_MISSING_IMPORT).unwrap();
    let first = plane.handle_load(
        &postcard::to_allocvec(&LoadComponent {
            wasm,
            name: Some("missing_import".into()),
        })
        .unwrap(),
    );
    assert!(
        matches!(first, LoadResult::Err { .. }),
        "first load should fail with missing import, got {first:?}",
    );
    assert!(
        plane.registry.lookup("missing_import").is_none(),
        "failed instantiate must not leave a ghost mailbox",
    );

    let healthy = wat::parse_str(WAT).unwrap();
    let second = plane.handle_load(
        &postcard::to_allocvec(&LoadComponent {
            wasm: healthy,
            name: Some("missing_import".into()),
        })
        .unwrap(),
    );
    assert!(
        matches!(second, LoadResult::Ok { .. }),
        "second load with same name should succeed, got {second:?}",
    );
}

/// ADR-0028 / ADR-0032 happy path: a component ships both its
/// canonical `aether.kinds` record and the paired
/// `aether.kinds.labels` sidecar. The substrate reads both and
/// registers the named kind before instantiation.
#[test]
fn load_component_registers_kinds_from_embedded_manifest() {
    let plane = make_plane();

    // Hand-roll the v0x03 records (ADR-0068 trailing is_stream byte) the derive would emit.
    let shape = aether_hub_protocol::KindShape {
        name: std::borrow::Cow::Borrowed("demo.embedded.kind"),
        schema: aether_hub_protocol::SchemaShape::Struct {
            fields: vec![aether_hub_protocol::SchemaShape::Scalar(Primitive::U32)],
            repr_c: true,
        },
    };
    let labels = aether_hub_protocol::KindLabels {
        kind_id: aether_mail::KindId(aether_hub_protocol::canonical::kind_id_from_shape(&shape)),
        kind_label: std::borrow::Cow::Borrowed("demo::EmbeddedKind"),
        root: aether_hub_protocol::LabelNode::Struct {
            type_label: Some(std::borrow::Cow::Borrowed("demo::EmbeddedKind")),
            field_names: std::borrow::Cow::Owned(vec![std::borrow::Cow::Borrowed("code")]),
            fields: std::borrow::Cow::Owned(vec![aether_hub_protocol::LabelNode::Anonymous]),
        },
    };
    let mut canonical = vec![0x03u8];
    canonical.extend(postcard::to_allocvec(&shape).unwrap());
    canonical.push(0u8); // is_stream=false trailing byte (ADR-0068)
    let mut labels_bytes = vec![0x03u8];
    labels_bytes.extend(postcard::to_allocvec(&labels).unwrap());
    let esc = |bs: &[u8]| -> String { bs.iter().map(|b| format!("\\{b:02x}")).collect() };
    let wat = format!(
        r#"(module
                (@custom "aether.kinds" "{}")
                (@custom "aether.kinds.labels" "{}")
                (memory (export "memory") 1)
                (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                    i32.const 0))"#,
        esc(&canonical),
        esc(&labels_bytes),
    );
    let wasm = wat::parse_str(wat).unwrap();

    let loaded = plane.handle_load(
        &postcard::to_allocvec(&LoadComponent {
            wasm,
            name: Some("embedded_consumer".into()),
        })
        .unwrap(),
    );
    assert!(
        matches!(loaded, LoadResult::Ok { .. }),
        "load result was {loaded:?}",
    );
    let registered_id = plane
        .registry
        .kind_id("demo.embedded.kind")
        .expect("manifest kind registered");
    let back = plane
        .registry
        .kind_descriptor(registered_id)
        .expect("descriptor recoverable");
    let SchemaType::Struct { fields, repr_c } = &back.schema else {
        panic!("expected Struct");
    };
    assert!(*repr_c);
    assert_eq!(fields[0].name, "code");
    assert_eq!(fields[0].ty, SchemaType::Scalar(Primitive::U32));
}

/// Same flow, but the embedded manifest conflicts with a kind
/// already registered with a different schema. The load aborts
/// rather than silently clobbering — same contract as the
/// legacy `LoadKind` conflict path.
#[test]
fn load_component_with_same_name_different_schema_registers_distinct_kind() {
    // ADR-0030 Phase 2: kind ids are `fnv1a(canonical(name, schema))`,
    // so two schemas under the same name produce two distinct ids
    // and coexist in the registry. The pre-Phase-2 behavior — load
    // rejected with "already registered with a different encoding"
    // — is gone; what used to be a conflict is now a clean new
    // registration. Producer/consumer parity is defended instead
    // by `K::ID` mismatch on the wire (a stale sender's mail lands
    // on "kind not found" at decode time).
    let plane = make_plane();

    let existing_id = plane
        .registry
        .register_kind_with_descriptor(KindDescriptor {
            name: "demo.conflict".into(),
            schema: SchemaType::Scalar(Primitive::U32),
            is_stream: false,
        })
        .unwrap();

    let shape = aether_hub_protocol::KindShape {
        name: std::borrow::Cow::Borrowed("demo.conflict"),
        schema: aether_hub_protocol::SchemaShape::Scalar(Primitive::U64),
    };
    let labels = aether_hub_protocol::KindLabels {
        kind_id: aether_mail::KindId(aether_hub_protocol::canonical::kind_id_from_shape(&shape)),
        kind_label: std::borrow::Cow::Borrowed("demo::Conflict"),
        root: aether_hub_protocol::LabelNode::Anonymous,
    };
    let mut canonical = vec![0x03u8];
    canonical.extend(postcard::to_allocvec(&shape).unwrap());
    canonical.push(0u8); // is_stream=false trailing byte (ADR-0068)
    let mut labels_bytes = vec![0x03u8];
    labels_bytes.extend(postcard::to_allocvec(&labels).unwrap());
    let esc = |bs: &[u8]| -> String { bs.iter().map(|b| format!("\\{b:02x}")).collect() };
    let wat = format!(
        r#"(module
                (@custom "aether.kinds" "{}")
                (@custom "aether.kinds.labels" "{}")
                (memory (export "memory") 1)
                (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                    i32.const 0))"#,
        esc(&canonical),
        esc(&labels_bytes),
    );
    let wasm = wat::parse_str(wat).unwrap();

    let result = plane.handle_load(
        &postcard::to_allocvec(&LoadComponent {
            wasm,
            name: Some("conflict_consumer".into()),
        })
        .unwrap(),
    );
    assert!(
        matches!(result, LoadResult::Ok { .. }),
        "load should succeed under hashed ids, got {result:?}"
    );

    let new_id = aether_mail::KindId(aether_hub_protocol::canonical::kind_id_from_parts(
        "demo.conflict",
        &SchemaType::Scalar(Primitive::U64),
    ));
    assert_ne!(
        existing_id, new_id,
        "u32 and u64 schemas under the same name must hash to distinct ids"
    );
    assert_eq!(
        plane.registry.kind_descriptor(existing_id).unwrap().schema,
        SchemaType::Scalar(Primitive::U32)
    );
    assert_eq!(
        plane.registry.kind_descriptor(new_id).unwrap().schema,
        SchemaType::Scalar(Primitive::U64)
    );
}

#[test]
fn drop_component_removes_component_and_frees_name() {
    let plane = make_plane();
    // Load first, then drop the same mailbox.
    let wasm = wat::parse_str(WAT).unwrap();
    let loaded = plane.handle_load(
        &postcard::to_allocvec(&LoadComponent {
            wasm,
            name: Some("victim".into()),
        })
        .unwrap(),
    );
    let LoadResult::Ok { mailbox_id, .. } = loaded else {
        panic!("load should succeed");
    };

    let dropped = plane.handle_drop(&postcard::to_allocvec(&DropComponent { mailbox_id }).unwrap());
    assert!(matches!(dropped, DropResult::Ok));
    assert!(
        plane.registry.lookup("victim").is_none(),
        "name should be released so it can be reused"
    );
    assert!(
        matches!(
            plane.registry.entry(mailbox_id),
            Some(crate::registry::MailboxEntry::Dropped),
        ),
        "entry should be marked Dropped",
    );
    assert!(
        !plane.components.read().unwrap().contains_key(&mailbox_id),
        "component must be removed from scheduler table",
    );
}

#[test]
fn drop_component_succeeds_with_outstanding_entry_arc_clone() {
    // Regression for the scheduler strand-tail race: a worker's
    // post-dispatch tail can still hold a clone of the
    // `Arc<ComponentEntry>` at the instant `handle_drop` runs,
    // because the worker's `strand_scheduled.store(false)` + Arc
    // drop happens after `mark_completed` already woke any
    // `wait_idle` the drop caller was parked on. Before the fix
    // `handle_drop` panicked in `extract_component`'s
    // `Arc::into_inner` when `strong_count > 1`.
    let plane = make_plane();
    let wasm = wat::parse_str(WAT).unwrap();
    let LoadResult::Ok { mailbox_id, .. } = plane.handle_load(
        &postcard::to_allocvec(&LoadComponent {
            wasm,
            name: Some("pinned".into()),
        })
        .unwrap(),
    ) else {
        panic!("load should succeed");
    };

    // Pin an extra Arc clone — mimics the worker's strand tail.
    let pinned = plane
        .components
        .read()
        .unwrap()
        .get(&mailbox_id)
        .cloned()
        .expect("entry must be bound after load");

    let result = plane.handle_drop(&postcard::to_allocvec(&DropComponent { mailbox_id }).unwrap());
    assert!(
        matches!(result, DropResult::Ok),
        "drop must succeed even with outstanding Arc clone",
    );

    // Cleanup: drop the extra ref so `ComponentEntry` deallocates.
    drop(pinned);
}

#[test]
fn drop_component_rejects_unknown_id() {
    let plane = make_plane();
    let result = plane.handle_drop(
        &postcard::to_allocvec(&DropComponent {
            mailbox_id: MailboxId(99),
        })
        .unwrap(),
    );
    assert!(matches!(result, DropResult::Err { .. }));
}

#[test]
fn drop_component_rejects_double_drop() {
    let plane = make_plane();
    let wasm = wat::parse_str(WAT).unwrap();
    let LoadResult::Ok { mailbox_id, .. } = plane.handle_load(
        &postcard::to_allocvec(&LoadComponent {
            wasm,
            name: Some("once".into()),
        })
        .unwrap(),
    ) else {
        panic!("load should succeed");
    };
    let args = postcard::to_allocvec(&DropComponent { mailbox_id }).unwrap();
    assert!(matches!(plane.handle_drop(&args), DropResult::Ok));
    assert!(matches!(plane.handle_drop(&args), DropResult::Err { .. }));
}

#[test]
fn replace_component_swaps_instance_and_preserves_id() {
    let plane = make_plane();
    let wasm = wat::parse_str(WAT).unwrap();
    let LoadResult::Ok {
        mailbox_id,
        name,
        capabilities: _,
    } = plane.handle_load(
        &postcard::to_allocvec(&LoadComponent {
            wasm: wasm.clone(),
            name: Some("swap_target".into()),
        })
        .unwrap(),
    )
    else {
        panic!("load should succeed");
    };
    assert_eq!(name, "swap_target");

    let result = plane.handle_replace(
        &postcard::to_allocvec(&ReplaceComponent {
            mailbox_id,
            wasm,
            drain_timeout_ms: None,
        })
        .unwrap(),
    );
    assert!(matches!(result, ReplaceResult::Ok { .. }));
    // Name still resolves to the same id; new Component bound.
    assert_eq!(plane.registry.lookup("swap_target"), Some(mailbox_id));
    assert!(plane.components.read().unwrap().contains_key(&mailbox_id));
}

#[test]
fn replace_component_rejects_unknown_target() {
    let plane = make_plane();
    let wasm = wat::parse_str(WAT).unwrap();
    let result = plane.handle_replace(
        &postcard::to_allocvec(&ReplaceComponent {
            mailbox_id: MailboxId(99),
            wasm,
            drain_timeout_ms: None,
        })
        .unwrap(),
    );
    assert!(matches!(result, ReplaceResult::Err { .. }));
}

#[test]
fn replace_component_rejects_dropped_target() {
    let plane = make_plane();
    let wasm = wat::parse_str(WAT).unwrap();
    let LoadResult::Ok { mailbox_id, .. } = plane.handle_load(
        &postcard::to_allocvec(&LoadComponent {
            wasm: wasm.clone(),
            name: Some("gone".into()),
        })
        .unwrap(),
    ) else {
        panic!();
    };
    plane.handle_drop(&postcard::to_allocvec(&DropComponent { mailbox_id }).unwrap());
    let result = plane.handle_replace(
        &postcard::to_allocvec(&ReplaceComponent {
            mailbox_id,
            wasm,
            drain_timeout_ms: None,
        })
        .unwrap(),
    );
    assert!(matches!(result, ReplaceResult::Err { .. }));
}

#[test]
fn replace_component_rejects_invalid_wasm() {
    let plane = make_plane();
    let wasm = wat::parse_str(WAT).unwrap();
    let LoadResult::Ok { mailbox_id, .. } = plane.handle_load(
        &postcard::to_allocvec(&LoadComponent {
            wasm,
            name: Some("target".into()),
        })
        .unwrap(),
    ) else {
        panic!();
    };
    let result = plane.handle_replace(
        &postcard::to_allocvec(&ReplaceComponent {
            mailbox_id,
            wasm: vec![0, 1, 2, 3],
            drain_timeout_ms: None,
        })
        .unwrap(),
    );
    assert!(matches!(result, ReplaceResult::Err { .. }));
}

#[test]
fn drop_component_with_hooks_completes_ok() {
    // WAT_HOOKS exports on_drop. handle_drop should fire it and
    // complete without error; the marker write is exercised in
    // component::tests::on_drop_invokes_export_and_writes_marker.
    let plane = make_plane();
    let wasm = wat::parse_str(WAT_HOOKS).unwrap();
    let LoadResult::Ok { mailbox_id, .. } = plane.handle_load(
        &postcard::to_allocvec(&LoadComponent {
            wasm,
            name: Some("hooked".into()),
        })
        .unwrap(),
    ) else {
        panic!("load should succeed");
    };
    let dropped = plane.handle_drop(&postcard::to_allocvec(&DropComponent { mailbox_id }).unwrap());
    assert!(matches!(dropped, DropResult::Ok));
}

#[test]
fn drop_component_with_trapping_on_drop_still_ok() {
    // ADR-0015 trap containment: a panicking hook must not stall
    // teardown. The handler logs and returns Ok regardless.
    let plane = make_plane();
    let wasm = wat::parse_str(WAT_TRAPS_ON_DROP).unwrap();
    let LoadResult::Ok { mailbox_id, .. } = plane.handle_load(
        &postcard::to_allocvec(&LoadComponent {
            wasm,
            name: Some("crasher".into()),
        })
        .unwrap(),
    ) else {
        panic!("load should succeed");
    };
    let dropped = plane.handle_drop(&postcard::to_allocvec(&DropComponent { mailbox_id }).unwrap());
    assert!(matches!(dropped, DropResult::Ok));
    // Mailbox still marked Dropped; component still removed.
    assert!(matches!(
        plane.registry.entry(mailbox_id),
        Some(crate::registry::MailboxEntry::Dropped),
    ));
}

#[test]
fn replace_component_fires_hooks_on_old_instance() {
    // handle_replace takes the write lock, fires on_replace +
    // on_drop on the old component, instantiates the new one,
    // and swaps under the same lock. Success means both hooks
    // completed without stalling the replace.
    let plane = make_plane();
    let wasm_old = wat::parse_str(WAT_HOOKS).unwrap();
    let LoadResult::Ok { mailbox_id, .. } = plane.handle_load(
        &postcard::to_allocvec(&LoadComponent {
            wasm: wasm_old,
            name: Some("swap_me".into()),
        })
        .unwrap(),
    ) else {
        panic!("load should succeed");
    };
    let wasm_new = wat::parse_str(WAT).unwrap();
    let result = plane.handle_replace(
        &postcard::to_allocvec(&ReplaceComponent {
            mailbox_id,
            wasm: wasm_new,
            drain_timeout_ms: None,
        })
        .unwrap(),
    );
    assert!(matches!(result, ReplaceResult::Ok { .. }));
}

#[test]
fn dispatch_unrecognised_kind_is_silent_drop() {
    let plane = make_plane();
    // No panic; no outbound reply. Unknown kind arriving at the
    // control mailbox just logs and moves on.
    plane.dispatch(
        aether_mail::KindId(0xdead_beef_dead_beef),
        "aether.control.does_not_exist",
        crate::mail::ReplyTo::NONE,
        &[],
    );
}

/// ADR-0016 rehydrate path + "snapshot to sink" scaffolding: the
/// replacement component both restores state via `on_rehydrate_p32`
/// and, on any incoming mail, forwards `memory[396..404]` (the
/// offsets `WAT_REHYDRATES` writes to) to the sink id encoded in
/// the first 8 bytes of the payload. Lets a test assert rehydrate
/// correctness without peeking into `ComponentEntry` internals —
/// dispatch the snapshot mail, drain, observe via the sink.
#[allow(dead_code)]
const WAT_REHYDRATES_AND_SNAPSHOT: &str = r#"
        (module
            (import "aether" "send_mail_p32"
                (func $send_mail (param i64 i64 i32 i32 i32) (result i32)))
            (memory (export "memory") 1)
            (func (export "receive_p32")
                (param $kind i64) (param $ptr i32) (param $byte_len i32) (param $count i32) (param $sender i32)
                (result i32)
                ;; Payload: [sink_id:u64]. Forward memory[396..404] to it.
                (drop (call $send_mail
                    (i64.load (local.get $ptr))
                    (local.get $kind)
                    (i32.const 396)
                    (i32.const 8)
                    (i32.const 1)))
                i32.const 0)
            (func (export "on_rehydrate_p32") (param i32 i32 i32) (result i32)
                i32.const 396
                local.get 0
                i32.store
                i32.const 400
                local.get 1
                local.get 2
                memory.copy
                i32.const 0))
    "#;

/// ADR-0016 migration end-to-end: load saves state on replace, new
/// instance rehydrates, then a follow-up snapshot mail proves the
/// rehydrate wrote `version=7` at offset 396 and `0xDEADBEEF` at
/// offset 400. Replaces the retired
/// `replace_migrates_state_from_old_to_new` test that peeked at
/// component memory directly — same intent, observation re-homed
/// onto the public send-and-observe surface per ADR-0038 Phase 1
/// follow-up.
#[test]
fn replace_migrates_state_observable_via_snapshot_sink() {
    use aether_hub_protocol::{KindDescriptor, SchemaType};
    use std::sync::Mutex as StdMutex;

    let plane = make_plane();
    // Scheduler::new is normally what wires the queue; the test
    // plane is built without one, so wire it directly so
    // `queue.push` can route the snapshot mail into the dispatcher.
    plane
        .queue
        .wire(Arc::clone(&plane.registry), Arc::clone(&plane.components));

    let snapshot_kind_id = plane
        .registry
        .register_kind_with_descriptor(KindDescriptor {
            name: "test.snapshot".into(),
            schema: SchemaType::Bytes,
            is_stream: false,
        })
        .expect("register snapshot kind");

    let captured: Arc<StdMutex<Option<Vec<u8>>>> = Arc::new(StdMutex::new(None));
    let captured_for_sink = Arc::clone(&captured);
    let sink_mbox = plane.registry.register_sink(
        "snapshot-sink",
        Arc::new(move |_kind_id, _kind, _origin, _sender, bytes, _count| {
            *captured_for_sink.lock().unwrap() = Some(bytes.to_vec());
        }),
    );

    let LoadResult::Ok { mailbox_id, .. } = plane.handle_load(
        &postcard::to_allocvec(&LoadComponent {
            wasm: wat::parse_str(WAT_SAVES_STATE).unwrap(),
            name: Some("stateful".into()),
        })
        .unwrap(),
    ) else {
        panic!("load should succeed");
    };

    let result = plane.handle_replace(
        &postcard::to_allocvec(&ReplaceComponent {
            mailbox_id,
            wasm: wat::parse_str(WAT_REHYDRATES_AND_SNAPSHOT).unwrap(),
            drain_timeout_ms: None,
        })
        .unwrap(),
    );
    assert!(matches!(result, ReplaceResult::Ok { .. }), "got {result:?}");

    // Ask the rehydrated component to emit its rehydrated-state
    // window to our sink. Payload: sink mailbox id as u64 LE.
    let payload = sink_mbox.0.to_le_bytes().to_vec();
    plane
        .queue
        .push(Mail::new(mailbox_id, snapshot_kind_id, payload, 1));
    plane.queue.drain_all();

    let bytes = captured
        .lock()
        .unwrap()
        .take()
        .expect("sink must receive snapshot");
    assert_eq!(
        bytes,
        vec![7, 0, 0, 0, 0xDE, 0xAD, 0xBE, 0xEF],
        "rehydrated memory must match saved state",
    );
}

#[test]
fn replace_aborts_when_save_state_over_cap() {
    // Old instance requests a save larger than 1 MiB; substrate
    // rejects, `handle_replace` surfaces the error, old stays live.
    let plane = make_plane();
    let LoadResult::Ok { mailbox_id, .. } = plane.handle_load(
        &postcard::to_allocvec(&LoadComponent {
            wasm: wat::parse_str(WAT_SAVES_TOO_LARGE).unwrap(),
            name: Some("greedy".into()),
        })
        .unwrap(),
    ) else {
        panic!("load should succeed");
    };

    let result = plane.handle_replace(
        &postcard::to_allocvec(&ReplaceComponent {
            mailbox_id,
            wasm: wat::parse_str(WAT).unwrap(),
            drain_timeout_ms: None,
        })
        .unwrap(),
    );
    let ReplaceResult::Err { error } = result else {
        panic!("expected replace to fail, got {result:?}");
    };
    assert!(error.contains("exceeds"), "got: {error}");
    // Old instance is still bound; name still resolves to its id.
    assert_eq!(plane.registry.lookup("greedy"), Some(mailbox_id));
    assert!(plane.components.read().unwrap().contains_key(&mailbox_id));
}

// Retired under ADR-0038 — see the comment block above
// `replace_migrates_state_from_old_to_new` for context.
#[cfg(any())]
#[test]
fn replace_without_rehydrate_hook_discards_bundle() {
    // Old saves, new doesn't implement on_rehydrate — the bundle
    // is silently discarded (ADR-0016 §3). Replace succeeds.
    let plane = make_plane();
    let LoadResult::Ok { mailbox_id, .. } = plane.handle_load(
        &postcard::to_allocvec(&LoadComponent {
            wasm: wat::parse_str(WAT_SAVES_STATE).unwrap(),
            name: Some("orphan_save".into()),
        })
        .unwrap(),
    ) else {
        panic!("load should succeed");
    };

    let result = plane.handle_replace(
        &postcard::to_allocvec(&ReplaceComponent {
            mailbox_id,
            wasm: wat::parse_str(WAT).unwrap(),
            drain_timeout_ms: None,
        })
        .unwrap(),
    );
    assert!(matches!(result, ReplaceResult::Ok { .. }), "got {result:?}");
}

// Retired under ADR-0038 — see the comment block above
// `replace_migrates_state_from_old_to_new` for context.
#[cfg(any())]
#[test]
fn replace_with_no_save_does_not_invoke_rehydrate() {
    // Old doesn't save; new has on_rehydrate but it must not
    // fire — ADR-0016 §3 says rehydrate only runs if a bundle
    // exists. The new instance's rehydrate marker offsets should
    // stay zero.
    let plane = make_plane();
    let LoadResult::Ok { mailbox_id, .. } = plane.handle_load(
        &postcard::to_allocvec(&LoadComponent {
            wasm: wat::parse_str(WAT).unwrap(),
            name: Some("stateless_old".into()),
        })
        .unwrap(),
    ) else {
        panic!("load should succeed");
    };

    let result = plane.handle_replace(
        &postcard::to_allocvec(&ReplaceComponent {
            mailbox_id,
            wasm: wat::parse_str(WAT_REHYDRATES).unwrap(),
            drain_timeout_ms: None,
        })
        .unwrap(),
    );
    assert!(matches!(result, ReplaceResult::Ok { .. }));
    let table = plane.components.read().unwrap();
    let cell = table.get(&mailbox_id).expect("present");
    let mut new = cell.component.lock().unwrap();
    assert_eq!(new.read_u32(396), 0);
    assert_eq!(new.read_u32(400), 0);
}

// ADR-0021 + ADR-0068: per-kind subscribe / unsubscribe, drop
// cleanup, replace-preserves-subscriptions. Subscriber sets are
// keyed by `KindId` directly — `make_plane` threads an empty
// `InputSubscribers` into the handler, so these tests only need to
// load a component and exercise the subscribe surface.

fn subs(plane: &ControlPlane, kind: KindId) -> std::collections::BTreeSet<MailboxId> {
    plane
        .input_subscribers
        .read()
        .unwrap()
        .get(&kind)
        .cloned()
        .unwrap_or_default()
}

fn load_blank(plane: &ControlPlane, name: &str) -> MailboxId {
    let wasm = wat::parse_str(WAT).unwrap();
    let result = plane.handle_load(
        &postcard::to_allocvec(&LoadComponent {
            wasm,
            name: Some(name.into()),
        })
        .unwrap(),
    );
    let LoadResult::Ok { mailbox_id, .. } = result else {
        panic!("load should succeed: {result:?}");
    };
    mailbox_id
}

fn do_subscribe(plane: &ControlPlane, mailbox: MailboxId, kind: KindId) -> SubscribeInputResult {
    plane.handle_subscribe(&postcard::to_allocvec(&SubscribeInput { kind, mailbox }).unwrap())
}

fn do_unsubscribe(plane: &ControlPlane, mailbox: MailboxId, kind: KindId) -> SubscribeInputResult {
    plane.handle_unsubscribe(&postcard::to_allocvec(&UnsubscribeInput { kind, mailbox }).unwrap())
}

#[test]
fn subscribe_adds_mailbox_to_stream_set() {
    let plane = make_plane();
    let id = load_blank(&plane, "listener");
    assert!(matches!(
        do_subscribe(&plane, id, Tick::ID),
        SubscribeInputResult::Ok
    ));
    let set = subs(&plane, Tick::ID);
    assert!(set.contains(&id));
    assert_eq!(set.len(), 1);
}

#[test]
fn subscribe_is_idempotent() {
    let plane = make_plane();
    let id = load_blank(&plane, "listener");
    for _ in 0..3 {
        assert!(matches!(
            do_subscribe(&plane, id, Key::ID),
            SubscribeInputResult::Ok
        ));
    }
    assert_eq!(subs(&plane, Key::ID).len(), 1);
}

#[test]
fn subscribe_two_components_fan_out_to_both() {
    let plane = make_plane();
    let a = load_blank(&plane, "a");
    let b = load_blank(&plane, "b");
    assert!(matches!(
        do_subscribe(&plane, a, Tick::ID),
        SubscribeInputResult::Ok
    ));
    assert!(matches!(
        do_subscribe(&plane, b, Tick::ID),
        SubscribeInputResult::Ok
    ));
    let set = subs(&plane, Tick::ID);
    assert_eq!(set.len(), 2);
    assert!(set.contains(&a));
    assert!(set.contains(&b));
}

#[test]
fn unsubscribe_removes_from_set() {
    let plane = make_plane();
    let id = load_blank(&plane, "listener");
    do_subscribe(&plane, id, MouseMove::ID);
    assert!(matches!(
        do_unsubscribe(&plane, id, MouseMove::ID),
        SubscribeInputResult::Ok
    ));
    assert!(subs(&plane, MouseMove::ID).is_empty());
}

#[test]
fn unsubscribe_not_subscribed_is_ok() {
    // ADR-0021 §2: unsubscribe of a non-subscriber is `Ok`, not
    // `Err`. The mailbox must still be a live component though.
    let plane = make_plane();
    let id = load_blank(&plane, "listener");
    assert!(matches!(
        do_unsubscribe(&plane, id, Tick::ID),
        SubscribeInputResult::Ok
    ));
}

#[test]
fn subscribe_unknown_mailbox_is_err() {
    let plane = make_plane();
    assert!(matches!(
        do_subscribe(&plane, MailboxId(9999), Tick::ID),
        SubscribeInputResult::Err { .. }
    ));
}

#[test]
fn subscribe_sink_mailbox_is_err() {
    // Sinks are substrate-owned and don't make sense as input
    // subscribers; the handler rejects.
    let plane = make_plane();
    let sink = plane
        .registry
        .register_sink("some.sink", Arc::new(|_, _, _, _, _, _| {}));
    assert!(matches!(
        do_subscribe(&plane, sink, Tick::ID),
        SubscribeInputResult::Err { .. }
    ));
}

#[test]
fn subscribe_dropped_mailbox_is_err() {
    let plane = make_plane();
    let id = load_blank(&plane, "victim");
    plane.handle_drop(&postcard::to_allocvec(&DropComponent { mailbox_id: id }).unwrap());
    assert!(matches!(
        do_subscribe(&plane, id, Tick::ID),
        SubscribeInputResult::Err { .. }
    ));
}

#[test]
fn drop_component_removes_from_every_subscriber_set() {
    // ADR-0021 §4: dropping a component clears its id from every
    // stream's subscriber set, not just the ones currently held.
    let plane = make_plane();
    let id = load_blank(&plane, "listener");
    do_subscribe(&plane, id, Tick::ID);
    do_subscribe(&plane, id, Key::ID);
    do_subscribe(&plane, id, MouseButton::ID);
    let dropped =
        plane.handle_drop(&postcard::to_allocvec(&DropComponent { mailbox_id: id }).unwrap());
    assert!(matches!(dropped, DropResult::Ok));
    for s in [Tick::ID, Key::ID, MouseMove::ID, MouseButton::ID] {
        assert!(
            !subs(&plane, s).contains(&id),
            "stream {s:?} still contains dropped id"
        );
    }
}

#[test]
fn replace_component_preserves_subscriptions() {
    // ADR-0021 §4: replace keeps the mailbox id, and subscriptions
    // are keyed by mailbox, so the new instance inherits them.
    let plane = make_plane();
    let id = load_blank(&plane, "listener");
    do_subscribe(&plane, id, Tick::ID);
    do_subscribe(&plane, id, Key::ID);
    let result = plane.handle_replace(
        &postcard::to_allocvec(&ReplaceComponent {
            mailbox_id: id,
            wasm: wat::parse_str(WAT).unwrap(),
            drain_timeout_ms: None,
        })
        .unwrap(),
    );
    assert!(matches!(result, ReplaceResult::Ok { .. }));
    assert!(subs(&plane, Tick::ID).contains(&id));
    assert!(subs(&plane, Key::ID).contains(&id));
}

#[test]
fn auto_subscribe_inputs_wires_known_streams_from_capabilities() {
    // Issue #403 regression: a component declaring an input handler
    // in its `aether.kinds.inputs` manifest must end up in the
    // matching stream's subscriber set after `handle_load` /
    // `handle_replace` register it. Pre-fix the SDK fired
    // `subscribe_input` mail during `init`, before the mailbox was
    // in the registry, and `validate_subscriber_mailbox` rejected
    // the unknown id — silently. The substrate now derives the
    // subscriptions from the manifest after registering, and this
    // test pins that wiring.
    let plane = make_plane();
    // Auto-subscribe consults the registry for `is_stream` per kind
    // (ADR-0068), so prime it with descriptors matching what a guest's
    // wasm `aether.kinds` v0x03 section would carry: Tick + WindowSize
    // are streams; the user-space kind is not.
    plane
        .registry
        .register_kind_with_descriptor(KindDescriptor {
            name: Tick::NAME.into(),
            schema: <Tick as aether_mail::Schema>::SCHEMA,
            is_stream: true,
        })
        .unwrap();
    plane
        .registry
        .register_kind_with_descriptor(KindDescriptor {
            name: WindowSize::NAME.into(),
            schema: <WindowSize as aether_mail::Schema>::SCHEMA,
            is_stream: true,
        })
        .unwrap();
    plane
        .registry
        .register_kind_with_descriptor(KindDescriptor {
            name: "user.custom.event".into(),
            schema: SchemaType::Unit,
            is_stream: false,
        })
        .unwrap();
    let mailbox = MailboxId(0xdead_beef);
    let subscribers = input::new_subscribers();
    let capabilities = ComponentCapabilities {
        handlers: vec![
            HandlerCapability {
                id: Tick::ID,
                name: Tick::NAME.into(),
                doc: None,
            },
            HandlerCapability {
                id: WindowSize::ID,
                name: WindowSize::NAME.into(),
                doc: None,
            },
            // User-space kinds fall through — `is_stream = false` on
            // the registry descriptor keeps `auto_subscribe_inputs`
            // from wiring them in. The runtime API
            // (`ctx.subscribe_input::<K>()`) is the explicit path.
            HandlerCapability {
                id: KindId(0xdeadbeef_cafef00d),
                name: "user.custom.event".into(),
                doc: None,
            },
        ],
        fallback: None,
        doc: None,
    };
    auto_subscribe_inputs(&subscribers, &plane.registry, mailbox, &capabilities);
    let snapshot = subscribers.read().unwrap();
    assert!(
        snapshot
            .get(&Tick::ID)
            .is_some_and(|set| set.contains(&mailbox)),
        "Tick handler should auto-subscribe the mailbox",
    );
    assert!(
        snapshot
            .get(&WindowSize::ID)
            .is_some_and(|set| set.contains(&mailbox)),
        "WindowSize handler should auto-subscribe the mailbox",
    );
    for stream in [Key::ID, KeyRelease::ID, MouseMove::ID, MouseButton::ID] {
        assert!(
            snapshot
                .get(&stream)
                .is_none_or(|set| !set.contains(&mailbox)),
            "{stream:?} should not be subscribed when the manifest \
                 doesn't declare its handler",
        );
    }
}

#[test]
fn subscribe_malformed_payload_is_err() {
    let plane = make_plane();
    let result = plane.handle_subscribe(&[0xFF; 4]);
    assert!(matches!(result, SubscribeInputResult::Err { .. }));
}

#[test]
fn subscribe_dispatch_replies_with_result_kind() {
    // Dispatch goes through `dispatch()` so a SubscribeInputResult
    // is sent via reply-to-sender. We can't easily observe the
    // outbound here without a richer fake, but at least confirm
    // the dispatch path doesn't panic on the two kinds.
    let plane = make_plane();
    let id = load_blank(&plane, "listener");
    plane.dispatch(
        SubscribeInput::ID,
        SubscribeInput::NAME,
        crate::mail::ReplyTo::NONE,
        &postcard::to_allocvec(&SubscribeInput {
            kind: Tick::ID,
            mailbox: id,
        })
        .unwrap(),
    );
    plane.dispatch(
        UnsubscribeInput::ID,
        UnsubscribeInput::NAME,
        crate::mail::ReplyTo::NONE,
        &postcard::to_allocvec(&UnsubscribeInput {
            kind: Tick::ID,
            mailbox: id,
        })
        .unwrap(),
    );
    assert!(!subs(&plane, Tick::ID).contains(&id));
}

// ADR-0022's `drain_pending` / `pending` / `frozen` / `parked`
// tests retire with the underlying machinery under ADR-0038 Phase
// 1: freeze-drain-swap is replaced by channel splice, so there is
// no `pending` counter to poll, no `parked` deque to flush, and no
// `frozen` flag to gate. The observable invariant (mail arriving
// during replace reaches the new instance in FIFO order) is
// preserved by the channel itself.

#[cfg(any())]
#[test]
fn drain_pending_returns_true_when_count_drops_in_time() {
    let plane = make_plane();
    let id = load_blank(&plane, "drainable");
    let entry = plane.components.read().unwrap().get(&id).unwrap().clone();
    entry.pending.store(2, Ordering::SeqCst);
    let entry_for_drainer = Arc::clone(&entry);
    let drainer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        entry_for_drainer.pending.store(0, Ordering::SeqCst);
    });
    assert!(super::drain_pending(&entry, Duration::from_millis(500)));
    drainer.join().unwrap();
}

#[cfg(any())]
#[test]
fn replace_drain_timeout_keeps_old_bound() {
    let plane = make_plane();
    let id = load_blank(&plane, "victim");
    let entry_before = plane.components.read().unwrap().get(&id).unwrap().clone();
    // Pin pending above zero so drain never completes within the
    // tight per-replace timeout.
    entry_before.pending.store(1, Ordering::SeqCst);

    let result = plane.handle_replace(
        &postcard::to_allocvec(&ReplaceComponent {
            mailbox_id: id,
            wasm: wat::parse_str(WAT).unwrap(),
            drain_timeout_ms: Some(20),
        })
        .unwrap(),
    );
    let ReplaceResult::Err { error } = result else {
        panic!("expected timeout, got {result:?}");
    };
    assert!(
        error.contains("drain timeout"),
        "unexpected error message: {error}"
    );

    // Same Arc still bound — no swap happened.
    let entry_after = plane.components.read().unwrap().get(&id).unwrap().clone();
    assert!(Arc::ptr_eq(&entry_before, &entry_after));
    // Frozen flag cleared so future mail flows through again.
    assert!(!entry_after.frozen.load(Ordering::SeqCst));

    // Reset pending so the entry drops cleanly when the table
    // releases it (no real worker to decrement on our behalf).
    entry_after.pending.store(0, Ordering::SeqCst);
}

#[cfg(any())]
#[test]
fn replace_flushes_parked_mail_to_new_instance() {
    // Old + new components both forward `receive` to a counter
    // sink. After parking N mails on the entry and triggering a
    // successful replace, the counter records exactly the parked
    // ticks — proving the new instance is the one that handled
    // them post-swap.
    let plane = make_plane();
    let counter = Arc::new(AtomicU32::new(0));
    let counter_for_sink = Arc::clone(&counter);
    let sink_id = plane.registry.register_sink(
        "drain-flush-sink",
        Arc::new(move |_kind_id, _kind, _origin, _sender, _bytes, count| {
            counter_for_sink.fetch_add(count, Ordering::SeqCst);
        }),
    );

    let LoadResult::Ok { mailbox_id: id, .. } = plane.handle_load(
        &postcard::to_allocvec(&LoadComponent {
            wasm: wat::parse_str(WAT_FORWARDS_TO_SINK).unwrap(),
            name: Some("flusher".into()),
        })
        .unwrap(),
    ) else {
        panic!("load failed");
    };

    let entry = plane.components.read().unwrap().get(&id).unwrap().clone();
    // Park three mails directly on the entry. Real workers would
    // do this when frozen=true; here we simulate the post-park
    // state without standing up a worker pool. We do NOT touch
    // queue.outstanding — these mails are off-queue from the
    // pool's perspective.
    entry.frozen.store(true, Ordering::SeqCst);
    let kind_id = 0; // sink_id payload is unused; kind is irrelevant.
    let _ = kind_id;
    for n in 1..=3u32 {
        entry.parked.lock().unwrap().push_back(Mail {
            recipient: id,
            kind: 0,
            payload: sink_id.0.to_le_bytes().to_vec(),
            count: n,
            sender: crate::mail::ReplyTo::NONE,
            from_component: None,
        });
    }

    let result = plane.handle_replace(
        &postcard::to_allocvec(&ReplaceComponent {
            mailbox_id: id,
            wasm: wat::parse_str(WAT_FORWARDS_TO_SINK).unwrap(),
            drain_timeout_ms: Some(500),
        })
        .unwrap(),
    );
    assert!(matches!(result, ReplaceResult::Ok { .. }), "got {result:?}");

    // Three parked ticks (counts 1, 2, 3) flushed to the new
    // instance, which forwarded each to the sink.
    assert_eq!(counter.load(Ordering::SeqCst), 1 + 2 + 3);

    // New entry is bound now, parked is empty, frozen cleared.
    let entry_after = plane.components.read().unwrap().get(&id).unwrap().clone();
    assert!(!Arc::ptr_eq(&entry, &entry_after));
    assert!(entry_after.parked.lock().unwrap().is_empty());
    assert!(!entry_after.frozen.load(Ordering::SeqCst));
}

#[cfg(any())]
#[test]
fn replace_drain_timeout_flushes_parked_to_old() {
    // Pending stays >0 (a forever in-flight deliver), so the
    // replace times out. Parked mail must still be delivered —
    // through the old instance, since the swap didn't happen.
    let plane = make_plane();
    let counter = Arc::new(AtomicU32::new(0));
    let counter_for_sink = Arc::clone(&counter);
    let sink_id = plane.registry.register_sink(
        "drain-timeout-sink",
        Arc::new(move |_kind_id, _kind, _origin, _sender, _bytes, count| {
            counter_for_sink.fetch_add(count, Ordering::SeqCst);
        }),
    );

    let LoadResult::Ok { mailbox_id: id, .. } = plane.handle_load(
        &postcard::to_allocvec(&LoadComponent {
            wasm: wat::parse_str(WAT_FORWARDS_TO_SINK).unwrap(),
            name: Some("survivor".into()),
        })
        .unwrap(),
    ) else {
        panic!("load failed");
    };

    let entry = plane.components.read().unwrap().get(&id).unwrap().clone();
    entry.pending.store(1, Ordering::SeqCst);
    entry.frozen.store(true, Ordering::SeqCst);
    for n in 1..=2u32 {
        entry.parked.lock().unwrap().push_back(Mail {
            recipient: id,
            kind: 0,
            payload: sink_id.0.to_le_bytes().to_vec(),
            count: n,
            sender: crate::mail::ReplyTo::NONE,
            from_component: None,
        });
    }

    let result = plane.handle_replace(
        &postcard::to_allocvec(&ReplaceComponent {
            mailbox_id: id,
            wasm: wat::parse_str(WAT_FORWARDS_TO_SINK).unwrap(),
            drain_timeout_ms: Some(20),
        })
        .unwrap(),
    );
    let ReplaceResult::Err { error } = result else {
        panic!("expected timeout, got {result:?}");
    };
    assert!(error.contains("drain timeout"), "{error}");

    // Old instance handled the parked counts (1 + 2 = 3).
    assert_eq!(counter.load(Ordering::SeqCst), 3);

    // Same entry still bound; parked empty, frozen cleared.
    let entry_after = plane.components.read().unwrap().get(&id).unwrap().clone();
    assert!(Arc::ptr_eq(&entry, &entry_after));
    assert!(entry_after.parked.lock().unwrap().is_empty());
    assert!(!entry_after.frozen.load(Ordering::SeqCst));

    // Reset for clean drop.
    entry_after.pending.store(0, Ordering::SeqCst);
}

/// Component that, on each `receive`, forwards a `send_mail` to
/// the sink mailbox encoded in the payload's first 8 bytes (as
/// a little-endian u64 — ADR-0029 made mailbox ids 64-bit name
/// hashes, so 1-byte truncation no longer works). Used by the
/// drain-flush tests so we can observe whether the new (or old)
/// instance handled each parked mail.
#[allow(dead_code)]
const WAT_FORWARDS_TO_SINK: &str = r#"
        (module
            (import "aether" "send_mail_p32"
                (func $send_mail (param i64 i64 i32 i32 i32) (result i32)))
            (memory (export "memory") 1)
            (func (export "receive_p32")
                (param $kind i64) (param $ptr i32) (param $byte_len i32) (param $count i32) (param $sender i32)
                (result i32)
                (drop (call $send_mail
                    (i64.load (local.get $ptr))
                    (i64.const 0)
                    (i32.const 0)
                    (i32.const 0)
                    (local.get $count)))
                i32.const 0))
    "#;
