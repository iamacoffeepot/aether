//! ADR-0113 / issue 1855: host-side proof of the framing the
//! `#[actor]`-generated `on_dehydrate` / `on_rehydrate` hooks ride.
//!
//! The generated `on_dehydrate` deposits the actor's `type State` via
//! `Persistence::save_state_kind`; the generated `on_rehydrate` recovers
//! it via `PriorState::as_kind` and boots fresh (warning) when the decode
//! misses. Those hooks only run on the wasm load/replace path, which the
//! test bench drives end-to-end — but the test bench cannot observe the
//! decode-miss warn (it does not route `aether.log` mail through its
//! observed sinks). This test stands in for that seam on the host: it
//! exercises the deposit (`save_state_kind`) and recovery (`as_kind`)
//! halves through a capture ctx, and asserts the exact decode-miss
//! predicate the generated warn branches on (`as_kind() == None` while
//! `bytes()` is non-empty).

use aether_actor::{Persistence, PriorState};
use aether_data::Kind;
use serde::{Deserialize, Serialize};

/// Captures whatever the dehydrate side deposits, standing in for the
/// substrate-owned migration buffer the real `WasmDropCtx` writes to.
#[derive(Default)]
struct CaptureCtx {
    saved: Option<(u32, Vec<u8>)>,
}

impl Persistence for CaptureCtx {
    fn save_state(&mut self, version: u32, bytes: &[u8]) {
        self.saved = Some((version, bytes.to_vec()));
    }
}

/// The state kind a typed actor declares as `type State`.
#[derive(
    aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq,
)]
#[kind(name = "test.state.counter")]
struct CounterState {
    count: u32,
}

/// A reshaped version of the same logical state — an added field changes
/// the schema and therefore `Kind::ID`, which is exactly how a
/// `replace_component` against an evolved state kind manifests.
#[derive(
    aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq,
)]
#[kind(name = "test.state.counter.reshaped")]
struct CounterStateReshaped {
    count: u32,
    generation: u32,
}

/// Build a `PriorState` over a captured save buffer. The returned value
/// borrows `buf` for its lifetime.
fn prior_from(buf: &[u8]) -> PriorState<'_> {
    // SAFETY: `PriorState<'_>` borrows `buf` via the explicit lifetime;
    // the `(addr, len)` pair derives from a live slice valid for the
    // borrow. Mirrors the substrate's `on_rehydrate` ABI on the host.
    unsafe { PriorState::__from_ptr(0, buf.as_ptr().addr(), buf.len()) }
}

/// The deposit half (`save_state_kind`) and the recovery half
/// (`PriorState::as_kind`) round-trip the state value — the two halves
/// the generated hooks each own, exercised back to back.
#[test]
fn save_state_kind_round_trips_through_as_kind() {
    let value = CounterState { count: 7 };

    let mut ctx = CaptureCtx::default();
    ctx.save_state_kind::<CounterState>(0, &value);

    let (version, buf) = ctx.saved.expect("dehydrate deposits a bundle");
    assert_eq!(version, 0, "the generated on_dehydrate frames version 0");

    let prior = prior_from(&buf);
    let recovered = prior
        .as_kind::<CounterState>()
        .expect("the framed bundle decodes back to the same state kind");
    assert_eq!(recovered, value);
}

/// A bundle framed under one state shape and recovered against a reshaped
/// one decodes to `None` while leaving the raw bytes present — the exact
/// condition the generated `on_rehydrate` reads to fire its warn before
/// booting fresh.
#[test]
fn reshaped_state_kind_misses_decode_with_bytes_present() {
    let value = CounterState { count: 7 };

    let mut ctx = CaptureCtx::default();
    ctx.save_state_kind::<CounterState>(0, &value);
    let (_, buf) = ctx.saved.expect("dehydrate deposits a bundle");

    // The reshaped kind has a different `Kind::ID`, so the leading-id
    // compare in `as_kind` rejects before the structured decode runs.
    assert_ne!(
        CounterState::ID,
        CounterStateReshaped::ID,
        "reshaping the state kind must change Kind::ID for the decode-miss to be real",
    );

    let prior = prior_from(&buf);
    assert!(
        prior.as_kind::<CounterStateReshaped>().is_none(),
        "a reshaped state kind must not decode the old bundle",
    );
    assert!(
        !prior.bytes().is_empty(),
        "the raw bytes stay present on a decode-miss — this is the warn trigger",
    );
}
