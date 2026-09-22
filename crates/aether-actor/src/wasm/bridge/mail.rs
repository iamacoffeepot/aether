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
/// shape, see `WasmActorMailbox::send_many`).
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
/// Not `#[must_use]`: the public ctx surfaces (`MailSender::send`,
/// `MailSender::send_to_named`, `OutboundReply::reply`, etc.) are
/// trait-defined as fire-and-forget and have no return channel for
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

/// ADR-0097: stage a sibling-spawn request and return the new
/// instance's `MailboxId`. `tag` is the sibling type's actor-type
/// tag (`mailbox_id_from_name(NAMESPACE)`); `is_counter` selects
/// `Subname::Counter` (the host appends a monotonic discriminator)
/// vs a caller-supplied name; `subname` is the full prefixed subname
/// for `Named` or the type-namespace prefix for `Counter`; `config`
/// is the encoded `Config` kind. The returned id is the spawned
/// sibling's ADR-0099 §3 lineage fold (the component root folded
/// with the sibling's node), known synchronously — one fold step on a
/// carry the host already holds; the spawn itself completes just
/// after this call (ADR-0097 §4), so a spawn-time failure surfaces
/// asynchronously rather than here.
#[allow(dead_code, reason = "legacy guest ABI bridge retained during the scoped-spawn migration")]
#[must_use]
pub fn spawn_sibling(tag: u64, is_counter: bool, subname: &str, config: &[u8]) -> u64 {
    let subname_bytes = subname.as_bytes();
    // SAFETY: forwards to `raw::spawn_sibling`, whose ABI is
    // documented at the import site in `ffi/raw.rs`. Both `(ptr,
    // len)` pairs are derived from references valid for `len` bytes
    // for the call's duration; the host copies before returning.
    unsafe {
        raw::spawn_sibling(
            tag,
            u32::from(is_counter),
            subname_bytes.as_ptr().addr() as u32,
            subname_bytes.len() as u32,
            config.as_ptr().addr() as u32,
            config.len() as u32,
        )
    }
}

/// Issue 4490: stage a sibling beneath the executing actor rather than the
/// component root. `parent` is the caller's current mailbox; the host accepts
/// it only when it belongs to this component cluster. The legacy
/// [`spawn_sibling`] bridge remains for already-built guest compatibility.
#[must_use]
pub fn spawn_sibling_scoped(parent: u64, tag: u64, is_counter: bool, subname: &str, config: &[u8]) -> u64 {
    let subname_bytes = subname.as_bytes();
    // SAFETY: both pointer/length pairs are borrowed for this call and the
    // host copies them before returning; the scalar parent is guest-carried
    // identity that the host validates before use.
    unsafe {
        raw::spawn_sibling_scoped(
            parent,
            tag,
            u32::from(is_counter),
            subname_bytes.as_ptr().addr() as u32,
            subname_bytes.len() as u32,
            config.as_ptr().addr() as u32,
            config.len() as u32,
        )
    }
}

/// ADR-0114: register an inline child's alias route and return its
/// `MailboxId`. The inline analogue of `spawn_sibling`: the
/// legacy host folds the alias id onto the component root and registers a
/// route to that trampoline's own slot, so the
/// co-located child is addressable like any actor with no new
/// trampoline. `is_counter` selects `Subname::Counter` (the host
/// appends a monotonic discriminator) vs a caller-supplied name;
/// `subname` is the bare `Named` segment (empty for `Counter`). No
/// config crosses here — the guest runs the child's `init` in-process
/// (see [`crate::WasmCtx::spawn_inline_child`]). The returned id is the
/// ADR-0099 §3 lineage fold, known synchronously; `0` on a host-side
/// error.
#[allow(dead_code, reason = "legacy guest ABI bridge retained during the scoped-spawn migration")]
#[must_use]
pub fn spawn_inline_child(is_counter: bool, subname: &str) -> u64 {
    let subname_bytes = subname.as_bytes();
    // SAFETY: forwards to `raw::spawn_inline_child`, whose ABI is
    // documented at the import site in `ffi/raw.rs`. The `(ptr, len)`
    // pair is derived from a reference valid for `len` bytes for the
    // call's duration; the host copies before returning.
    unsafe {
        raw::spawn_inline_child(u32::from(is_counter), subname_bytes.as_ptr().addr() as u32, subname_bytes.len() as u32)
    }
}

/// Issue 4490: allocate an inline alias beneath the executing actor. The
/// host validates `parent` as this component's root or inline alias before
/// folding or rendering the new address. The unscoped bridge remains only
/// for staged compatibility with legacy guests.
#[must_use]
pub fn spawn_inline_child_scoped(parent: u64, is_counter: bool, subname: &str) -> u64 {
    let subname_bytes = subname.as_bytes();
    // SAFETY: the slice remains valid for the call and is copied host-side;
    // the scalar parent is validated against the active component cluster.
    unsafe {
        raw::spawn_inline_child_scoped(
            parent,
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
