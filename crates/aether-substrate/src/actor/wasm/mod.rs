//! THE wasm runtime — substrate's host-side implementation of the
//! `_p32` FFI contract that `aether_actor::wasm` defines. Owns the
//! wasmtime engine, the host-fn linker registration, the per-instance
//! `reply_table` for in-flight reply correlations, and the
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
//!   `aether_actor::WasmActor`. The wasmtime trampoline drives them
//!   per inbound mail.
//! - [`host_fns`] — `extern "C"` import linker registration matching
//!   the `aether` wasm import-module names the guest SDK's private `raw`
//!   declarations expect.
//! - `reply_table` — wasm-only reply correlation table (crate-private).
//! - `blob_table` — one instance's held blob-store entries, keyed by hash,
//!   that the `blob_*_p32` host fns resolve against (crate-private,
//!   ADR-0238).
//! - `watch_table` — one instance's watches on other actors and the
//!   registrations behind them, which the `watch_p32` / `unwatch_p32` /
//!   `watch_ended_p32` host fns read and write (crate-private, ADR-0079 §8).
//! - [`kind_manifest`] — parses the `aether.kinds` custom section the
//!   guest's [`aether_actor::export!`] macro emits.
//! - [`module`] — code as a value (ADR-0241 §2): the engine's one
//!   [`module::ModuleCache`] compiles a wasm blob and parses its sections
//!   once per content hash into a [`module::Module`], whose assets are blobs.
//!
//! The `WasmTrampoline` actor itself lives in
//! `aether_component::trampoline` (issue 654) — next to the
//! `ComponentHostCapability` that spawns it. A guest is born under its own
//! published namespace (ADR-0241 §5, §6), never the trampoline's. The
//! substrate still owns the
//! spawn primitives, the `Component`/`ComponentCtx` types, and the
//! host-fn linker; only the actor wrapper moved.

pub mod asset_manifest;
pub(crate) mod blob_table;
pub mod component;
pub mod host_fns;
pub mod kind_manifest;
pub mod module;
pub(crate) mod reply_table;
pub(crate) mod watch_table;

pub use component::{Component, ComponentCtx};
