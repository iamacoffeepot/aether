//! The main test-fixture bundle: the bulk of the workspace's wasm
//! fixtures consolidated into one ADR-0096 multi-actor module. One
//! `src/<name>.rs` module per former fixture; a single
//! `export!(Probe, …)` packs all of them into one cdylib, with `Probe`
//! the entry so a bare `load` of `aether_test_fixtures_bundle.wasm`
//! instantiates it. The integration tests load this one wasm and select
//! an in-bundle actor with `export: Some("<NAMESPACE>")`.
//!
//! The `InlineChild` / `InlineDespawnChild` inline children ride in
//! `inline_child` as types but are absent from the `export!` list — an
//! inline child is constructed in-process by its parent, never
//! instantiated by the host. The typed↔reshaped replace pair is *not*
//! here: a cross-module `replace_component` needs two distinct binaries,
//! so each lives in its own satellite crate.

mod cube;
mod http_handler;
mod inline_child;
mod mat4_source;
mod matrix_sweep;
mod multi_actor;
mod probe;
mod source_observer;
mod stateful_replace;
mod ui_widget;

pub use cube::Cube;
pub use http_handler::HttpHandler;
pub use inline_child::{
    InlineDespawnParent, InlineParent, InlineStatefulChild, InlineStatefulParent,
};
pub use mat4_source::MatSource;
pub use matrix_sweep::{MatrixChild, MatrixParent};
pub use multi_actor::{Panel, RootManager};
pub use probe::{Probe, ProbeWithConfig};
pub use source_observer::SourceObserver;
pub use stateful_replace::{Counter, Sidecar};
pub use ui_widget::UiWidget;

// `Probe` is listed first so a bare `load` (no `export` selector)
// instantiates it, preserving the entry-load contract the probe scenarios
// rely on. The remaining actors are reachable by their `NAMESPACE`
// export selector.
aether_actor::export!(
    Probe,
    ProbeWithConfig,
    RootManager,
    Panel,
    Cube,
    MatSource,
    UiWidget,
    HttpHandler,
    SourceObserver,
    MatrixParent,
    MatrixChild,
    InlineParent,
    InlineStatefulParent,
    InlineStatefulChild,
    InlineDespawnParent,
    Counter,
    Sidecar,
);
