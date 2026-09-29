// Wire-encode: `usize → u32` narrowings forward `(ptr, len)` pairs
// to the wasm32 host-fn ABI. wasm32 already has 32-bit addresses;
// `_p32`-suffixed FFI per ADR-0024 documents the convention.
#![allow(clippy::cast_possible_truncation)]

//! Outbound-mail FFI bridge — free functions in a `pub(crate)` module.
//!
//! Each function forwards to the matching `extern "C"` host fn in [`raw`]
//! and localizes `unsafe` to one audited site per FFI op. `send_mail`
//! pushes a typed payload at a recipient mailbox; `reply_mail` routes to
//! the originator of the mail currently being dispatched; `prev_correlation`
//! reads the correlation id the host minted for the most-recent `send_mail`;
//! `reply_correlation` reads the current inbound reply's echoed correlation.
//!
//! Correlation is universal — every send mints a correlation id so a
//! handler can match the reply to the request it sent. It's a property
//! of the outbound mail, so it lives in this module.
//!
//! Log-event emission lives in the sibling [`crate::wasm::bridge::log`]
//! module (a distinct FFI op family).

use crate::wasm::raw;

/// Push a typed payload at `recipient`. `bytes` is the wire
/// encoding of the payload (cast for `#[repr(C)]` kinds, structured
/// for schema-shaped kinds — `Kind::encode_into_bytes` already
/// resolves which). `count` is `1` for a single send and N for a
/// batch (cast-only — structured kinds have no efficient batched wire
/// shape, see `WasmCtx::send_many`).
///
/// `detached` carries the ADR-0080 §7 lineage signal. `false` (the
/// default `send` path) lets the host stamp the in-flight
/// dispatch's `parent`/`root` onto this send, so the recipient's
/// work stays in the caller's causal chain. `true` (`send_detached`)
/// suppresses inheritance — the host mints a fresh root chain. The
/// guest holds no trace ids, so the flag is all it can contribute;
/// the host owns the stamping.
///
/// `from` (issue 1987) is the sending actor's own folded `MailboxId`
/// raw value — the dispatch identity carried on the send so the host
/// stamps it as origin without consulting an ambient per-receive cell.
/// The host validates it is in-cluster and falls back to the
/// component's own id for a zero / foreign value.
///
/// Returns `0` on success; `1` on substrate-side recipient
/// lookup miss. Other non-zero values are reserved for future
/// host-side failure surfaces.
///
/// Not `#[must_use]`: the public ctx surfaces (the flat `send` verbs,
/// `MailSender::send_detached_to`, `OutboundReply::reply`, etc.) are
/// defined as fire-and-forget and have no return channel for
/// a lookup-miss status. The substrate warn-drops unknown
/// recipients on its side, which is the diagnostic path; the guest
/// can't surface the status anywhere meaningful.
#[allow(
    clippy::must_use_candidate,
    reason = "fire-and-forget by contract — see doc-comment above; #[must_use] retired in issue 892"
)]
pub fn send_mail(recipient: u64, kind: u64, bytes: &[u8], count: u32, detached: bool, from: u64) -> u32 {
    // SAFETY: forwards to `raw::send_mail`, whose ABI is documented
    // at the import site in `ffi/raw.rs`. The `(ptr, len)` pair is
    // derived from the `&[u8]` slice we just received, which the
    // borrow checker proves is valid for `bytes.len()` bytes for
    // the duration of the call; the host copies before returning.
    unsafe {
        raw::send_mail(
            recipient,
            kind,
            bytes.as_ptr().addr() as u32,
            bytes.len() as u32,
            count,
            u32::from(detached),
            from,
        )
    }
}

/// Reply to the originator of the mail currently being dispatched
/// (ADR-0013). `sender` is the per-instance handle the dispatcher
/// threaded onto the ctx at receive time; the substrate routes it
/// to the right Claude session, sibling component, or remote
/// engine mailbox. `from` (issue 1987) is the replying actor's own
/// folded `MailboxId` raw value — the dispatch identity stamped on
/// the reply's lineage, validated host-side like `send_mail`'s.
///
/// Not `#[must_use]`: the trait surface (`OutboundReply::reply`)
/// is fire-and-forget by contract — see the
/// matching rationale on `send_mail`.
#[allow(
    clippy::must_use_candidate,
    reason = "fire-and-forget by contract — see doc-comment above; #[must_use] retired in issue 892"
)]
pub fn reply_mail(sender: u32, kind: u64, bytes: &[u8], count: u32, from: u64) -> u32 {
    // SAFETY: forwards to `raw::reply_mail`, whose ABI is documented
    // at the import site in `ffi/raw.rs`. The `(ptr, len)` pair is
    // derived from the `&[u8]` slice we just received, which the
    // borrow checker proves is valid for `bytes.len()` bytes for
    // the duration of the call; the host copies before returning.
    unsafe { raw::reply_mail(sender, kind, bytes.as_ptr().addr() as u32, bytes.len() as u32, count, from) }
}

/// Register the reply the dispatch in progress held on reply handle
/// `sender` (ADR-0243 §6): `kind` and the encoded `unanswered` value the
/// host sends the requester if this instance closes before answering.
/// Returns `0` on success and one of `reply_mail`'s refusal statuses
/// otherwise; the caller panics on a refusal.
#[must_use]
pub fn held_unanswered(sender: u32, kind: u64, bytes: &[u8]) -> u32 {
    // SAFETY: forwards to `raw::held_unanswered`, whose ABI is documented at
    // the import site in `raw.rs`. The `(ptr, len)` pair is derived from the
    // `&[u8]` slice we just received, valid for the call; the host copies
    // before returning.
    unsafe { raw::held_unanswered(sender, kind, bytes.as_ptr().addr() as u32, bytes.len() as u32) }
}

/// Correlation id the host minted for this actor's most recent
/// `send_mail` call (ADR-0042). `0` before any send. Universal —
/// every send mints a correlation; a handler stashes it and
/// matches it against the inbound reply's correlation to pair a
/// reply with the request it sent.
#[must_use]
pub fn prev_correlation() -> u64 {
    // SAFETY: `raw::prev_correlation` takes no arguments and reads
    // a host-side scalar set on the most recent `send_mail`; no
    // ABI invariants to uphold beyond "we are the FFI guest", which
    // the `#[cfg(target_family = "wasm")]` import gate enforces
    // (the host-target stub panics rather than returning garbage).
    unsafe { raw::prev_correlation() }
}

/// Correlation id echoed on the reply currently being dispatched.
/// Returns `0` when the inbound mail is not a reply envelope.
#[must_use]
pub fn reply_correlation() -> u64 {
    // SAFETY: `raw::reply_correlation` takes no arguments and reads a
    // host-side scalar set for the active dispatch.
    unsafe { raw::reply_correlation() }
}

/// ADR-0114 + issue 4490: allocate an inline-child alias beneath `parent`
/// and return its `MailboxId`. The host validates `parent` as this
/// component's root or inline alias before folding or rendering the new
/// address, and publishes the namespace and contract rows of the child's
/// actor type `tag` on the alias (ADR-0231 §4); a tag the resident module
/// does not declare allocates no alias. `is_counter` selects
/// `Subname::Counter` (the host appends a monotonic discriminator) vs a
/// caller-supplied name; `subname` is the bare `Named` segment (empty for
/// `Counter`). No config crosses here — the guest runs the child's `init`
/// in-process (see [`crate::WasmCtx::spawn_inline_child`]). The returned id
/// is the ADR-0099 §3 lineage fold, known synchronously; `0` on a host-side
/// error.
#[must_use]
pub fn spawn_inline_child(parent: u64, tag: u64, is_counter: bool, subname: &str) -> u64 {
    let subname_bytes = subname.as_bytes();
    // SAFETY: the slice remains valid for the call and is copied host-side;
    // the scalar parent is validated against the active component cluster.
    unsafe {
        raw::spawn_inline_child(
            parent,
            tag,
            u32::from(is_counter),
            subname_bytes.as_ptr().addr() as u32,
            subname_bytes.len() as u32,
        )
    }
}

/// ADR-0114 teardown (#4228): retire the alias route
/// [`spawn_inline_child`] registered, now that the child it addressed has
/// been despawned. `alias` is that call's returned raw `MailboxId`. The host
/// retires the route and fires the departure notices the alias's watchers are
/// owed, so an address never outlives the actor it named. `true` when the
/// retirement was staged, `false` when `alias` is not this component's own
/// inline-child alias (see [`crate::WasmCtx::despawn_inline_child`]).
#[cfg(target_family = "wasm")]
pub fn despawn_inline_child(alias: u64) -> bool {
    // SAFETY: forwards to `raw::despawn_inline_child`, whose ABI is
    // documented at the import site in `raw.rs`. The argument is a plain
    // scalar — no pointer crosses, so there is nothing for the host to read
    // out of guest memory.
    unsafe { raw::despawn_inline_child(alias) == 1 }
}
