//! ADR-0243 §6–§7: the guest's typed deferred reply — the `hold` pair, the
//! fail-fast drops, the ledger-granting context and saved-state codecs, and
//! the dehydrate and untaken-reply guards.
//!
//! The host build has no FFI, so a `Held` here is never answered: each test
//! that ends with a live ticket releases it from the registry and forgets the
//! value, which is what `Held::answer` does after its send.

extern crate std;

use std::panic;

use alloc::string::String;
use alloc::vec::Vec;
use core::mem;
use core::panic::AssertUnwindSafe;

use aether_data::{Kind, RequestId, wire};

use super::{NO_INBOUND_SOURCE, Registry, WasmCtx};
use crate::HeldReply;
use crate::mail::{Mail, NO_REPLY_HANDLE, PriorState, ReplyHandle};
use crate::model::ctx::{Erased, Unchecked};
use crate::request_context::split_state_envelope;
use crate::wasm::ctx::{CapturedState, Held, WasmDropCtx};
use crate::wasm::{ActorInitError, WasmInitCtx};

#[aether_data::kind(name = "test.held.answer")]
struct Answer {
    value: u32,
}

impl HeldReply for Answer {
    fn unanswered() -> Self {
        Self { value: u32::MAX }
    }
}

#[aether_data::kind(name = "test.held.context")]
struct HeldContext {
    debt: Held<Answer>,
    tag: u32,
}

#[aether_data::kind(name = "test.held.state")]
struct HeldState {
    debt: Held<Answer>,
}

#[aether_data::kind(name = "test.held.plain_state", eq)]
struct PlainState {
    count: u32,
    label: String,
}

#[aether_data::kind(name = "test.held.ask")]
struct Ask {
    value: u32,
}

const ACTOR: u64 = 0x10;

/// A ctx dispatching mail whose reply handle is `handle`.
fn ctx_for(registry: &Registry, handle: u32) -> WasmCtx<'_, Erased, Unchecked> {
    let mut ctx = WasmCtx::__new(ACTOR, registry, NO_INBOUND_SOURCE);
    ctx.__set_reply_to(Some(ReplyHandle::__from_raw(handle)));
    ctx
}

/// Holds `Answer` on the mail it is asked and parks the ticket in its state
/// (the `Deferrer` pattern in `tests/dispatch.rs`), so [`hold_on`] reaches a
/// hold through a real generated arm rather than a hand-built ctx and a
/// direct `hold` call.
struct Holder {
    parked: Option<Held<Answer>>,
}

#[crate::actor]
impl crate::WasmActor for Holder {
    const NAMESPACE: &'static str = "test.held.holder";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self { parked: None })
    }

    #[handler::single]
    fn on_ask(&mut self, ctx: &mut WasmCtx<'_>, _ask: Ask) -> crate::Pending<Answer> {
        let (pending, held) = ctx.hold::<Answer>();
        self.parked = Some(held);
        pending
    }
}

/// Dispatch a real [`Holder`] mail on `handle`, so the `#[actor]`-generated
/// `Unchecked` arm accepts the returned receipt, and return its ticket. The
/// registration the hold staged stays staged, as it is when the dispatch
/// returns to the `receive` shim.
fn dispatch_hold(registry: &Registry, handle: u32) -> Held<Answer> {
    let mut holder = Holder { parked: None };
    let payload = Ask { value: 0 }.encode_into_bytes();
    // SAFETY: `payload` outlives the `Mail` built over it.
    let mail = unsafe { Mail::__from_ptr(Ask::ID.0, payload.as_ptr().addr(), payload.len() as u32, 1, handle, ACTOR) };
    let mut ctx: WasmCtx<'_, Erased, Unchecked> = WasmCtx::__new(ACTOR, registry, NO_INBOUND_SOURCE);
    let rc = <Holder as crate::WasmDispatch<Holder>>::dispatch(&mut holder, &mut ctx, mail);
    assert_eq!(rc, crate::DISPATCH_HANDLED_HOLD, "a single `-> Pending<R>` arm reports the hold");
    holder.parked.take().expect("the handler parked its ticket")
}

/// Hold a reply on `handle` and return its ticket, taking the staged
/// registration as the `receive` shim's flush does.
fn hold_on(registry: &Registry, handle: u32) -> Held<Answer> {
    let held = dispatch_hold(registry, handle);
    registry.take_unanswered().expect("a live hold stages its unanswered reply");
    held
}

/// Stand in for `Held::answer` on the host: release the ticket and forget the
/// value.
fn discharge(registry: &Registry, held: Held<Answer>) {
    registry.release_held(held.ticket());
    mem::forget(held);
}

/// A handler whose own dispatch calls `hold` twice, the misuse `hold`'s
/// own guard catches: two holds in one dispatch would owe two replies to
/// one request.
struct DoubleHolder;

#[crate::actor]
impl crate::WasmActor for DoubleHolder {
    const NAMESPACE: &'static str = "test.held.double_holder";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_ask(&mut self, ctx: &mut WasmCtx<'_>, _ask: Ask) -> crate::Pending<Answer> {
        let _ = self;
        let (pending, held) = ctx.hold::<Answer>();
        mem::forget(pending);
        mem::forget(held);
        ctx.hold::<Answer>().0
    }
}

#[test]
#[should_panic(expected = "`hold` called twice in one dispatch")]
fn second_hold_panics() {
    let registry = Registry::new();
    let mut holder = DoubleHolder;
    let payload = Ask { value: 0 }.encode_into_bytes();
    // SAFETY: `payload` outlives the `Mail` built over it.
    let mail = unsafe { Mail::__from_ptr(Ask::ID.0, payload.as_ptr().addr(), payload.len() as u32, 1, 5, ACTOR) };
    let mut ctx: WasmCtx<'_, Erased, Unchecked> = WasmCtx::__new(ACTOR, &registry, NO_INBOUND_SOURCE);
    let _ = <DoubleHolder as crate::WasmDispatch<DoubleHolder>>::dispatch(&mut holder, &mut ctx, mail);
}

/// A handler that holds and discards its receipt would report a reply it
/// never declares; the dropped receipt must fail fast.
#[test]
#[should_panic(expected = "a `Pending` receipt was dropped")]
fn unreturned_pending_panics() {
    let registry = Registry::new();
    let (pending, held) = ctx_for(&registry, 5).as_single().hold::<Answer>();
    mem::forget(held);
    drop(pending);
}

/// The receipt the dispatch view accepts must not trap, or every deferred
/// handler would.
#[test]
fn accepted_pending_is_silent() {
    let registry = Registry::new();
    let held = hold_on(&registry, 5);
    discharge(&registry, held);
}

/// Catches a hold that registers no unanswered reply, or registers another
/// kind's or another handle's, so the host has nothing to send, or the wrong
/// thing, when the holder closes first; and a detached hold that registers
/// one for a request with no reply target.
#[test]
fn hold_stages_its_unanswered_reply() {
    let registry = Registry::new();
    let held = dispatch_hold(&registry, 5);

    let staged = registry.take_unanswered().expect("a live hold stages its unanswered reply");
    assert_eq!((staged.ticket, staged.reply), (5, Answer::ID));
    assert_eq!(staged.mail.bytes, Answer::unanswered().encode_into_bytes());
    discharge(&registry, held);

    let detached = dispatch_hold(&registry, NO_REPLY_HANDLE);
    assert!(registry.take_unanswered().is_none(), "a detached hold owes no reply to register");
    drop(detached);
}

/// A ticket dropped live loses the requester's reply; it must fail fast.
#[test]
#[should_panic(expected = "was dropped unanswered")]
fn unanswered_held_panics() {
    let registry = Registry::new();
    drop(hold_on(&registry, 5));
}

/// A stored context parks its ticket, so the value it leaves behind drops
/// silently; without the flag every correct park would trap.
#[test]
fn parked_held_is_silent() {
    let registry = Registry::new();
    let debt = hold_on(&registry, 5);
    registry.insert_request_context(RequestId(7), HeldContext { debt, tag: 1 });

    assert!(!registry.__held_unsaved(), "the parked ticket is owned by the stored context, not live");
    let context = registry.take_request_context::<HeldContext>(RequestId(7)).expect("the context takes back");
    discharge(&registry, context.debt);
}

/// A plain encode grants no ledger: it must refuse the ticket and leave it
/// armed, so a stray encode can never defuse a debt.
#[test]
fn plain_encode_refuses_and_leaves_held_armed() {
    let registry = Registry::new();
    let context = HeldContext { debt: hold_on(&registry, 5), tag: 1 };

    let encoded = panic::catch_unwind(AssertUnwindSafe(|| context.encode_into_bytes()));
    assert!(encoded.is_err(), "a plain encode refuses a held ticket");
    assert!(!context.debt.is_parked(), "the refused ticket stays armed");
    discharge(&registry, context.debt);
}

/// A context take claims its ticket back live and answerable, and the same
/// ticket cannot be claimed a second time.
#[test]
fn context_round_trip_claims_the_ticket() {
    let registry = Registry::new();
    registry.insert_request_context(RequestId(7), HeldContext { debt: hold_on(&registry, 5), tag: 3 });
    let (version, snapshot) = registry.compose_request_context_state(None).expect("a stored context snapshots");

    let context = registry.take_request_context::<HeldContext>(RequestId(7)).expect("the context takes back");
    assert_eq!((context.debt.ticket(), context.tag), (5, 3));
    assert!(!context.debt.is_parked(), "the claimed ticket is live again");
    assert!(registry.__held_unsaved(), "the registry tracks the claimed ticket as live");

    registry.restore_request_contexts(split_state_envelope(version, &snapshot).0);
    assert!(
        registry.take_request_context::<HeldContext>(RequestId(7)).is_none(),
        "a ticket already live cannot be claimed again",
    );
    discharge(&registry, context.debt);
}

/// A reply that leaves a held context untaken strands its debt; the check
/// must name the stored kind, and a context holding no ticket is exempt.
#[test]
#[should_panic(expected = "left its context `test.held.context` untaken")]
fn untaken_held_context_panics_naming_its_kind() {
    let registry = Registry::new();
    registry.insert_request_context(RequestId(8), PlainState { count: 1, label: String::new() });
    registry.check_held_context_taken(RequestId(8));

    registry.insert_request_context(RequestId(7), HeldContext { debt: hold_on(&registry, 5), tag: 1 });
    registry.check_held_context_taken(RequestId(7));
}

/// A ticket saved through the dehydrate ctx passes the guard, a live unsaved
/// one refuses it, a refusal reverts the saved ones to live, and the saved
/// bytes claim back in a replacement's registry exactly once.
#[test]
fn dehydrate_refuses_live_unsaved_and_reverts() {
    let registry = Registry::new();
    let saved = hold_on(&registry, 5);
    let unsaved = hold_on(&registry, 6);

    let mut capture = CapturedState::default();
    WasmDropCtx::__new_capturing(ACTOR, &mut capture, &registry).save_state_kind(0, &HeldState { debt: saved });
    assert!(registry.__held_unsaved(), "the unsaved ticket refuses the dehydrate");

    discharge(&registry, unsaved);
    assert!(!registry.__held_unsaved(), "a saved ticket passes");
    registry.__revert_dehydrate();
    assert!(registry.__held_unsaved(), "a refused dehydrate returns its saved tickets to live");

    let (version, bytes) = capture.take().expect("the dehydrate saved state");
    let successor = Registry::new();
    // SAFETY: `bytes` outlives both `PriorState` values built over it.
    let prior = || unsafe { PriorState::__from_ptr(version, bytes.as_ptr().addr(), bytes.len()) };
    let state = prior().__with_registry(&successor).decode_kind::<HeldState>().expect("the saved ticket claims");
    assert_eq!(state.debt.ticket(), 5);
    assert!(successor.__held_unsaved(), "the claimed ticket is live in the successor");
    assert!(prior().__with_registry(&successor).decode_kind::<HeldState>().is_none(), "a second claim refuses");
    discharge(&successor, state.debt);
}

/// An existing serde state kind frames as `K::ID` then its serde wire bytes,
/// the layout older SDKs write and read.
#[test]
fn save_state_kind_bytes_match_serde() {
    let registry = Registry::new();
    let value = PlainState { count: 7, label: String::from("seven") };

    let mut capture = CapturedState::default();
    WasmDropCtx::__new_capturing(ACTOR, &mut capture, &registry).save_state_kind(0, &value);

    // Tripwire: the saved-state framing is the cross-SDK replace format; a
    // drift here breaks replace between old and new guests.
    let mut expected = Vec::from(PlainState::ID.0.to_le_bytes());
    expected.extend(wire::to_vec(&value).expect("serde wire encodes"));
    assert_eq!(capture.take(), Some((0, expected)));
}
