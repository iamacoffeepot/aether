// Raw FFI boundary with the substrate. This is the only place in a
// guest tree that should write `extern "C"` decls or host-stub panics;
// everything else goes through the typed wrappers in `lib.rs`.
//
// On the FFI guest target (today: `wasm32-unknown-unknown`) the fns
// are imports from the `aether` module the substrate's wasm runtime
// exposes (see `aether-substrate/src/actor/wasm/host_fns.rs`). On
// any other target they're stubs that panic if called, which keeps
// the crate (and every actor crate that depends on it) compilable
// for `cargo test --workspace` on the host — actors can still be
// unit-tested for pure logic there, they just can't cross the FFI.
//
// The `target_arch = "wasm32"` cfg gate matches the only FFI host
// the substrate ships today. A future C / OS-process host would
// either pick a different gate or drop the cfg entirely; the
// import surface itself is target-agnostic.
//
// ADR-0024 Phase 1: the FFI-visible import names carry a `_p32`
// suffix in anticipation of a future `_p64` sibling for wasm64
// guests. The Rust-side identifiers stay un-suffixed (`send_mail`,
// not `send_mail_p32`) so callers in `lib.rs` don't have to thread
// the suffix through every call site — `#[link_name]` does the
// remap.

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "aether")]
unsafe extern "C" {
    /// `detached` is the ADR-0080 §7 lineage signal: `0` inherits the
    /// in-flight dispatch's `parent`/`root` (the host stamps them onto
    /// this send), `1` suppresses inheritance so the host mints a fresh
    /// causal chain. The default guest path passes `0`; `send_detached`
    /// passes `1`.
    ///
    /// `from` (issue 1987) is the sending actor's own folded `MailboxId`
    /// raw value — the dispatch identity the host stamps as origin. The
    /// host validates it is in-cluster (the component's own id or a
    /// registered inline-child alias) and falls back to the component's
    /// own id for a zero / foreign value, so a guest can only claim an
    /// origin inside its own cluster. Carrying it on the send is what
    /// retires the host's ambient per-receive dispatch-identity cell.
    #[link_name = "send_mail_p32"]
    pub fn send_mail(
        recipient: u64,
        kind: u64,
        ptr: u32,
        len: u32,
        count: u32,
        detached: u32,
        from: u64,
    ) -> u32;
    /// `from` (issue 1987) is the replying actor's own folded `MailboxId`
    /// raw value — the dispatch identity the host stamps on the reply's
    /// lineage, validated and fallback-resolved exactly like `send_mail`.
    #[link_name = "reply_mail_p32"]
    pub fn reply_mail(sender: u32, kind: u64, ptr: u32, len: u32, count: u32, from: u64) -> u32;
    #[link_name = "save_state_p32"]
    pub fn save_state(version: u32, ptr: u32, len: u32) -> u32;
    /// ADR-0042: return the correlation id the substrate minted for
    /// this component's most recent `send_mail`. `0` before any
    /// send. A handler filters its inbound reply on this so it picks
    /// "the reply I just sent a request for" rather than "any reply
    /// of this kind."
    #[link_name = "prev_correlation_p32"]
    pub fn prev_correlation() -> u64;
    /// Resolve the source mailbox of the mail bound to `handle`. Returns
    /// the sender's `MailboxId` raw value when the inbound mail originated
    /// from a peer component (`SourceAddr::Component`); returns `0`
    /// (`MailboxId::NONE`) for Session / EngineMailbox / None sources,
    /// for `NO_REPLY_HANDLE`, and for unknown handles (issue 1958).
    #[link_name = "source_of_p32"]
    pub fn source_of(handle: u32) -> u64;
    /// Issue 525 Phase 4b / issue 531: stage a `BootError` message
    /// for the substrate to surface in `LoadResult::Err` after the
    /// guest's `init` returns non-zero. The `export!` macro is the
    /// only intended caller — user code returns `Err(BootError)`
    /// from `FfiActor::init` and the macro plumbs the bytes
    /// through this import. Bytes at `(ptr, len)` are copied out of
    /// guest memory before the call returns.
    #[link_name = "init_failed_p32"]
    pub fn init_failed(ptr: u32, len: u32);
    /// ADR-0081 §7: re-emit one `tracing::*` event on the host side
    /// so the trampoline's `ActorAwareLayer` lands it in this guest's
    /// per-actor `ActorLogRing`. Called by `WasmSubscriber::event`
    /// per event. `level` follows the `0 = trace .. 4 = error`
    /// mapping the rest of `aether.log.*` uses. `target_ptr/len` and
    /// `message_ptr/len` are byte slices in guest memory; the host
    /// copies before returning.
    #[link_name = "log_event_p32"]
    pub fn log_event(
        level: u32,
        target_ptr: u32,
        target_len: u32,
        message_ptr: u32,
        message_len: u32,
    );
    /// ADR-0097: spawn a sibling actor type from the same resident
    /// module. `tag` is the sibling's actor-type tag
    /// (`mailbox_id_from_name(NAMESPACE)`), used to pick the export at
    /// `init_typed_p32`. `is_counter` is `1` for `Subname::Counter`
    /// (the host appends a monotonic discriminator) or `0` for a
    /// caller-supplied name. `subname_ptr/len` is the full subname for
    /// `Named` or the type-namespace prefix for `Counter`;
    /// `config_ptr/len` is the encoded `Config` kind. The host stages
    /// the request and returns the new instance's `MailboxId`
    /// (the ADR-0099 §3 lineage fold of the trampoline's carry with the
    /// sibling's node); the spawn itself completes just after this
    /// returns (ADR-0097 §4). Bytes at
    /// `(subname_ptr, subname_len)` / `(config_ptr, config_len)` are
    /// copied out of guest memory before the call returns.
    #[link_name = "spawn_sibling_p32"]
    pub fn spawn_sibling(
        tag: u64,
        is_counter: u32,
        subname_ptr: u32,
        subname_len: u32,
        config_ptr: u32,
        config_len: u32,
    ) -> u64;
    /// ADR-0114: register an inline child's alias route and return its
    /// `MailboxId`. Unlike [`spawn_sibling`] (which stages a detached
    /// spawn), this folds the alias id `with_tag(Mailbox,
    /// fold_lineage(parent_carry, instanced(aether.embedded, subname)))`
    /// and synchronously registers an alias `MailboxEntry` routing to the
    /// parent trampoline's own dispatcher slot — the child is co-located
    /// in the parent's wasm instance, so there is no new trampoline and no
    /// config (the guest runs `init` in-process). `is_counter` is `1` for
    /// `Subname::Counter` (the host appends a monotonic discriminator) or
    /// `0` for a caller-supplied name; `subname_ptr/len` is the bare
    /// `Named` segment (empty for `Counter`), copied out of guest memory
    /// before the call returns. The returned id is the ADR-0099 §3 lineage
    /// fold of the trampoline's carry with the child's node; `0` on a
    /// host-side error (no memory, OOB, bad UTF-8, no binding/spawner).
    #[link_name = "spawn_inline_child_p32"]
    pub fn spawn_inline_child(is_counter: u32, subname_ptr: u32, subname_len: u32) -> u64;
}

/// Host-side stub for the FFI `aether::send_mail` import. Always
/// panics — callers outside the FFI guest are misusing the SDK.
///
/// # Safety
/// FFI-import stub; the wasm32 variant is `unsafe extern "C"`.
///
/// # Panics
/// Always panics — fail-fast per ADR-0063: the host build of the SDK
/// has no FFI host to call, so any invocation is a bug.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub unsafe fn send_mail(
    _recipient: u64,
    _kind: u64,
    _ptr: u32,
    _len: u32,
    _count: u32,
    _detached: u32,
    _from: u64,
) -> u32 {
    panic!("aether-actor: send_mail called outside the FFI guest");
}

/// Host-side stub for the FFI `aether::reply_mail` import. Always
/// panics — callers outside the FFI guest are misusing the SDK.
///
/// # Safety
/// FFI-import stub; the wasm32 variant is `unsafe extern "C"`.
///
/// # Panics
/// Always panics — fail-fast per ADR-0063: the host build of the SDK
/// has no FFI host to call, so any invocation is a bug.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub unsafe fn reply_mail(
    _sender: u32,
    _kind: u64,
    _ptr: u32,
    _len: u32,
    _count: u32,
    _from: u64,
) -> u32 {
    panic!("aether-actor: reply_mail called outside the FFI guest");
}

/// Host-side stub for the FFI `aether::save_state` import. Always
/// panics — callers outside the FFI guest are misusing the SDK.
///
/// # Safety
/// FFI-import stub; the wasm32 variant is `unsafe extern "C"`.
///
/// # Panics
/// Always panics — fail-fast per ADR-0063: the host build of the SDK
/// has no FFI host to call, so any invocation is a bug.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub unsafe fn save_state(_version: u32, _ptr: u32, _len: u32) -> u32 {
    panic!("aether-actor: save_state called outside the FFI guest");
}

/// Host-side stub for the FFI `aether::prev_correlation` import.
/// Always panics — callers outside the FFI guest are misusing the SDK.
///
/// # Safety
/// FFI-import stub; the wasm32 variant is `unsafe extern "C"`.
///
/// # Panics
/// Always panics — fail-fast per ADR-0063: the host build of the SDK
/// has no FFI host to call, so any invocation is a bug.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub unsafe fn prev_correlation() -> u64 {
    panic!("aether-actor: prev_correlation called outside the FFI guest");
}

/// Host-side stub for the FFI `aether::source_of` import (issue 1958).
/// Always panics — callers outside the FFI guest are misusing the SDK.
///
/// # Safety
/// FFI-import stub; the wasm32 variant is `unsafe extern "C"`.
///
/// # Panics
/// Always panics — fail-fast per ADR-0063: the host build of the SDK
/// has no FFI host to call, so any invocation is a bug.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub unsafe fn source_of(_handle: u32) -> u64 {
    panic!("aether-actor: source_of called outside the FFI guest");
}

/// Host-side stub for the FFI `aether::init_failed` import.
/// Always panics — callers outside the FFI guest are misusing the SDK.
///
/// # Safety
/// FFI-import stub; the wasm32 variant is `unsafe extern "C"`.
///
/// # Panics
/// Always panics — fail-fast per ADR-0063: the host build of the SDK
/// has no FFI host to call, so any invocation is a bug.
#[cfg(not(target_arch = "wasm32"))]
pub unsafe fn init_failed(_ptr: u32, _len: u32) {
    panic!("aether-actor: init_failed called outside the FFI guest");
}

/// Host-side stub for the FFI `aether::log_event` import.
/// Always panics — callers outside the FFI guest are misusing the SDK.
///
/// # Safety
/// FFI-import stub; the wasm32 variant is `unsafe extern "C"`.
///
/// # Panics
/// Always panics — fail-fast per ADR-0063: the host build of the SDK
/// has no FFI host to call, so any invocation is a bug.
#[cfg(not(target_arch = "wasm32"))]
pub unsafe fn log_event(
    _level: u32,
    _target_ptr: u32,
    _target_len: u32,
    _message_ptr: u32,
    _message_len: u32,
) {
    panic!("aether-actor: log_event called outside the FFI guest");
}

/// Host-side stub for the FFI `aether::spawn_sibling` import (ADR-0097).
/// Always panics — callers outside the FFI guest are misusing the SDK.
///
/// # Safety
/// FFI-import stub; the wasm32 variant is `unsafe extern "C"`.
///
/// # Panics
/// Always panics — fail-fast per ADR-0063: the host build of the SDK
/// has no FFI host to call, so any invocation is a bug.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub unsafe fn spawn_sibling(
    _tag: u64,
    _is_counter: u32,
    _subname_ptr: u32,
    _subname_len: u32,
    _config_ptr: u32,
    _config_len: u32,
) -> u64 {
    panic!("aether-actor: spawn_sibling called outside the FFI guest");
}

/// Host-side stub for the FFI `aether::spawn_inline_child` import
/// (ADR-0114). Always panics — callers outside the FFI guest are
/// misusing the SDK.
///
/// # Safety
/// FFI-import stub; the wasm32 variant is `unsafe extern "C"`.
///
/// # Panics
/// Always panics — fail-fast per ADR-0063: the host build of the SDK
/// has no FFI host to call, so any invocation is a bug.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub unsafe fn spawn_inline_child(_is_counter: u32, _subname_ptr: u32, _subname_len: u32) -> u64 {
    panic!("aether-actor: spawn_inline_child called outside the FFI guest");
}
