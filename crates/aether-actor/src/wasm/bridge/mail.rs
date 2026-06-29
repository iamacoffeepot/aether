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
//! reads the correlation id the host minted for the most-recent `send_mail`.
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
pub fn send_mail(
    recipient: u64,
    kind: u64,
    bytes: &[u8],
    count: u32,
    detached: bool,
    from: u64,
) -> u32 {
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
    unsafe {
        raw::reply_mail(
            sender,
            kind,
            bytes.as_ptr().addr() as u32,
            bytes.len() as u32,
            count,
            from,
        )
    }
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

/// ADR-0097: stage a sibling-spawn request and return the new
/// instance's `MailboxId`. `tag` is the sibling type's actor-type
/// tag (`mailbox_id_from_name(NAMESPACE)`); `is_counter` selects
/// `Subname::Counter` (the host appends a monotonic discriminator)
/// vs a caller-supplied name; `subname` is the full prefixed subname
/// for `Named` or the type-namespace prefix for `Counter`; `config`
/// is the encoded `Config` kind. The returned id is the spawned
/// sibling's ADR-0099 §3 lineage fold (the trampoline's carry folded
/// with the sibling's node), known synchronously — one fold step on a
/// carry the host already holds; the spawn itself completes just
/// after this call (ADR-0097 §4), so a spawn-time failure surfaces
/// asynchronously rather than here.
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

/// ADR-0114: register an inline child's alias route and return its
/// `MailboxId`. The inline analogue of `spawn_sibling`: the
/// host folds the alias id onto the parent's lineage carry and
/// registers a route to the parent trampoline's own slot, so the
/// co-located child is addressable like any actor with no new
/// trampoline. `is_counter` selects `Subname::Counter` (the host
/// appends a monotonic discriminator) vs a caller-supplied name;
/// `subname` is the bare `Named` segment (empty for `Counter`). No
/// config crosses here — the guest runs the child's `init` in-process
/// (see [`crate::WasmCtx::spawn_inline_child`]). The returned id is the
/// ADR-0099 §3 lineage fold, known synchronously; `0` on a host-side
/// error.
#[must_use]
pub fn spawn_inline_child(is_counter: bool, subname: &str) -> u64 {
    let subname_bytes = subname.as_bytes();
    // SAFETY: forwards to `raw::spawn_inline_child`, whose ABI is
    // documented at the import site in `ffi/raw.rs`. The `(ptr, len)`
    // pair is derived from a reference valid for `len` bytes for the
    // call's duration; the host copies before returning.
    unsafe {
        raw::spawn_inline_child(
            u32::from(is_counter),
            subname_bytes.as_ptr().addr() as u32,
            subname_bytes.len() as u32,
        )
    }
}
