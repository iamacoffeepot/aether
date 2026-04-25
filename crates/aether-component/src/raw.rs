// Raw FFI boundary with the substrate. This is the only place in a
// guest component tree that should write `extern "C"` decls or host-stub
// panics; everything else goes through the typed wrappers in `lib.rs`.
//
// On wasm32 the fns are imports from the `aether` module the substrate
// exposes (see `aether-substrate/src/host_fns.rs`). On any other target
// they're stubs that panic if called, which keeps the crate (and every
// component that depends on it) compilable for `cargo test --workspace`
// on the host — components can still be unit-tested for pure logic
// there, they just can't cross the FFI.
//
// ADR-0024 Phase 1: the wasm-visible import names carry a `_p32`
// suffix in anticipation of a future `_p64` sibling for wasm64
// components. The Rust-side identifiers stay un-suffixed (`send_mail`,
// not `send_mail_p32`) so callers in `lib.rs` don't have to thread the
// suffix through every callsite — `#[link_name]` does the remap.

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "aether")]
unsafe extern "C" {
    #[link_name = "send_mail_p32"]
    pub fn send_mail(recipient: u64, kind: u64, ptr: u32, len: u32, count: u32) -> u32;
    #[link_name = "reply_mail_p32"]
    pub fn reply_mail(sender: u32, kind: u64, ptr: u32, len: u32, count: u32) -> u32;
    #[link_name = "save_state_p32"]
    pub fn save_state(version: u32, ptr: u32, len: u32) -> u32;
    /// ADR-0042: block the component thread until a mail whose kind
    /// id matches `expected_kind` (and, when `expected_correlation`
    /// != 0, whose `ReplyTo.correlation_id` also matches) arrives,
    /// then copy up to `out_cap` bytes of its payload to `out_ptr`.
    /// Return codes: `>= 0` = bytes written, `-1` = timeout, `-2` =
    /// payload larger than `out_cap` (mail re-parked for retry),
    /// `-3` = substrate tore the component down mid-wait.
    /// `timeout_ms` is clamped to 30s substrate-side.
    #[link_name = "wait_reply_p32"]
    pub fn wait_reply(
        expected_kind: u64,
        out_ptr: u32,
        out_cap: u32,
        timeout_ms: u32,
        expected_correlation: u64,
    ) -> i32;
    /// ADR-0042: return the correlation id the substrate minted for
    /// this component's most recent `send_mail`. `0` before any
    /// send. Paired with `wait_reply` so sync wrappers can filter on
    /// "the reply I just sent a request for" rather than "any reply
    /// of this kind."
    #[link_name = "prev_correlation_p32"]
    pub fn prev_correlation() -> u64;
    /// ADR-0045: copy `len` bytes at `ptr` (in the guest's linear
    /// memory) into the substrate's handle store under `kind_id` and
    /// return a fresh ephemeral handle id. Returns `0` on failure
    /// (out-of-bounds pointer, no store wired, eviction-failed).
    /// The publishing component holds an initial refcount on the
    /// returned handle — call `handle_release` to drop it.
    #[link_name = "handle_publish_p32"]
    pub fn handle_publish(kind_id: u64, ptr: u32, len: u32) -> u64;
    /// ADR-0045: drop one reference on `id`. Returns `0` on success,
    /// non-zero status codes for unknown handle (`1`) or no store
    /// wired (`2`). `dec_ref` saturates at zero so calling release
    /// on an already-released handle is a no-op success.
    #[link_name = "handle_release_p32"]
    pub fn handle_release(id: u64) -> u32;
    /// ADR-0045: pin `id` against LRU eviction. Pinned entries stay
    /// in the store regardless of refcount. Same return codes as
    /// `handle_release`.
    #[link_name = "handle_pin_p32"]
    pub fn handle_pin(id: u64) -> u32;
    /// ADR-0045: clear the pinned flag on `id`. Same return codes
    /// as `handle_release`.
    #[link_name = "handle_unpin_p32"]
    pub fn handle_unpin(id: u64) -> u32;
}

/// # Safety
/// Host-target stub for the wasm `aether::send_mail` import. Always
/// panics — callers on non-wasm targets are misusing the SDK.
#[cfg(not(target_arch = "wasm32"))]
pub unsafe fn send_mail(_recipient: u64, _kind: u64, _ptr: u32, _len: u32, _count: u32) -> u32 {
    panic!("aether-component: send_mail called on non-wasm target");
}

/// # Safety
/// Host-target stub for the wasm `aether::reply_mail` import. Always
/// panics — callers on non-wasm targets are misusing the SDK.
#[cfg(not(target_arch = "wasm32"))]
pub unsafe fn reply_mail(_sender: u32, _kind: u64, _ptr: u32, _len: u32, _count: u32) -> u32 {
    panic!("aether-component: reply_mail called on non-wasm target");
}

/// # Safety
/// Host-target stub for the wasm `aether::save_state` import. Always
/// panics — callers on non-wasm targets are misusing the SDK.
#[cfg(not(target_arch = "wasm32"))]
pub unsafe fn save_state(_version: u32, _ptr: u32, _len: u32) -> u32 {
    panic!("aether-component: save_state called on non-wasm target");
}

/// # Safety
/// Host-target stub for the wasm `aether::wait_reply` import. Always
/// panics — callers on non-wasm targets are misusing the SDK.
#[cfg(not(target_arch = "wasm32"))]
pub unsafe fn wait_reply(
    _expected_kind: u64,
    _out_ptr: u32,
    _out_cap: u32,
    _timeout_ms: u32,
    _expected_correlation: u64,
) -> i32 {
    panic!("aether-component: wait_reply called on non-wasm target");
}

/// # Safety
/// Host-target stub for the wasm `aether::prev_correlation` import.
/// Always panics — callers on non-wasm targets are misusing the SDK.
#[cfg(not(target_arch = "wasm32"))]
pub unsafe fn prev_correlation() -> u64 {
    panic!("aether-component: prev_correlation called on non-wasm target");
}

/// # Safety
/// Host-target stub for the wasm `aether::handle_publish` import.
/// Always panics — callers on non-wasm targets are misusing the SDK.
#[cfg(not(target_arch = "wasm32"))]
pub unsafe fn handle_publish(_kind_id: u64, _ptr: u32, _len: u32) -> u64 {
    panic!("aether-component: handle_publish called on non-wasm target");
}

/// # Safety
/// Host-target stub for the wasm `aether::handle_release` import.
/// Always panics — callers on non-wasm targets are misusing the SDK.
#[cfg(not(target_arch = "wasm32"))]
pub unsafe fn handle_release(_id: u64) -> u32 {
    panic!("aether-component: handle_release called on non-wasm target");
}

/// # Safety
/// Host-target stub for the wasm `aether::handle_pin` import.
/// Always panics — callers on non-wasm targets are misusing the SDK.
#[cfg(not(target_arch = "wasm32"))]
pub unsafe fn handle_pin(_id: u64) -> u32 {
    panic!("aether-component: handle_pin called on non-wasm target");
}

/// # Safety
/// Host-target stub for the wasm `aether::handle_unpin` import.
/// Always panics — callers on non-wasm targets are misusing the SDK.
#[cfg(not(target_arch = "wasm32"))]
pub unsafe fn handle_unpin(_id: u64) -> u32 {
    panic!("aether-component: handle_unpin called on non-wasm target");
}
