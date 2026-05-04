//! Issue 442 regression: `#[handlers]` emits the
//! `aether.kinds.inputs` payload as associated consts on the
//! component type's inherent impl, NOT as `#[link_section]` statics.
//! `aether_component::export!()` is the only place that pins those
//! bytes into the wasm custom section, so the section can only land
//! in the cdylib root that calls `export!()` — never in transitive
//! rlib pulls of a `#[handlers]`-using crate.
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

use aether_component::{Component, Ctx, DropCtx, InitCtx, handlers};
use aether_data::Kind;
use aether_data::{INPUTS_SECTION_VERSION, InputsRecord};
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

// Minimal fixture, mirrored from `examples/hello.rs`. Lives here as a
// duplicate (rather than reused) because `examples/*.rs` declare
// `crate-type = ["cdylib"]` and only build for `wasm32-unknown-unknown`
// — the test exercises the const path host-side. Maintenance is the
// usual SDK-surface cadence: when `Component` / `Ctx` / `Mail` /
// `#[handlers]` change shape, this fixture moves with every other
// component in the workspace.
struct ManifestProbe;

#[handlers]
impl Component for ManifestProbe {
    const NAMESPACE: &'static str = "manifest_probe";

    fn init(_ctx: &mut InitCtx<'_>) -> Self {
        Self
    }

    /// # Agent
    /// Increments the tick counter.
    #[handler]
    fn on_tick(&mut self, _ctx: &mut Ctx<'_>, _tick: Tick) {}

    #[handler]
    fn on_ping(&mut self, _ctx: &mut Ctx<'_>, _ping: Ping) {}

    /// # Agent
    /// Catch-all for anything else.
    #[fallback]
    fn on_other(&mut self, _ctx: &mut Ctx<'_>, _mail: aether_component::Mail<'_>) {}

    fn on_drop(&mut self, _ctx: &mut DropCtx<'_>) {}
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
        let (rec, rest) = postcard::take_from_bytes::<InputsRecord>(cursor)
            .expect("postcard decode of InputsRecord failed");
        out.push(rec);
        cursor = rest;
    }
    out
}

#[test]
fn manifest_const_round_trips_to_expected_records() {
    const LEN: usize = ManifestProbe::__AETHER_INPUTS_MANIFEST_LEN;
    const { assert!(LEN > 0, "ManifestProbe declares two handlers + fallback") };
    let bytes: &[u8] = &ManifestProbe::__AETHER_INPUTS_MANIFEST;
    assert_eq!(bytes.len(), LEN);

    let records = parse_section(bytes);

    let mut handler_count = 0usize;
    let mut fallback_count = 0usize;
    let mut tick_doc: Option<String> = None;

    for rec in &records {
        match rec {
            InputsRecord::Handler { id, name, doc } => {
                handler_count += 1;
                match name.as_ref() {
                    "test.tick" => {
                        assert_eq!(*id, <Tick as Kind>::ID);
                        tick_doc = doc.as_ref().map(|c| c.to_string());
                    }
                    "test.ping" => {
                        assert_eq!(*id, <Ping as Kind>::ID);
                    }
                    other => panic!("unexpected handler name: {other}"),
                }
            }
            InputsRecord::Fallback { .. } => fallback_count += 1,
            InputsRecord::Component { .. } => {}
        }
    }

    assert_eq!(handler_count, 2, "expected two #[handler] records");
    assert_eq!(fallback_count, 1, "expected one #[fallback] record");
    assert_eq!(
        tick_doc.as_deref(),
        Some("Increments the tick counter."),
        "rustdoc # Agent body should land on the Tick handler"
    );
}
