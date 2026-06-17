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
//! `target_arch = "wasm32"`. That gate fired for both the cdylib
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

use aether_actor::{BootError, FfiActor, FfiCtx, Manual, Resolver, actor};
use aether_data::Kind;
use aether_data::{INPUTS_SECTION_VERSION, InputsRecord, ReplyContract, wire};
use bytemuck::{Pod, Zeroable};

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.tick")]
struct Tick;

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.ping")]
struct Ping {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.pong")]
struct Pong {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.poke")]
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

#[actor]
impl FfiActor for ManifestProbe {
    const NAMESPACE: &'static str = "manifest_probe";

    fn init<C>(_ctx: &mut C) -> Result<Self, BootError>
    where
        C: Resolver,
    {
        Ok(Self)
    }

    /// # Agent
    /// Increments the tick counter.
    #[handler]
    fn on_tick(&mut self, _ctx: &mut FfiCtx<'_>, _tick: Tick) {}

    // ADR-0109: a `-> R` handler — the return type is the reply
    // contract, so the macro auto-replies `Pong` and threads its kind id
    // onto this handler's inputs-manifest record.
    #[handler]
    fn on_ping(&mut self, _ctx: &mut FfiCtx<'_>, ping: Ping) -> Pong {
        Pong { seq: ping.seq }
    }

    // ADR-0112: a manual-class handler — it receives the `Manual` ctx and
    // issues its own replies, so the manifest reports `ReplyContract::Manual`
    // (no single static reply kind).
    #[handler::manual]
    fn on_poke(&mut self, _ctx: &mut FfiCtx<'_, Manual>, _poke: Poke) {}

    /// # Agent
    /// Catch-all for anything else.
    #[fallback]
    fn on_other(&mut self, _ctx: &mut FfiCtx<'_>, _mail: aether_actor::Mail<'_>) {}

    fn unwire(&mut self, _ctx: &mut FfiCtx<'_>) {}
}

fn parse_section(bytes: &[u8]) -> Vec<InputsRecord> {
    let mut out: Vec<InputsRecord> = Vec::new();
    let mut cursor = bytes;
    while !cursor.is_empty() {
        assert_eq!(
            cursor[0], INPUTS_SECTION_VERSION,
            "every record must start with the section version byte"
        );
        cursor = &cursor[1..];
        let (rec, rest) = wire::take_from_bytes_bare::<InputsRecord>(cursor)
            .expect("wire decode of InputsRecord failed");
        out.push(rec);
        cursor = rest;
    }
    out
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
            InputsRecord::Handler {
                id,
                name,
                doc,
                reply,
            } => {
                handler_count += 1;
                match name.as_ref() {
                    "test.tick" => {
                        assert_eq!(*id, <Tick as Kind>::ID);
                        tick_doc = doc.as_ref().map(ToString::to_string);
                        // ADR-0112: a single `-> ()` handler is `None`.
                        assert_eq!(
                            *reply,
                            ReplyContract::None,
                            "on_tick returns () — no reply kind"
                        );
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
