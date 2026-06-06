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
    #[link_name = "send_mail_p32"]
    pub fn send_mail(recipient: u64, kind: u64, ptr: u32, len: u32, count: u32) -> u32;
    #[link_name = "reply_mail_p32"]
    pub fn reply_mail(sender: u32, kind: u64, ptr: u32, len: u32, count: u32) -> u32;
    #[link_name = "save_state_p32"]
    pub fn save_state(version: u32, ptr: u32, len: u32) -> u32;
    /// Issue 1363: spawn a sibling component instance from inside a
    /// handler — the wasm-side counterpart of `NativeCtx::spawn_child`.
    /// `subname_ptr/len` is the caller-chosen instance segment
    /// (`subname_len == 0` ⇒ a host-allocated counter); `config_ptr/len`
    /// is the wire-encoded init-config payload (the same byte-carrier as
    /// `LoadComponent.config`, empty for a `Config = ()` child). Both
    /// slices are copied out of guest memory before the call returns.
    /// Returns the child's non-zero `MailboxId` on success, or `0` on
    /// failure (the host logs the reason).
    #[link_name = "spawn_child_p32"]
    pub fn spawn_child(
        subname_ptr: u32,
        subname_len: u32,
        config_ptr: u32,
        config_len: u32,
    ) -> u64;
    /// ADR-0042: return the correlation id the substrate minted for
    /// this component's most recent `send_mail`. `0` before any
    /// send. A handler filters its inbound reply on this so it picks
    /// "the reply I just sent a request for" rather than "any reply
    /// of this kind."
    #[link_name = "prev_correlation_p32"]
    pub fn prev_correlation() -> u64;
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
pub unsafe fn send_mail(_recipient: u64, _kind: u64, _ptr: u32, _len: u32, _count: u32) -> u32 {
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
pub unsafe fn reply_mail(_sender: u32, _kind: u64, _ptr: u32, _len: u32, _count: u32) -> u32 {
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

/// Host-side stub for the FFI `aether::spawn_child` import. Always
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
pub unsafe fn spawn_child(
    _subname_ptr: u32,
    _subname_len: u32,
    _config_ptr: u32,
    _config_len: u32,
) -> u64 {
    panic!("aether-actor: spawn_child called outside the FFI guest");
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
