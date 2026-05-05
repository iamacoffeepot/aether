//! Issue 552 stage 1: cross-transport actor-typed sender surface.
//!
//! Both the wasm-guest [`Ctx<'a, WasmTransport>`] and the native
//! [`aether_substrate::NativeCtx<'a>`] implement [`Sender`] and
//! [`MailCtx`]. The traits are the language stage-3 senders walk
//! against once `resolve_mailbox::<K>(name)` retires — `ctx.send::<R>(&kind)`
//! is the same call shape everywhere, with the trait body picking
//! the per-transport routing.
//!
//! [`Sender`] is the addressing minimum every per-handler ctx and
//! every init-time ctx exposes — single-payload `send`, batched
//! `send_many` (cast-only, see [`crate::sink::Mailbox::send_many`]
//! for the wire-shape rationale), plus `send_to_named` as the
//! string-keyed escape hatch for cases where the caller genuinely
//! has no Rust type at compile site.
//!
//! [`MailCtx`] adds the per-mail surface that only makes sense
//! while a handler is running: read the inbound's `sender` (so
//! a reply can route back to the originator) and `reply` to it
//! without rethreading the handle. Init contexts implement
//! [`Sender`] but NOT [`MailCtx`] — there's no active inbound mail
//! at boot time, so neither a sender nor a reply target is defined.
//!
//! The two-trait split mirrors the existing `InitCtx` / `Ctx` /
//! `DropCtx` typestate fence (init can resolve, receive can send /
//! reply): `Sender` is "you can send mail," `MailCtx` is "and you
//! also have an active inbound to reply to." Stage 2 caps and
//! stage 3 components will program against [`Sender`] / [`MailCtx`]
//! generic over the ctx type when they need to be ctx-agnostic.

use aether_data::Kind;

use crate::actor::{Actor, HandlesKind};

/// Outbound-mail surface every actor ctx exposes. Implementations
/// route through their owning transport — the wasm impl on
/// [`crate::Ctx<'a, WasmTransport>`] dispatches through host fns;
/// the native impl on `NativeCtx<'_>` (in `aether-substrate`)
/// pushes onto the cross-actor `Arc<Mailer>` queue.
///
/// `R` is the receiving actor type. `R::NAMESPACE` resolves to the
/// receiver's mailbox id at compile time (ADR-0029 stable hash).
/// The `R: Actor + HandlesKind<K>` bound is the compile-time gate:
/// trying to send a kind the receiver doesn't handle is rejected
/// at the call site, not silently warn-dropped at runtime.
pub trait Sender {
    /// Send a single payload of kind `K` to the singleton instance
    /// of receiver actor `R`. The receiver's mailbox is resolved
    /// from `R::NAMESPACE`; the kind is `K::ID`. Wire shape (cast
    /// or postcard) follows `Kind::encode_into_bytes` (issue #240).
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Actor + HandlesKind<K>,
        K: Kind;

    /// Send a slice of cast-shape payloads as a contiguous batch.
    /// Cast-only — postcard has no efficient batched wire shape, so
    /// senders that need to fan out N postcard payloads call
    /// [`Self::send`] in a loop. ADR-0019 §6 fixes the batch wire
    /// as raw bytes; `count = payloads.len()`.
    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Actor + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit;

    /// String-keyed escape hatch for callers that genuinely don't
    /// know the receiver type at compile site (debug tools, dynamic
    /// dispatch, components addressing user-named mailboxes the
    /// substrate registered without a corresponding Rust type).
    /// Survives stage 4 alongside the actor-typed sends.
    fn send_to_named<K: Kind>(&mut self, name: &str, payload: &K);
}

/// Per-handler ctx surface, on top of [`Sender`]. Adds reply-to-
/// originator: handlers call [`Self::reply::<K>(&payload)`][Self::reply]
/// without threading a per-call sender argument; the ctx pulled the
/// inbound's reply target out of the dispatcher and stashed it
/// internally.
///
/// Init contexts deliberately don't implement this — there's no
/// inbound-mail context at boot time. Per-handler ctxs (`WasmCtx`,
/// `NativeCtx`) do.
///
/// Note: Stage 1 deliberately omits a `sender()` accessor. The
/// wasm-side reply handle (opaque `u32`) and the substrate-side
/// `aether_data::ReplyTo` (target + correlation) carry different
/// shapes — no single return type fits both transports honestly.
/// The two sides converge through the implicit reply path
/// ([`Self::reply`]) that knows internally which shape it holds.
/// Stage 2 may add an `Option<…>`-shaped accessor if a handler
/// needs to inspect the sender (multi-tenancy, audit trails) —
/// today's caps don't, so the accessor doesn't ship pre-emptively.
pub trait MailCtx: Sender {
    /// Reply to the originator of the mail currently being dispatched.
    /// No-op when there's no reply target (broadcast / peer-component
    /// mail). The wire shape (cast or postcard) follows
    /// `Kind::encode_into_bytes`.
    ///
    /// Stage 1 ships the trait method; stage 2/3 fold call sites
    /// over from the existing `Ctx::reply(sender, kind, payload)`
    /// shape onto `ctx.reply::<K>(&payload)`.
    fn reply<K: Kind>(&mut self, payload: &K);
}
