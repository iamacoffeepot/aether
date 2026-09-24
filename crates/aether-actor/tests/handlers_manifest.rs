//! Issue 442 regression: `#[actor]` emits the
//! `aether.kinds.inputs` payload as associated consts on the
//! component type's inherent impl, NOT as `#[link_section]` statics.
//! `aether_actor::export!()` is the only place that pins those
//! bytes into the wasm custom section, so the section can only land
//! in the cdylib root that calls `export!()` — never in transitive
//! rlib pulls of a `#[actor]`-using crate.
//!
//! Pre-issue-442 the macro emitted N separate `#[link_section]`
//! statics, one per handler/fallback/component-doc record, gated on
//! `target_family = "wasm"`. That gate fired for both the cdylib
//! root and any transitive wasm32 rlib pull, so a cdylib that
//! depended on a sibling `cdylib + rlib` crate's rlib output would
//! see both crates' Component records stack in its
//! `aether.kinds.inputs` section and fail the substrate's "duplicate
//! Component record" check.

#![allow(dead_code)]
// Manifest-probe fixture's `#[handler]` / `#[fallback]` bodies are
// stubs that exercise the const-emission path — they have to keep
// `&mut self` to match the dispatch ABI but don't read state.
#![allow(clippy::unused_self)]

use aether_actor::__macro_internals::WasmPlacementFacts;
use aether_actor::{
    ActorInitError, ActorTypeTag, Addressable, Contract, Contracts, DependencyResolver, DependsOn, Embedded, Erased,
    Manual, One, ReplyShape, Silent, Undeclared, WasmActor, WasmCtx, WasmInitCtx, actor, handler_set,
};
use aether_data::Kind;
use aether_data::{
    ACTOR_LINEAGE_SECTION_VERSION, ActorId, ActorLineageRecord, INPUTS_SECTION_VERSION, InputsRecord, KindId,
    ReplyContract, actor_lineage_child_len, actor_lineage_module_child_len, actor_lineage_root_len, wire,
    write_actor_lineage_child, write_actor_lineage_module_child, write_actor_lineage_root,
};

#[repr(C)]
#[aether_data::kind(name = "test.tick", pod)]
struct Tick;

#[repr(C)]
#[aether_data::kind(name = "test.ping", pod)]
struct Ping {
    seq: u32,
}

#[repr(C)]
#[aether_data::kind(name = "test.pong", pod)]
struct Pong {
    seq: u32,
}

#[repr(C)]
#[aether_data::kind(name = "test.poke", pod)]
struct Poke {
    seq: u32,
}

// Minimal fixture, mirrored from `examples/hello.rs`. Lives here as a
// duplicate (rather than reused) because `examples/*.rs` declare
// `crate-type = ["cdylib"]` and only build for `wasm32-unknown-unknown`
// — the test exercises the const path host-side. Maintenance is the
// usual SDK-surface cadence: when `Component` / `Ctx` / `Mail` /
// `#[actor]` change shape, this fixture moves with every other
// component in the workspace.
struct ManifestProbe;

struct FirstParent;

impl Addressable for FirstParent {
    const NAMESPACE: &'static str = "manifest.parent.first";
    type Resolver = One;
}

struct SecondParent;

impl Addressable for SecondParent {
    const NAMESPACE: &'static str = "manifest.parent.second";
    type Resolver = One;
}

struct EmbeddedPeer;

impl Addressable for EmbeddedPeer {
    const NAMESPACE: &'static str = "manifest.peer.embedded";
    type Resolver = Embedded;
}

#[actor(instanced, child_of(FirstParent), child_of(SecondParent))]
impl WasmActor for ManifestProbe {
    const NAMESPACE: &'static str = "manifest_probe";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    /// # Agent
    /// Increments the tick counter.
    #[handler::single]
    fn on_tick(&mut self, _ctx: &mut WasmCtx<'_>, _tick: Tick) {}

    // ADR-0109: a `-> R` handler — the return type is the reply
    // contract, so the macro auto-replies `Pong` and threads its kind id
    // onto this handler's inputs-manifest record.
    #[handler::single]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, ping: Ping) -> Pong {
        Pong { seq: ping.seq }
    }

    // ADR-0112: a manual-class handler — it receives the `Manual` ctx and
    // issues its own replies, so the manifest reports `ReplyContract::Manual`
    // (no single static reply kind).
    #[handler::manual]
    fn on_poke(&mut self, _ctx: &mut WasmCtx<'_, Erased, Manual>, _poke: Poke) {}

    /// # Agent
    /// Catch-all for anything else.
    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: aether_actor::Mail<'_>) {}

    fn unwire(&mut self, _ctx: &mut WasmCtx<'_>) {}
}

struct ComposableProbe;

#[actor(instanced, composable)]
impl WasmActor for ComposableProbe {
    const NAMESPACE: &'static str = "manifest.composable";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn fallback(&mut self, _ctx: &mut WasmCtx<'_>, _mail: aether_actor::Mail<'_>) {}
}

/// ADR-0231: the contract-row probe. A test build sets `cfg(test)`, so
/// `on_present` and its kind survive while `on_stripped` and its kind are gone;
/// the adopted set adds one silent and one manual handler.
#[repr(C)]
#[aether_data::kind(name = "test.contract.present", pod)]
struct Present {
    seq: u32,
}

#[cfg(not(test))]
#[repr(C)]
#[aether_data::kind(name = "test.contract.stripped", pod)]
struct Stripped {
    seq: u32,
}

#[repr(C)]
#[aether_data::kind(name = "test.contract.set_silent", pod)]
struct SetSilent {
    seq: u32,
}

#[repr(C)]
#[aether_data::kind(name = "test.contract.set_manual", pod)]
struct SetManual {
    seq: u32,
}

#[handler_set]
trait ContractSet {
    #[handler::single]
    fn on_set_silent(&mut self, _ctx: &mut WasmCtx<'_>, _mail: SetSilent) {}

    #[handler::manual]
    fn on_set_manual(&mut self, _ctx: &mut WasmCtx<'_, Erased, Manual>, _mail: SetManual) {}
}

struct ContractProbe;

impl ContractSet for ContractProbe {}

#[actor(handler_set(ContractSet))]
impl WasmActor for ContractProbe {
    const NAMESPACE: &'static str = "manifest.contract";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::single]
    #[cfg(test)]
    fn on_present(&mut self, _ctx: &mut WasmCtx<'_>, present: Present) -> Pong {
        Pong { seq: present.seq }
    }

    #[handler::single]
    #[cfg(not(test))]
    fn on_stripped(&mut self, _ctx: &mut WasmCtx<'_>, _stripped: Stripped) {}
}

struct DependentProbe;

#[actor(depends(FirstParent, EmbeddedPeer))]
impl WasmActor for DependentProbe {
    const NAMESPACE: &'static str = "manifest.dependent";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_tick(&mut self, _ctx: &mut WasmCtx<'_>, _tick: Tick) {}
}

#[derive(Debug, PartialEq, Eq)]
enum LineageSectionError {
    UnsupportedVersion(u8),
    MalformedRecord,
}

/// Decode the generated lineage manifest so the tests below can assert its
/// *contents* — which namespaces, which edge kinds. This is a test-local
/// decoder for macro output, not a mirror of a host reader: enforcement is
/// guest-side and no host parses this section (ADR-0166 §3).
fn parse_lineage_section(bytes: &[u8]) -> Result<Vec<ActorLineageRecord>, LineageSectionError> {
    let mut out = Vec::new();
    let mut cursor = bytes;
    while !cursor.is_empty() {
        if cursor[0] != ACTOR_LINEAGE_SECTION_VERSION {
            return Err(LineageSectionError::UnsupportedVersion(cursor[0]));
        }
        let (record, rest) = wire::take_from_bytes::<ActorLineageRecord>(&cursor[1..])
            .map_err(|_| LineageSectionError::MalformedRecord)?;
        out.push(record);
        cursor = rest;
    }
    Ok(out)
}

fn parse_section(bytes: &[u8]) -> Vec<InputsRecord> {
    let mut out: Vec<InputsRecord> = Vec::new();
    let mut cursor = bytes;
    while !cursor.is_empty() {
        assert_eq!(cursor[0], INPUTS_SECTION_VERSION, "every record must start with the section version byte");
        cursor = &cursor[1..];
        let (rec, rest) = wire::take_from_bytes::<InputsRecord>(cursor).expect("wire decode of InputsRecord failed");
        out.push(rec);
        cursor = rest;
    }
    out
}

fn assert_replies<T: aether_actor::Replies<Ping, Reply = Pong>>() {}

fn assert_row<T: Contract<K, Reply = R>, K: Kind, R: ReplyShape>() {}

/// Sort `(kind, reply)` rows by kind so two emissions compare as sets. An
/// actor handles each kind once, so the kind alone orders them.
fn sorted(mut rows: Vec<(KindId, ReplyContract)>) -> Vec<(KindId, ReplyContract)> {
    rows.sort_by_key(|(id, _)| *id);
    rows
}

/// The `(kind, reply)` pairs of the handler records in an inputs manifest.
fn manifest_rows(bytes: &[u8]) -> Vec<(KindId, ReplyContract)> {
    parse_section(bytes)
        .into_iter()
        .filter_map(|record| match record {
            InputsRecord::Handler { id, reply, .. } => Some((id, reply)),
            _ => None,
        })
        .collect()
}

#[test]
fn every_handler_emits_its_contract_row() {
    assert_row::<ManifestProbe, Tick, Silent>();
    assert_row::<ManifestProbe, Ping, Pong>();
    assert_row::<ManifestProbe, Poke, Undeclared>();
    assert_row::<ContractProbe, Present, Pong>();
}

/// ADR-0231 §4: `CONTRACTS` and the inputs manifest are produced by separate
/// code, so this pins them to the same rows: a reply mapped differently on
/// either side, or an adopted set's rows missing from either. A
/// `#[cfg]`-stripped handler's row names a kind absent from this build, so
/// keeping it fails compilation instead.
#[test]
fn contracts_match_inputs_manifest() {
    assert_eq!(
        sorted(ManifestProbe::CONTRACTS.to_vec()),
        sorted(manifest_rows(&ManifestProbe::__AETHER_INPUTS_MANIFEST)),
        "ManifestProbe's CONTRACTS must match its inputs manifest",
    );
    assert_eq!(
        sorted(ContractProbe::CONTRACTS.to_vec()),
        sorted(manifest_rows(&ContractProbe::__AETHER_INPUTS_MANIFEST)),
        "ContractProbe's CONTRACTS must match its inputs manifest, set rows included",
    );
    assert_eq!(ContractProbe::CONTRACTS.len(), 3, "one surviving local row plus the set's two");
}

#[test]
fn handler_return_type_emits_replies_marker() {
    assert_replies::<ManifestProbe>();
}

#[test]
fn manifest_const_round_trips_to_expected_records() {
    const LEN: usize = ManifestProbe::__AETHER_INPUTS_MANIFEST_LEN;
    const { assert!(LEN > 0, "ManifestProbe declares three handlers + fallback") };
    let bytes: &[u8] = &ManifestProbe::__AETHER_INPUTS_MANIFEST;
    assert_eq!(bytes.len(), LEN);

    let records = parse_section(bytes);

    let mut handler_count = 0usize;
    let mut fallback_count = 0usize;
    let mut tick_doc: Option<String> = None;

    for rec in &records {
        match rec {
            InputsRecord::Handler { id, name, doc, reply } => {
                handler_count += 1;
                match name.as_ref() {
                    "test.tick" => {
                        assert_eq!(*id, <Tick as Kind>::ID);
                        tick_doc = doc.as_ref().map(ToString::to_string);
                        // ADR-0112: a single `-> ()` handler is `None`.
                        assert_eq!(*reply, ReplyContract::None, "on_tick returns () — no reply kind");
                    }
                    "test.ping" => {
                        assert_eq!(*id, <Ping as Kind>::ID);
                        // ADR-0112: a single `-> Pong` handler is `One(Pong)`.
                        assert_eq!(
                            *reply,
                            ReplyContract::One(<Pong as Kind>::ID),
                            "on_ping returns Pong — its reply kind rides the manifest"
                        );
                    }
                    "test.poke" => {
                        assert_eq!(*id, <Poke as Kind>::ID);
                        // ADR-0112: a `#[handler::manual]` handler is `Manual`.
                        assert_eq!(
                            *reply,
                            ReplyContract::Manual,
                            "on_poke is manual-class — the manifest reports Manual"
                        );
                    }
                    other => panic!("unexpected handler name: {other}"),
                }
            }
            InputsRecord::Fallback { .. } => fallback_count += 1,
            InputsRecord::Component { .. } => {}
            // ADR-0090 (issue 1257): this fixture declares no `type
            // Config`, so the macro emits no Config record.
            InputsRecord::Config { .. } => {
                panic!("unexpected Config record for a no-config component")
            }
            // ADR-0096: single-actor `export!` emits no boundary record.
            InputsRecord::ActorBoundary { .. } => {
                panic!("unexpected ActorBoundary record for a single-actor module")
            }
            // ADR-0230: this fixture declares no `depends(...)`, so the
            // macro emits no Dependency record.
            InputsRecord::Dependency { .. } => {
                panic!("unexpected Dependency record for a dependency-free component")
            }
        }
    }

    assert_eq!(handler_count, 3, "expected three #[handler] records");
    assert_eq!(fallback_count, 1, "expected one #[fallback] record");
    assert_eq!(
        tick_doc.as_deref(),
        Some("Increments the tick counter."),
        "rustdoc # Agent body should land on the Tick handler"
    );
}

#[test]
fn depends_entries_emit_dependency_records() {
    fn assert_depends_on<T: DependsOn<FirstParent> + DependsOn<EmbeddedPeer>>() {}
    assert_depends_on::<DependentProbe>();

    let records = parse_section(&DependentProbe::__AETHER_INPUTS_MANIFEST);
    let dependencies: Vec<InputsRecord> =
        records.into_iter().filter(|record| matches!(record, InputsRecord::Dependency { .. })).collect();
    assert_eq!(
        dependencies,
        vec![
            InputsRecord::Dependency { resolver: One::TAG, namespace: FirstParent::NAMESPACE.into() },
            InputsRecord::Dependency { resolver: Embedded::TAG, namespace: EmbeddedPeer::NAMESPACE.into() },
        ],
        "one Dependency record per depends(...) entry, in declaration order",
    );
}

#[test]
fn lineage_manifest_const_round_trips_to_actor_owned_names_and_tags() {
    const EXACT_PARENT_TAGS: &[ActorTypeTag] = &[ActorTypeTag::of::<FirstParent>(), ActorTypeTag::of::<SecondParent>()];

    let records = parse_lineage_section(&ManifestProbe::__AETHER_LINEAGE_MANIFEST)
        .expect("generated exact lineage manifest must decode");
    let actor = ActorId::singleton(ManifestProbe::NAMESPACE).0;
    let first = ActorId::singleton(FirstParent::NAMESPACE).0;
    let second = ActorId::singleton(SecondParent::NAMESPACE).0;

    assert_eq!(
        records,
        vec![
            ActorLineageRecord::Child {
                parent: first,
                child: actor,
                parent_namespace: FirstParent::NAMESPACE.into(),
                child_namespace: ManifestProbe::NAMESPACE.into(),
            },
            ActorLineageRecord::Child {
                parent: second,
                child: actor,
                parent_namespace: SecondParent::NAMESPACE.into(),
                child_namespace: ManifestProbe::NAMESPACE.into(),
            },
        ]
    );

    assert_eq!(
        ManifestProbe::__AETHER_PLACEMENT,
        WasmPlacementFacts { is_instanced: true, module_child: false, exact_parent_tags: EXACT_PARENT_TAGS },
        "runtime placement facts must derive from the same exact parents as the wire records"
    );

    let expected_child = ActorLineageRecord::Child {
        parent: first,
        child: actor,
        parent_namespace: FirstParent::NAMESPACE.into(),
        child_namespace: ManifestProbe::NAMESPACE.into(),
    };
    let runtime = wire::to_vec(&expected_child).expect("runtime lineage encoding");
    assert_eq!(
        &ManifestProbe::__AETHER_LINEAGE_MANIFEST[1..=runtime.len()],
        runtime,
        "const encoder must match the runtime aether-wire vocabulary"
    );
}

#[test]
fn composable_lineage_manifest_records_module_child_and_placement_facts() {
    assert_eq!(ACTOR_LINEAGE_SECTION_VERSION, 0x02, "module-child metadata requires v0x02 framing");

    let bytes = &ComposableProbe::__AETHER_LINEAGE_MANIFEST;
    assert_eq!(&bytes[1..5], &2u32.to_le_bytes(), "ModuleChild must retain wire selector 2");

    let child = ActorId::singleton(ComposableProbe::NAMESPACE).0;
    assert_eq!(
        parse_lineage_section(bytes).expect("generated module-child lineage manifest must decode"),
        vec![ActorLineageRecord::ModuleChild { child, child_namespace: ComposableProbe::NAMESPACE.into() }]
    );
    assert_eq!(
        ComposableProbe::__AETHER_PLACEMENT,
        WasmPlacementFacts { is_instanced: true, module_child: true, exact_parent_tags: &[] },
        "the hidden runtime facts and module-child record must derive from one actor declaration"
    );

    let runtime =
        wire::to_vec(&ActorLineageRecord::ModuleChild { child, child_namespace: ComposableProbe::NAMESPACE.into() })
            .expect("runtime module-child encoding");
    assert_eq!(&bytes[1..], runtime, "generated module-child bytes must match runtime aether-wire encoding");
}

#[test]
fn lineage_const_encoders_match_runtime_wire_for_every_selector() {
    const ACTOR: u64 = ActorId::singleton(ManifestProbe::NAMESPACE).0;
    const FIRST: u64 = ActorId::singleton(FirstParent::NAMESPACE).0;
    const ROOT_LEN: usize = actor_lineage_root_len(ACTOR, ManifestProbe::NAMESPACE);
    const ROOT_BYTES: [u8; ROOT_LEN] = write_actor_lineage_root(ACTOR, ManifestProbe::NAMESPACE);
    const CHILD_LEN: usize = actor_lineage_child_len(FIRST, ACTOR, FirstParent::NAMESPACE, ManifestProbe::NAMESPACE);
    const CHILD_BYTES: [u8; CHILD_LEN] =
        write_actor_lineage_child(FIRST, ACTOR, FirstParent::NAMESPACE, ManifestProbe::NAMESPACE);
    const MODULE_CHILD_LEN: usize = actor_lineage_module_child_len(ACTOR, ManifestProbe::NAMESPACE);
    const MODULE_CHILD_BYTES: [u8; MODULE_CHILD_LEN] =
        write_actor_lineage_module_child(ACTOR, ManifestProbe::NAMESPACE);

    let records = [
        (ROOT_BYTES.as_slice(), ActorLineageRecord::Root { actor: ACTOR, namespace: ManifestProbe::NAMESPACE.into() }),
        (
            CHILD_BYTES.as_slice(),
            ActorLineageRecord::Child {
                parent: FIRST,
                child: ACTOR,
                parent_namespace: FirstParent::NAMESPACE.into(),
                child_namespace: ManifestProbe::NAMESPACE.into(),
            },
        ),
        (
            MODULE_CHILD_BYTES.as_slice(),
            ActorLineageRecord::ModuleChild { child: ACTOR, child_namespace: ManifestProbe::NAMESPACE.into() },
        ),
    ];

    for (const_bytes, record) in records {
        assert_eq!(
            const_bytes,
            wire::to_vec(&record).expect("runtime actor-lineage encoding"),
            "const and runtime actor-lineage encoders must agree for {record:?}"
        );
    }
}
