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

use aether_actor::__macro_internals::{WasmPlacementFacts, dependency_records_len, write_dependency_records};
use aether_actor::{
    ActorInitError, ActorTypeTag, Addressable, Contract, Contracts, Declared, DependencyLink, DependencyList,
    DependencyResolver, DependsOn, Erased, One, ReplyShape, Silent, Unchecked, Undeclared, WasmActor, WasmCtx,
    WasmInitCtx, actor, handler_set,
};
use aether_data::Kind;
use aether_data::{
    ACTOR_LINEAGE_SECTION_VERSION, ActorId, ActorLineageRecord, INPUTS_SECTION_VERSION, InputsRecord, KindId,
    ReplyContract, actor_lineage_child_len, actor_lineage_root_len, wire, write_actor_lineage_child,
    write_actor_lineage_root,
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

struct RootPeer;

impl Addressable for RootPeer {
    const NAMESPACE: &'static str = "manifest.peer.root";
    type Resolver = One;
}

#[actor(instanced, child_of(FirstParent, SecondParent))]
impl WasmActor for ManifestProbe {
    const NAMESPACE: &'static str = "manifest_probe";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    /// # Agent
    /// Increments the tick counter.
    #[handler::event]
    fn on_tick(&mut self, _ctx: &mut WasmCtx<'_>, _tick: Tick) {}

    // ADR-0109: a `-> R` handler — the return type is the reply
    // contract, so the macro auto-replies `Pong` and threads its kind id
    // onto this handler's inputs-manifest record.
    #[handler::request]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, ping: Ping) -> Pong {
        Pong { seq: ping.seq }
    }

    // ADR-0112: an unchecked-class handler — it receives the `Unchecked` ctx and
    // issues its own replies, so the manifest reports `ReplyContract::Unchecked`
    // (no single static reply kind).
    #[handler::unchecked(reason = "test: the manifest carries an unchecked row's reason")]
    fn on_poke(&mut self, _ctx: &mut WasmCtx<'_, Erased, Unchecked>, _poke: Poke) {}

    /// # Agent
    /// Catch-all for anything else.
    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: aether_actor::Mail<'_>) {}

    fn unwire(&mut self, _ctx: &mut WasmCtx<'_>) {}
}

/// ADR-0231: the contract-row probe. A test build sets `cfg(test)`, so
/// `on_present` and its kind survive while `on_stripped` and its kind are gone;
/// the adopted set adds one silent and one unchecked handler.
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
#[aether_data::kind(name = "test.contract.set_unchecked", pod)]
struct SetUnchecked {
    seq: u32,
}

#[handler_set]
trait ContractSet {
    #[handler::tell]
    fn on_set_silent(&mut self, _ctx: &mut WasmCtx<'_>, _mail: SetSilent) {}

    #[handler::unchecked(reason = "test: a handler set carries an unchecked row's reason")]
    fn on_set_unchecked(&mut self, _ctx: &mut WasmCtx<'_, Erased, Unchecked>, _mail: SetUnchecked) {}
}

struct ContractProbe;

impl ContractSet for ContractProbe {}

#[actor(handler_set(ContractSet))]
impl WasmActor for ContractProbe {
    const NAMESPACE: &'static str = "manifest.contract";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::request]
    #[cfg(test)]
    fn on_present(&mut self, _ctx: &mut WasmCtx<'_>, present: Present) -> Pong {
        Pong { seq: present.seq }
    }

    #[handler::tell]
    #[cfg(not(test))]
    fn on_stripped(&mut self, _ctx: &mut WasmCtx<'_>, _stripped: Stripped) {}
}

struct DependentProbe;

#[actor(depends(FirstParent, RootPeer))]
impl WasmActor for DependentProbe {
    const NAMESPACE: &'static str = "manifest.dependent";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::event]
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
    let mut instanced_count = 0usize;
    let mut tick_doc: Option<String> = None;

    for rec in &records {
        match rec {
            InputsRecord::Handler { id, name, doc, reply, reason } => {
                handler_count += 1;
                match name.as_ref() {
                    "test.tick" => {
                        assert_eq!(*id, <Tick as Kind>::ID);
                        tick_doc = doc.as_ref().map(ToString::to_string);
                        // ADR-0112: a single `-> ()` handler is `None`.
                        assert_eq!(*reply, ReplyContract::None, "on_tick returns () — no reply kind");
                        assert_eq!(*reason, None, "a single handler carries no reason");
                    }
                    "test.ping" => {
                        assert_eq!(*id, <Ping as Kind>::ID);
                        // ADR-0112: a single `-> Pong` handler is `One(Pong)`.
                        assert_eq!(
                            *reply,
                            ReplyContract::One(<Pong as Kind>::ID),
                            "on_ping returns Pong — its reply kind rides the manifest"
                        );
                        assert_eq!(*reason, None, "a single handler carries no reason");
                    }
                    "test.poke" => {
                        assert_eq!(*id, <Poke as Kind>::ID);
                        // ADR-0112: a `#[handler::unchecked(..)]` handler is `Unchecked`.
                        assert_eq!(
                            *reply,
                            ReplyContract::Unchecked,
                            "on_poke is unchecked-class — the manifest reports Unchecked"
                        );
                        // #7193: the stated reason rides beside the reply.
                        assert_eq!(
                            reason.as_deref(),
                            Some("test: the manifest carries an unchecked row's reason"),
                            "on_poke's reason rides its handler record"
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
            // ADR-0231 §10: `#[actor]` writes no Dependency record into the
            // inherent manifest; `export!` writes them from the
            // `Declared::Depends` list.
            InputsRecord::Dependency { .. } => {
                panic!("unexpected Dependency record in an inherent inputs manifest")
            }
            // ADR-0241 §5: the fixture declares `#[actor(instanced)]`.
            InputsRecord::Instanced => instanced_count += 1,
        }
    }

    assert_eq!(handler_count, 3, "expected three #[handler] records");
    assert_eq!(fallback_count, 1, "expected one #[fallback] record");
    assert_eq!(instanced_count, 1, "expected one Instanced record");
    assert_eq!(
        tick_doc.as_deref(),
        Some("Increments the tick counter."),
        "rustdoc # Agent body should land on the Tick handler"
    );
}

/// ADR-0241 §5: the host reads cardinality only from the manifest, so the
/// derive must write exactly one `Instanced` record for an instanced type and
/// none for a singleton. A dropped record names an instanced actor by its
/// bare namespace; a stray one makes a singleton demand a key.
#[test]
fn the_inputs_manifest_records_cardinality() {
    fn instanced_records(bytes: &[u8]) -> usize {
        parse_section(bytes).into_iter().filter(|record| *record == InputsRecord::Instanced).count()
    }

    assert_eq!(instanced_records(&ManifestProbe::__AETHER_INPUTS_MANIFEST), 1, "instanced child");
    assert_eq!(instanced_records(&ContractProbe::__AETHER_INPUTS_MANIFEST), 0, "singleton adopting a handler set");
    assert_eq!(instanced_records(&DependentProbe::__AETHER_INPUTS_MANIFEST), 0, "singleton with dependencies");
}

#[test]
fn depends_entries_emit_dependency_records() {
    // The call `export!` makes: the records come off the `Declared::Depends`
    // list, not the inherent manifest.
    const FIRST: Option<&'static DependencyLink> = <<DependentProbe as Declared>::Depends as DependencyList>::FIRST;
    const LEN: usize = dependency_records_len(FIRST);
    const BYTES: [u8; LEN] = {
        let mut out = [0u8; LEN];
        let end = write_dependency_records(FIRST, &mut out, 0);
        assert!(end == LEN, "the writer fills exactly the length the walk measured");
        out
    };

    fn assert_depends_on<T: DependsOn<FirstParent> + DependsOn<RootPeer>>() {}

    assert_depends_on::<DependentProbe>();

    assert_eq!(
        parse_section(&BYTES),
        vec![
            InputsRecord::Dependency { resolver: One::TAG, namespace: FirstParent::NAMESPACE.into() },
            InputsRecord::Dependency { resolver: One::TAG, namespace: RootPeer::NAMESPACE.into() },
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
        WasmPlacementFacts { is_instanced: true, exact_parent_tags: EXACT_PARENT_TAGS },
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
fn lineage_const_encoders_match_runtime_wire_for_every_selector() {
    const ACTOR: u64 = ActorId::singleton(ManifestProbe::NAMESPACE).0;
    const FIRST: u64 = ActorId::singleton(FirstParent::NAMESPACE).0;
    const ROOT_LEN: usize = actor_lineage_root_len(ACTOR, ManifestProbe::NAMESPACE);
    const ROOT_BYTES: [u8; ROOT_LEN] = write_actor_lineage_root(ACTOR, ManifestProbe::NAMESPACE);
    const CHILD_LEN: usize = actor_lineage_child_len(FIRST, ACTOR, FirstParent::NAMESPACE, ManifestProbe::NAMESPACE);
    const CHILD_BYTES: [u8; CHILD_LEN] =
        write_actor_lineage_child(FIRST, ACTOR, FirstParent::NAMESPACE, ManifestProbe::NAMESPACE);

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
    ];

    for (const_bytes, record) in records {
        assert_eq!(
            const_bytes,
            wire::to_vec(&record).expect("runtime actor-lineage encoding"),
            "const and runtime actor-lineage encoders must agree for {record:?}"
        );
    }
}
