//! The gameplay-systems runtime: the reusable actors `aether-kit`
//! packs into one cdylib (ADR-0096 multi-actor module).
//!
//! - [`Locomotion`] — tile-grid movement on a fixed-point ground plane;
//!   the module **entry**, so a bare `load` of `aether_kit.wasm`
//!   instantiates it.
//! - [`camera::CameraComponent`] — the multi-camera driver, selected by
//!   the `aether_kit@aether.camera` export. Its `aether.camera.*` driver kinds
//!   live in [`crate::camera`].
//! - [`mesh_viewer::MeshViewer`] — loads a `.dsl` / `.obj` mesh file
//!   and replays it to the render sink each tick, selected by the
//!   `aether_kit@aether.mesh_viewer` export. Its `aether.mesh.load`
//!   kind lives in [`crate::mesh`].
//! - [`world_view::WorldView`] — meshes the chunked world plane stack
//!   ([`crate::world`]) into keyed-quilt ground cells and corner-minimized
//!   overlay contours and replays them to the render sink each frame,
//!   selected by the `aether_kit@aether.world` export. Its
//!   `aether.kit.world.*` kinds live in [`crate::world`]; the pure mesher
//!   it drives lives in [`mesher`].
//! - [`widget::Widget`] — the widget-compositing node (ADR-0117),
//!   selected by the `aether_kit@aether.kit.widget` export. A cluster of
//!   these draws local and composites up so the whole subtree is one
//!   render sender in structural draw order; its wire kinds live in
//!   [`crate::widgets`] and its compositing bookkeeping in [`composite`].
//! - The **widget set** (issue 2660): the five concrete widgets in
//!   [`widgets`] ([`widgets::SliderWidget`], [`widgets::TextFieldWidget`],
//!   [`widgets::RadioGroupWidget`], [`widgets::ButtonWidget`],
//!   [`widgets::LabelWidget`]) spawned as inline children of the
//!   reference [`widget_panel::WidgetPanel`] (export
//!   `aether_kit@aether.kit.widget.panel`), which drives them through the
//!   [`composite::Composite`] draw protocol and the [`focus::Focus`]
//!   root-owned input model.
//!
//! `export!(Locomotion, …)` lists the entry first; the macro emits the wasm32
//! FFI shims and the `aether.kinds` custom section for every listed actor.

pub mod camera;
pub mod composite;
pub mod focus;
pub mod locomotion;
pub mod mesh_viewer;
pub mod mesher;
pub mod widget;
pub mod widget_panel;
pub mod widgets;
pub mod world_view;

pub use camera::CameraComponent;
pub use locomotion::Locomotion;
pub use mesh_viewer::MeshViewer;
pub use mesher::mesh_chunk;
pub use mesher::style::StyleTable;
pub use widget::Widget;
pub use widget_panel::WidgetPanel;
pub use widgets::{ButtonWidget, LabelWidget, RadioGroupWidget, SliderWidget, TextFieldWidget};
pub use world_view::WorldView;

// `arena` (the hazard-field builder) keys its fixed-size scratch on the
// locomotion grid dimensions; keep them reachable at the `runtime`
// module root where `arena` imports them.
pub(crate) use locomotion::{GRID_H, GRID_W};

// A cdylib carries one `export!` (the shared init/receive FFI entry). The
// `behavior` feature (ADR-0137, issue 2687) appends `aether-behavior`'s
// `BehaviorHost` to the exported set so the panel's `WidgetKind::BehaviorHost`
// arm can spawn it by tag; the two invocations are cfg-exclusive, keeping the
// ordinary kit build's exported set (and its `aether.kinds` section) unchanged.
#[cfg(not(feature = "behavior"))]
aether_actor::export!(
    Locomotion,
    CameraComponent,
    MeshViewer,
    WorldView,
    Widget,
    SliderWidget,
    TextFieldWidget,
    RadioGroupWidget,
    ButtonWidget,
    LabelWidget,
    WidgetPanel
);

#[cfg(feature = "behavior")]
aether_actor::export!(
    Locomotion,
    CameraComponent,
    MeshViewer,
    WorldView,
    Widget,
    SliderWidget,
    TextFieldWidget,
    RadioGroupWidget,
    ButtonWidget,
    LabelWidget,
    WidgetPanel,
    aether_behavior::BehaviorHost
);
