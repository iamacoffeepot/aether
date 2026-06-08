//! THE wasm runtime — substrate's host-side implementation of the
//! `_p32` FFI contract that `aether_actor::ffi` defines. Owns the
//! wasmtime engine, the host-fn linker registration, the per-instance
//! [`reply_table`] for in-flight reply correlations, and the
//! [`kind_manifest`] reader that parses `aether.kinds` / `aether.namespace`
//! custom sections at load time.
//!
//! This is one consumer of the FFI-actor contract — the wasm host. A
//! future C / OS-process host would live as a sibling under
//! `aether-substrate::actor::*` with the same shape: a trampoline
//! (substrate-side dispatcher), a [`Component`]-equivalent trait, and
//! a per-mail context. The actor crate stays target-agnostic; this
//! module owns the wasm-specific machinery.
//!
//! - [`Component`] / [`ComponentCtx`] — substrate-side counterpart of
//!   `aether_actor::FfiActor`. The wasmtime trampoline drives them
//!   per inbound mail.
//! - [`host_fns`] — `extern "C"` import linker registration matching
//!   the names guest [`aether_actor::ffi::raw`] expects.
//! - [`reply_table`] — wasm-only reply correlation table.
//! - [`kind_manifest`] — parses the `aether.kinds` custom section the
//!   guest's [`aether_actor::export!`] macro emits.
//!
//! The `WasmTrampoline` actor itself lives in
//! `aether_capabilities::trampoline` (issue 654) — next to the
//! `ComponentHostCapability` that spawns it, so the trampoline's
//! `Actor::NAMESPACE` is the single cap-owned declaration of the
//! `aether.embedded` prefix. The substrate still owns the
//! spawn primitives, the `Component`/`ComponentCtx` types, and the
//! host-fn linker; only the actor wrapper moved.

pub mod component;
pub mod host_fns;
pub mod kind_manifest;
pub mod reply_table;

pub use component::{Component, ComponentCtx};
