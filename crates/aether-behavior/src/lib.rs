//! aether-behavior: the script-side SDK for behavior scripts (ADR-0137).
//!
//! A behavior script is small wasm injected at a position in a running
//! actor cluster that transforms the mail already flowing there. This
//! crate is the surface a script compiles against, mirroring the
//! `aether-actor` / `aether-actor-derive` pairing one tier over. It
//! depends on `aether-data` and `serde` only — never `aether-actor`,
//! never `wasmi` — the crate-boundary invariant the whole tier split
//! rests on (a `cdylib` with an `aether-actor` dependency *is* a
//! component to structural discovery, so behavior wasm must not name it).
//!
//! ## Two faces, one crate
//!
//! - **Trunk** (always compiled, visible under `default-features = false`):
//!   the host<->script [`envelope`] (`FilterOutput` / `Verdict` / `Effect`),
//!   the packed-pointer [`abi`], the exports-[`manifest`] decoder, and the
//!   reserved lifecycle [`sentinel`]s. A later issue's script host consumes
//!   exactly this trunk to drain a script's output without pulling the guest
//!   surface.
//! - **Runtime** (the `runtime` feature, on by default): the authoring SDK —
//!   [`BehaviorCtx`], the widget/child/panel handles, the decode-once mirror,
//!   the effect accumulator, and the [`Behavior`] lifecycle trait. It compiles
//!   FFI-free on native so host-side `cargo test` exercises the ctx / mirror /
//!   drain logic, and the `#[behavior]` macro's guest exports (`alloc` /
//!   `filter` / `state_save` / `state_load`) are emitted only on `wasm`.
//!
//! Authoring lives in the sibling `aether-behavior-derive` crate, re-exported
//! here so a script depends on `aether-behavior` alone.

#![no_std]

extern crate alloc;

pub mod abi;
pub mod envelope;
pub mod manifest;
pub mod sentinel;

#[cfg(feature = "runtime")]
pub mod runtime;

/// The behavior authoring macros, re-exported on the runtime face so guest
/// crates depend on `aether-behavior` alone.
#[cfg(feature = "runtime")]
pub use aether_behavior_derive::{behavior, on, on_attach, on_detach, on_frame};
#[cfg(feature = "runtime")]
pub use runtime::{Behavior, BehaviorCtx, ChildHandle, PanelHandle, WidgetHandle};

/// The host face (ADR-0137, issue 2687), behind the non-default `host`
/// feature: [`BehaviorHost`](host::BehaviorHost), the wasm actor that embeds a
/// `wasmi` interpreter and interposes at a tree slot. Turns on the optional
/// `aether-actor` / `wasmi` deps the default face names neither of, so a
/// script build never links them.
#[cfg(feature = "host")]
pub mod host;

#[cfg(feature = "host")]
pub use host::{BehaviorHost, HostConfig};

/// Items the `#[behavior]` macro's generated code names by absolute path,
/// re-exported so a script depends on `aether-behavior` alone (never a
/// direct `aether-data` path) for the surface the codegen touches.
#[doc(hidden)]
pub mod __macro_internals {
    pub use aether_data::{Kind, KindId};
    pub use alloc::boxed::Box;
    pub use alloc::vec::Vec;

    pub use crate::abi::pack_ptr_len;
    pub use crate::manifest::EXPORTS_MANIFEST_VERSION;

    #[cfg(feature = "runtime")]
    pub use crate::runtime::{
        Behavior, BehaviorCtx, MirrorStore, Slot, run_filter, state_load_serde, state_save_serde,
    };
    #[cfg(all(feature = "runtime", target_family = "wasm"))]
    pub use crate::runtime::{leak_packed, read_guest_slice, realloc_bytes};
}
