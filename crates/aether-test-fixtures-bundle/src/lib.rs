//! The main test-fixture bundle: the bulk of the workspace's wasm
//! fixtures consolidated into one ADR-0096 multi-actor module. One
//! `src/<name>.rs` module per former fixture; a single
//! `export!(default = Probe, …)` packs all of them into one cdylib, with
//! `Probe` the opted-in default (ADR-0138) so a bare `load` of
//! `aether_test_fixtures_bundle.wasm` instantiates it. The integration
//! tests load this one wasm and select an in-bundle actor with
//! `export: Some("<NAMESPACE>")`.
//!
//! The `InlineChild` / `InlineDespawnChild` inline children ride in
//! `inline_child` as types but are absent from the `export!` list — an
//! inline child is constructed in-process by its parent, never
//! instantiated by the host. The typed↔reshaped replace pair is *not*
//! here: a cross-module `replace_component` needs two distinct binaries,
//! so each lives in its own satellite crate.

mod cube;
mod editor_region_probe;
mod fs_demux;
mod http_handler;
mod inline_child;
mod mat4_source;
mod matrix_sweep;
mod multi_actor;
mod peer_routing;
mod probe;
mod source_observer;
mod stateful_replace;
mod tcp_load_probe;
mod ui_widget;

pub use cube::Cube;
pub use editor_region_probe::EditorRegionProbe;
pub use fs_demux::FsDemux;
pub use http_handler::{
    HttpHandler, RoutedHttpHandler, RoutedStreamingHttpHandler, StreamingHttpHandler, WebSocketHandler,
};
pub use inline_child::{
    InlineConfiguredChild, InlineConfiguredParent, InlineDespawnParent, InlineParent, InlineStatefulChild,
    InlineStatefulParent, InlineTagParent, NestedDetachedLeaf, NestedLineageChild, NestedLineageLeaf,
    NestedLineageParent,
};
pub use mat4_source::MatSource;
pub use matrix_sweep::{MatrixChild, MatrixParent};
pub use multi_actor::{Panel, RootManager};
pub use peer_routing::{ParentPeerCaller, ParentPeerTarget};
pub use probe::{Probe, ProbeWithConfig};
pub use source_observer::SourceObserver;
pub use stateful_replace::{Counter, Sidecar};
pub use tcp_load_probe::TcpLoadProbe;
pub use ui_widget::UiWidget;

// ADR-0138: `default = Probe` opts this multi-actor module into `Probe` as
// its bare-load default, so a `load` with no `export` selector instantiates
// it — the default-load contract the probe scenarios rely on. The remaining
// actors are reachable by their `NAMESPACE` export selector.
aether_actor::export!(
    default = Probe,
    ProbeWithConfig,
    RootManager,
    Panel,
    ParentPeerCaller,
    ParentPeerTarget,
    Cube,
    EditorRegionProbe,
    FsDemux,
    MatSource,
    UiWidget,
    HttpHandler,
    StreamingHttpHandler,
    RoutedHttpHandler,
    RoutedStreamingHttpHandler,
    WebSocketHandler,
    SourceObserver,
    MatrixParent,
    MatrixChild,
    InlineParent,
    InlineStatefulParent,
    InlineStatefulChild,
    InlineDespawnParent,
    InlineConfiguredParent,
    InlineConfiguredChild,
    NestedLineageParent,
    NestedLineageChild,
    NestedLineageLeaf,
    NestedDetachedLeaf,
    InlineTagParent,
    Counter,
    Sidecar,
    TcpLoadProbe,
);

// ADR-0163 §2: embed a small asset in the `aether.asset.asset_fixture.txt`
// custom section of this bundle's wasm. This is the fixture the aether-actor
// `asset_sections` integration test parses out of the built
// `aether_test_fixtures_bundle.wasm` to pin `export_asset!`'s emission (the
// section is present exactly once and byte-exact against the source file).
// It rides this bundle rather than an aether-actor example because `cargo xtask
// dist` cross-builds this crate's wasm (it deps `aether-actor` + is a cdylib),
// whereas aether-actor's own examples are never discovered — the crate cannot
// depend on itself, so they fail the actor-dep gate and no CI path emits them.
aether_actor::export_asset!("asset_fixture.txt");
