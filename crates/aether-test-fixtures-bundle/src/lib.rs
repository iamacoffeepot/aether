//! The main test-fixture bundle: the bulk of the workspace's wasm
//! fixtures consolidated into one ADR-0096 multi-actor module. One
//! `src/<name>.rs` module per former fixture; a single
//! `export!(public = […], private = […])` packs all of them into one
//! cdylib, so a bare `load` of `aether_test_fixtures_bundle.wasm` is refused
//! naming its exports (ADR-0241 §9). The integration tests load this one wasm
//! and select an in-bundle actor with `export: Some("<NAMESPACE>")`.
//!
//! The `InlineChild` / `InlineDespawnChild` inline children ride in
//! `inline_child`, and `InlineContextAsker` in `inline_context`, under the
//! `export!` call's `private = [..]` key: each
//! parent constructs its child in-process and a replace rebuilds it, but the
//! host never instantiates it by selector (issue 6136). The typed↔reshaped
//! replace pair is *not* here: a cross-module `replace_component` needs two
//! distinct binaries, so each lives in its own satellite crate. Neither is
//! the fs-demux pair: its private child declares dependencies the host checks
//! at every load of its module, so it lives in `aether-test-fixtures-fs-demux`.

mod contract_replace;
mod correlation_carry;
mod cube;
mod dependent_probe;
mod editor_region_probe;
mod held_carry;
mod http_handler;
mod inline_child;
mod inline_context;
mod mat4_source;
mod matrix_sweep;
mod multi_actor;
mod paint_probe;
mod peer_routing;
mod probe;
mod quiet_probe;
mod source_forwarder;
mod source_observer;
mod stateful_replace;
mod tcp_load_probe;
mod ui_widget;

pub use contract_replace::{ContractBase, ContractChanged, ContractDropped, ContractExtended, ContractFallback};
pub use correlation_carry::{CarryRequester, ReplyHolder};
pub use cube::Cube;
pub use dependent_probe::DependentProbe;
pub use editor_region_probe::EditorRegionProbe;
pub use held_carry::{HeldForgetter, HeldKeeper, HeldRelay, HeldRequester};
pub use http_handler::{
    HttpHandler, RoutedHttpHandler, RoutedStreamingHttpHandler, StreamingHttpHandler, WebSocketHandler,
};
pub use inline_child::{
    InlineChild, InlineConfiguredChild, InlineConfiguredParent, InlineDespawnChild, InlineDespawnParent, InlineParent,
    InlineStatefulChild, InlineStatefulParent, InlineTagParent, NestedLineageChild, NestedLineageLeaf,
    NestedLineageParent,
};
pub use inline_context::{InlineContextAsker, InlineContextHost};
pub use mat4_source::MatSource;
pub use matrix_sweep::{MatrixChild, MatrixParent};
pub use multi_actor::{Panel, RootManager};
pub use paint_probe::PaintProbe;
pub use peer_routing::{ParentPeerCaller, ParentPeerStandIn, ParentPeerTarget};
pub use probe::{KeyProbe, Probe, ProbeWithConfig};
pub use quiet_probe::QuietProbe;
pub use source_forwarder::SourceForwarder;
pub use source_observer::SourceObserver;
pub use stateful_replace::{Counter, RehydrateTrap, Sidecar};
pub use tcp_load_probe::TcpLoadProbe;
pub use ui_widget::UiWidget;

// Every actor is reachable by its `NAMESPACE` export selector; a `load` with
// no selector is refused naming them (ADR-0241 §9).
aether_actor::export!(
    public = [
        Probe,
        ProbeWithConfig,
        KeyProbe,
        PaintProbe,
        QuietProbe,
        RootManager,
        Panel,
        ParentPeerCaller,
        ParentPeerTarget,
        ParentPeerStandIn,
        Cube,
        EditorRegionProbe,
        MatSource,
        UiWidget,
        HttpHandler,
        StreamingHttpHandler,
        RoutedHttpHandler,
        RoutedStreamingHttpHandler,
        WebSocketHandler,
        SourceObserver,
        SourceForwarder,
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
        InlineTagParent,
        InlineContextHost,
        Counter,
        Sidecar,
        RehydrateTrap,
        TcpLoadProbe,
        DependentProbe,
        CarryRequester,
        ReplyHolder,
        HeldRequester,
        HeldRelay,
        HeldKeeper,
        HeldForgetter,
        ContractBase,
        ContractDropped,
        ContractChanged,
        ContractExtended,
        ContractFallback,
    ],
    private = [InlineChild, InlineDespawnChild, InlineContextAsker],
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
