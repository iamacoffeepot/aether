// Raw FFI boundary with the substrate. This is the only place in a
// guest tree that should write `extern "C"` decls or host-stub panics;
// the module is private to `crate::wasm`, and everything else goes
// through the typed wrappers in `crate::wasm::bridge`.
//
// On the FFI guest target (today: `wasm32-unknown-unknown`) the fns
// are imports from the `aether` module the substrate's wasm runtime
// exposes (see `aether-substrate/src/actor/wasm/host_fns.rs`). On
// any other target, the imports a host build reaches have stubs that
// panic if called, which keeps the crate (and every actor crate that
// depends on it) compilable for `cargo test --workspace` on the host —
// actors can still be unit-tested for pure logic there, they just
// can't cross the FFI. An import whose every caller is wasm-gated has
// no host stub.
//
// The `target_family = "wasm"` cfg gate matches the only FFI host
// the substrate ships today. A future C / OS-process host would
// either pick a different gate or drop the cfg entirely; the
// import surface itself is target-agnostic.
//
// ADR-0024 Phase 1: the FFI-visible import names carry a `_p32`
// suffix in anticipation of a future `_p64` sibling for wasm64
// guests. The Rust-side identifiers stay un-suffixed (`send_mail`,
// not `send_mail_p32`) so callers in `bridge` don't have to thread
// the suffix through every call site — `#[link_name]` does the
// remap.

#[cfg(target_family = "wasm")]
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
    pub fn send_mail(recipient: u64, kind: u64, ptr: u32, len: u32, count: u32, detached: u32, from: u64) -> u32;
    /// `from` (issue 1987) is the replying actor's own folded `MailboxId`
    /// raw value — the dispatch identity the host stamps on the reply's
    /// lineage, validated and fallback-resolved exactly like `send_mail`.
    #[link_name = "reply_mail_p32"]
    pub fn reply_mail(sender: u32, kind: u64, ptr: u32, len: u32, count: u32, from: u64) -> u32;
    /// ADR-0243 §6: register the reply the dispatch in progress held on
    /// reply handle `sender`: `kind` and the `(ptr, len)` bytes of its
    /// `unanswered` value, copied out before the call returns. The host sends
    /// it to the requester if this instance closes before answering. `0` on
    /// success; the non-zero statuses are `reply_mail`'s: `3` out of bounds,
    /// `4` an unregistered kind, `5` engine-only mail, `6` a payload naming a
    /// blob this instance neither pins nor holds.
    #[link_name = "held_unanswered_p32"]
    pub fn held_unanswered(sender: u32, kind: u64, ptr: u32, len: u32) -> u32;
    #[link_name = "save_state_p32"]
    pub fn save_state(version: u32, ptr: u32, len: u32) -> u32;
    /// ADR-0042: return the correlation id the substrate minted for
    /// this component's most recent `send_mail`. `0` before any
    /// send. A handler filters its inbound reply on this so it picks
    /// "the reply I just sent a request for" rather than "any reply
    /// of this kind."
    #[link_name = "prev_correlation_p32"]
    pub fn prev_correlation() -> u64;
    /// Issue 2791: return the current inbound reply's echoed correlation id.
    /// Returns `0` when the dispatch is not a reply envelope.
    #[link_name = "reply_correlation_p32"]
    pub fn reply_correlation() -> u64;
    /// Issue 7627: read the engine's actor clock, in nanoseconds since its
    /// anchor. Within one engine a later read is never less than an earlier
    /// one. The same clock answers a native actor's `NativeCtx::now`.
    #[link_name = "now_nanos_p32"]
    pub fn now_nanos() -> u64;
    /// Issue 525 Phase 4b / issue 531: stage a `ActorInitError` message
    /// for the substrate to surface in `LoadResult::Err` after the
    /// guest's `init` returns non-zero. The `export!` macro is the
    /// only intended caller — user code returns `Err(ActorInitError)`
    /// from `WasmActor::init` and the macro plumbs the bytes
    /// through this import. Bytes at `(ptr, len)` are copied out of
    /// guest memory before the call returns.
    #[link_name = "init_failed_p32"]
    pub fn init_failed(ptr: u32, len: u32);
    /// ADR-0081 §7: re-emit one `tracing::*` event on the host side
    /// so the trampoline's `ActorAwareLayer` lands it in this guest's
    /// per-actor `ActorLogRing`. Called from `ForwardingSubscriber::event`
    /// (via the installed `crate::log::Sink`)
    /// per event. `level` follows the `0 = trace .. 4 = error`
    /// mapping the rest of `aether.log.*` uses. `target_ptr/len` and
    /// `message_ptr/len` are byte slices in guest memory; the host
    /// copies before returning.
    #[link_name = "log_event_p32"]
    pub fn log_event(level: u32, target_ptr: u32, target_len: u32, message_ptr: u32, message_len: u32);
    /// ADR-0114: register an inline child's alias route and return its
    /// `MailboxId`. The host folds the alias id `with_tag(Mailbox,
    /// fold_lineage(parent, instanced(<child NS>, subname)))` under the child
    /// type's own namespace (ADR-0241 §6)
    /// and synchronously registers an alias `MailboxEntry` routing to the
    /// component root's own dispatcher slot — the child is co-located
    /// in the parent's wasm instance, so there is no new trampoline and no
    /// config (the guest runs `init` in-process). `parent` is the executing
    /// actor's current mailbox and is validated host-side before it becomes
    /// the alias routing seed and rendered-name parent (issue 4490). `tag`
    /// is the child's actor-type tag, `ActorId::singleton(NAMESPACE)`, from
    /// which the host picks the namespace and contract rows the alias
    /// publishes (ADR-0231 §4); a tag the resident module does not declare
    /// allocates no alias. `is_counter` is `1` for `Subname::Counter` (the
    /// host appends a monotonic discriminator) or `0` for a caller-supplied
    /// name; `subname_ptr/len` is the bare `Named` segment (empty for
    /// `Counter`), copied out of guest memory before the call returns. The
    /// returned id is the ADR-0099 §3 lineage fold of `parent` with the
    /// child's node; `1` when the child's type declares a dependency with no
    /// `Live` route (ADR-0230), and `0` on any other host-side error (no
    /// memory, OOB, bad UTF-8, no spawner, an unresolvable parent, an
    /// undeclared tag, or a spent name).
    #[link_name = "spawn_inline_child_p32"]
    pub fn spawn_inline_child(parent: u64, tag: u64, is_counter: u32, subname_ptr: u32, subname_len: u32) -> u64;
    /// ADR-0114 teardown (#4228): retire the alias route
    /// [`spawn_inline_child`] registered, because the child it addressed was
    /// despawned. `alias` is that call's returned `MailboxId` raw value. The
    /// host validates it names an inline-child alias of the calling
    /// component's own cluster, then stages the retirement and the departure
    /// notices its watchers are owed; both land just after this returns,
    /// alongside the staged publications. Returns `1` when the retirement was
    /// staged, `0` when `alias` is not this component's (warn-logged
    /// host-side, nothing staged).
    #[link_name = "despawn_inline_child_p32"]
    pub fn despawn_inline_child(alias: u64) -> u32;
    /// ADR-0250: the component's asset catalog as a wire-encoded
    /// `Vec<AssetInfo>`, delivered into a guest buffer: the return is the
    /// packed `(ptr << 32) | len` of a live guest buffer the SDK decodes and
    /// frees. An empty catalog is a valid empty sequence.
    /// Backs `Assets::assets()`; readable for the instance's life.
    #[link_name = "asset_catalog_p32"]
    pub fn asset_catalog() -> u64;
    /// ADR-0250: take one asset from the instance's own module as a blob
    /// this instance holds by hash, with no payload byte entering guest
    /// memory. `(name_ptr, name_len)` is the asset name, copied out before
    /// the call returns. On success the host places the asset in this
    /// instance's blob table with one hold, writes its 32-byte hash at
    /// `hash_out_ptr`, and returns its length; `blob_drop` gives the hold
    /// back. A negative return, with nothing held or written, means "no such
    /// asset in the module" (the SDK maps it to `None`). The host traps on
    /// a `hash_out_ptr` outside guest memory.
    #[link_name = "asset_blob_p32"]
    pub fn asset_blob(name_ptr: u32, name_len: u32, hash_out_ptr: u32) -> i64;
    /// ADR-0230 §3 (#6786): prove the actor path at `(path_ptr, path_len)`, a
    /// UTF-8 slice in guest memory copied out before the call returns. The
    /// return is the packed `(ptr << 32) | len` of a live guest buffer
    /// holding the wire-encoded answer, a `__ResolvedPath`, which the SDK
    /// decodes and frees as [`asset_catalog`] does. The host traps on an
    /// out-of-bounds pointer, text that is not UTF-8, and text outside the
    /// ADR-0166 path grammar; the SDK passes only a validated `ErasedActorPath`.
    #[link_name = "resolve_path_p32"]
    pub fn resolve_path(path_ptr: u32, path_len: u32) -> u64;
    /// ADR-0230 §3 (#7205): the position of the `Live` route standing under
    /// exactly the canonical path at `(path_ptr, path_len)`, a UTF-8 slice in
    /// guest memory copied out before the call returns. The return is the
    /// packed `(ptr << 32) | len` of a live guest buffer holding the
    /// wire-encoded answer, a `__LiveRoute`, which the SDK decodes and frees
    /// as [`asset_catalog`] does. The host traps on an out-of-bounds
    /// pointer, text that is not UTF-8, and text outside the ADR-0166 path
    /// grammar; the SDK passes only a typed path's text.
    #[link_name = "live_route_p32"]
    pub fn live_route(path_ptr: u32, path_len: u32) -> u64;
    /// ADR-0231 §3 (#7501): the rows of the `Live` or `Dropped` route
    /// standing under exactly the canonical path at `(path_ptr, path_len)`, a
    /// UTF-8 slice in guest memory copied out before the call returns. The
    /// return is the packed `(ptr << 32) | len` of a live guest buffer
    /// holding the wire-encoded answer, a `__PublishedRows`, which the SDK
    /// decodes and frees as [`asset_catalog`] does. The host traps on an
    /// out-of-bounds pointer, text that is not UTF-8, and text outside the
    /// ADR-0166 path grammar; the SDK passes only a typed path's text. Its
    /// one caller is the wasm32-only guest decode context, so it has no host
    /// stub.
    #[link_name = "route_rows_p32"]
    pub fn route_rows(path_ptr: u32, path_len: u32) -> u64;
    /// ADR-0231 §4: the rows the route at `position` published while it is
    /// `Live`. The return is the packed `(ptr << 32) | len` of a live guest
    /// buffer holding the wire-encoded answer, a `__PublishedRows`, which the
    /// SDK decodes and frees as [`asset_catalog`] does. Any position is
    /// answered; one naming no `Live` route answers no rows.
    #[link_name = "published_rows_p32"]
    pub fn published_rows(position: u64) -> u64;
    /// ADR-0231 §11: the canonical path of the route record at `position`,
    /// the position of a reference the guest holds; the guest half of the
    /// native `NativeCtx::actor_path` read. The return is the packed
    /// `(ptr << 32) | len` of a live guest buffer holding the wire-encoded
    /// answer, a `__ActorPath`, which the SDK decodes and frees as
    /// [`asset_catalog`] does. Any position is answered; one holding no
    /// route record answers no path.
    #[link_name = "actor_path_p32"]
    pub fn actor_path(position: u64) -> u64;
    /// ADR-0079 §8: watch the actor at `target` for the calling actor `from`,
    /// through the watched type whose tag is `tag`, and return the
    /// watch's id. A standing watch of that watcher, target, and watched type
    /// answers its own id and nothing else changes; otherwise the id is new,
    /// drawn from this mailbox's send-correlation sequence, and never `0`.
    /// `from` is checked host-side like a send's; a claim outside this
    /// component's cluster watches as the component itself. The host traps on
    /// a `target` that holds no route record and is no alias of this
    /// component, which no SDK reference names.
    #[link_name = "watch_p32"]
    pub fn watch(target: u64, from: u64, tag: u64) -> u64;
    /// ADR-0079 §8: end the watch `watch` names. Returns `1` when a watch was
    /// there, and `0`, with nothing changed, for a number that names no watch
    /// of this mailbox.
    #[link_name = "unwatch_p32"]
    pub fn unwatch(watch: u64) -> u32;
    /// ADR-0079 §8: end the watch `watcher` holds on `target` through the
    /// watched type whose tag is `tag`, because `target`'s departure
    /// notice is being dispatched, and return its id. `0` when no such watch
    /// stands, or `watcher` is not this component or one of its aliases.
    #[link_name = "watch_ended_p32"]
    pub fn watch_ended(target: u64, watcher: u64, tag: u64) -> u64;
    /// ADR-0238 decisions 2 and 9: take one hold on the blob whose 32-byte
    /// hash sits at `hash_ptr`, resolved only against this instance's blob
    /// table, and return its length. Negative, with no hold taken, when the
    /// table neither pins nor holds the hash or the pointer is out of bounds.
    /// `blob_drop` gives the hold back.
    #[link_name = "blob_hold_p32"]
    pub fn blob_hold(hash_ptr: u32) -> i64;
    /// ADR-0238 decision 9: copy at most `dst_len` bytes (and at most
    /// `MAX_READ_BYTES`) of the blob whose hash sits at `hash_ptr`, from
    /// `offset`, into `(dst_ptr, dst_len)`, and return how many; `0` at or
    /// past the end. Negative, with nothing written, when the table neither
    /// pins nor holds the hash or either range is out of bounds.
    #[link_name = "blob_read_p32"]
    pub fn blob_read(hash_ptr: u32, offset: u64, dst_ptr: u32, dst_len: u32) -> i64;
    /// ADR-0238 decision 2: give back one hold on the blob whose hash sits
    /// at `hash_ptr`; `GuestHold::drop` is the one caller. An
    /// unheld hash or out-of-bounds pointer is warn-logged host-side and
    /// changes nothing.
    #[link_name = "blob_drop_p32"]
    pub fn blob_drop(hash_ptr: u32);
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
#[cfg(not(target_family = "wasm"))]
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
#[cfg(not(target_family = "wasm"))]
#[must_use]
pub unsafe fn reply_mail(_sender: u32, _kind: u64, _ptr: u32, _len: u32, _count: u32, _from: u64) -> u32 {
    panic!("aether-actor: reply_mail called outside the FFI guest");
}

/// Host-side stub for the FFI `aether::held_unanswered` import (ADR-0243
/// §6). Always panics — callers outside the FFI guest are misusing the SDK.
///
/// # Safety
/// FFI-import stub; the wasm32 variant is `unsafe extern "C"`.
///
/// # Panics
/// Always panics — fail-fast per ADR-0063: the host build of the SDK
/// has no FFI host to call, so any invocation is a bug.
#[cfg(not(target_family = "wasm"))]
#[must_use]
pub unsafe fn held_unanswered(_sender: u32, _kind: u64, _ptr: u32, _len: u32) -> u32 {
    panic!("aether-actor: held_unanswered called outside the FFI guest");
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
#[cfg(not(target_family = "wasm"))]
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
#[cfg(not(target_family = "wasm"))]
#[must_use]
pub unsafe fn prev_correlation() -> u64 {
    panic!("aether-actor: prev_correlation called outside the FFI guest");
}

/// Host-side stub for the FFI `aether::now_nanos` import.
/// Always panics — callers outside the FFI guest are misusing the SDK.
///
/// # Safety
/// FFI-import stub; the wasm32 variant is `unsafe extern "C"`.
///
/// # Panics
/// Always panics — fail-fast per ADR-0063: the host build of the SDK
/// has no FFI host to call, so any invocation is a bug.
#[cfg(not(target_family = "wasm"))]
#[must_use]
pub unsafe fn now_nanos() -> u64 {
    panic!("aether-actor: now_nanos called outside the FFI guest");
}

/// Host-side stub for the FFI `aether::reply_correlation` import.
/// Always panics — callers outside the FFI guest are misusing the SDK.
///
/// # Safety
/// FFI-import stub; the wasm32 variant is `unsafe extern "C"`.
///
/// # Panics
/// Always panics — fail-fast per ADR-0063: the host build of the SDK
/// has no FFI host to call, so any invocation is a bug.
#[cfg(not(target_family = "wasm"))]
#[must_use]
pub unsafe fn reply_correlation() -> u64 {
    panic!("aether-actor: reply_correlation called outside the FFI guest");
}

/// Host-side stub for the FFI `aether::spawn_inline_child` import
/// (ADR-0114, issue 4490). Always panics — callers outside the FFI guest are
/// misusing the SDK.
///
/// # Safety
/// FFI-import stub; the wasm32 variant is `unsafe extern "C"`.
///
/// # Panics
/// Always panics — fail-fast per ADR-0063: the host build of the SDK
/// has no FFI host to call, so any invocation is a bug.
#[cfg(not(target_family = "wasm"))]
#[must_use]
pub unsafe fn spawn_inline_child(
    _parent: u64,
    _tag: u64,
    _is_counter: u32,
    _subname_ptr: u32,
    _subname_len: u32,
) -> u64 {
    panic!("aether-actor: spawn_inline_child called outside the FFI guest");
}

/// Host-side stub for the FFI `aether::asset_catalog` import (ADR-0250).
/// Always panics — callers outside the FFI guest are misusing the SDK.
///
/// # Safety
/// FFI-import stub; the wasm32 variant is `unsafe extern "C"`.
///
/// # Panics
/// Always panics — fail-fast per ADR-0063: the host build of the SDK
/// has no FFI host to call, so any invocation is a bug.
#[cfg(not(target_family = "wasm"))]
#[must_use]
pub unsafe fn asset_catalog() -> u64 {
    panic!("aether-actor: asset_catalog called outside the FFI guest");
}

/// Host-side stub for the FFI `aether::resolve_path` import (ADR-0230 §3).
/// Always panics — callers outside the FFI guest are misusing the SDK.
///
/// # Safety
/// FFI-import stub; the wasm32 variant is `unsafe extern "C"`.
///
/// # Panics
/// Always panics — fail-fast per ADR-0063: the host build of the SDK
/// has no FFI host to call, so any invocation is a bug.
#[cfg(not(target_family = "wasm"))]
#[must_use]
pub unsafe fn resolve_path(_path_ptr: u32, _path_len: u32) -> u64 {
    panic!("aether-actor: resolve_path called outside the FFI guest");
}

/// Host-side stub for the FFI `aether::live_route` import (ADR-0230 §3).
/// Always panics — callers outside the FFI guest are misusing the SDK.
///
/// # Safety
/// FFI-import stub; the wasm32 variant is `unsafe extern "C"`.
///
/// # Panics
/// Always panics — fail-fast per ADR-0063: the host build of the SDK
/// has no FFI host to call, so any invocation is a bug.
#[cfg(not(target_family = "wasm"))]
#[must_use]
pub unsafe fn live_route(_path_ptr: u32, _path_len: u32) -> u64 {
    panic!("aether-actor: live_route called outside the FFI guest");
}

/// Host-side stub for the FFI `aether::published_rows` import (ADR-0231 §4).
/// Always panics — callers outside the FFI guest are misusing the SDK.
///
/// # Safety
/// FFI-import stub; the wasm32 variant is `unsafe extern "C"`.
///
/// # Panics
/// Always panics — fail-fast per ADR-0063: the host build of the SDK
/// has no FFI host to call, so any invocation is a bug.
#[cfg(not(target_family = "wasm"))]
#[must_use]
pub unsafe fn published_rows(_position: u64) -> u64 {
    panic!("aether-actor: published_rows called outside the FFI guest");
}

/// Host-side stub for the FFI `aether::actor_path` import (ADR-0231 §11).
/// Always panics — callers outside the FFI guest are misusing the SDK.
///
/// # Safety
/// FFI-import stub; the wasm32 variant is `unsafe extern "C"`.
///
/// # Panics
/// Always panics — fail-fast per ADR-0063: the host build of the SDK
/// has no FFI host to call, so any invocation is a bug.
#[cfg(not(target_family = "wasm"))]
#[must_use]
pub unsafe fn actor_path(_position: u64) -> u64 {
    panic!("aether-actor: actor_path called outside the FFI guest");
}

/// Host-side stub for the FFI `aether::watch` import (ADR-0079 §8).
///
/// # Safety
/// The signature mirrors the FFI import so callers can stay
/// target-agnostic; on the host there is no host fn to forward to.
///
/// # Panics
/// Always panics — fail-fast per ADR-0063: the host build of the SDK
/// has no FFI host to call, so any invocation is a bug.
#[cfg(not(target_family = "wasm"))]
#[must_use]
pub unsafe fn watch(_target: u64, _from: u64, _tag: u64) -> u64 {
    panic!("aether-actor: watch called outside the FFI guest");
}

/// Host-side stub for the FFI `aether::unwatch` import (ADR-0079 §8).
///
/// # Safety
/// The signature mirrors the FFI import so callers can stay
/// target-agnostic; on the host there is no host fn to forward to.
///
/// # Panics
/// Always panics — fail-fast per ADR-0063: the host build of the SDK
/// has no FFI host to call, so any invocation is a bug.
#[cfg(not(target_family = "wasm"))]
#[must_use]
pub unsafe fn unwatch(_watch: u64) -> u32 {
    panic!("aether-actor: unwatch called outside the FFI guest");
}

/// Host-side stub for the FFI `aether::watch_ended` import (ADR-0079 §8).
///
/// # Safety
/// The signature mirrors the FFI import so callers can stay
/// target-agnostic; on the host there is no host fn to forward to.
///
/// # Panics
/// Always panics — fail-fast per ADR-0063: the host build of the SDK
/// has no FFI host to call, so any invocation is a bug.
#[cfg(not(target_family = "wasm"))]
#[must_use]
pub unsafe fn watch_ended(_target: u64, _watcher: u64, _tag: u64) -> u64 {
    panic!("aether-actor: watch_ended called outside the FFI guest");
}
