//! Wasm-flavoured actor primitives: the [`Component`] trait and per-mail
//! [`ComponentCtx`] (the `aether-actor::WasmActor` counterpart that the
//! substrate's wasmtime trampoline drives), the linker registration for
//! the host fns the guest imports, and the wasm-only support tables
//! ([`reply_table`] for in-flight `wait_reply` correlations,
//! [`kind_manifest`] for parsing the `aether.kinds` custom section).

pub mod component;
pub mod host_fns;
pub mod kind_manifest;
pub mod reply_table;

pub use component::{Component, ComponentCtx};
