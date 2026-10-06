//! Watching another actor (ADR-0079 §8): the guest half of the `watch_p32` /
//! `unwatch_p32` / `watch_ended_p32` host fns.
//!
//! The host keeps a guest's watches in a table on its instance: one
//! registration per watcher and target, and one row per watched type with the
//! id it returned. These calls pass positions and a watched type's tag and
//! get ids back; the context a watch carries never crosses, since the SDK
//! stores it under the id. They are the transport under `WasmCtx::watch`,
//! `WasmCtx::unwatch`, and the departure arm `#[actor]` emits.

use core::num::NonZeroU64;

use aether_data::{__watch_id_from_host, __watch_id_number, WatchId};

use crate::wasm::raw;

/// Watch `target` for the actor at `from` through the watched type whose tag
/// is `tag`, and return the watch's id: the standing one for that triple, or
/// a new one.
pub fn watch(target: u64, from: u64, tag: u64) -> WatchId {
    // SAFETY: FFI import over plain integers; the host answers an id or
    // traps.
    __watch_id_from_host(unsafe { raw::watch(target, from, tag) })
}

/// End `watch`, answering whether the host held a watch under that id.
pub fn unwatch(watch: WatchId) -> bool {
    // SAFETY: FFI import over a plain integer.
    let ended = unsafe { raw::unwatch(__watch_id_number(watch)) };
    ended != 0
}

/// End the watch `watcher` holds on the departed `target` through the watched
/// type whose tag is `tag`, and return its id, or `None` when no such watch
/// stands.
pub fn watch_ended(target: u64, watcher: u64, tag: u64) -> Option<WatchId> {
    // SAFETY: FFI import over plain integers.
    let ended = unsafe { raw::watch_ended(target, watcher, tag) };
    NonZeroU64::new(ended).map(|id| __watch_id_from_host(id.get()))
}
