//! `aether-kit` — the gameplay-systems layer.
//!
//! Reusable game-building actors that run on the substrate. Each system is
//! one module under the crate root that co-locates the actor with its own
//! `kinds` submodule (the mail shapes peers send it) and whatever support
//! files it needs — the crate is guest code all the way down, so there is
//! no data/runtime split, just one module per actor:
//!
//! - [`camera::CameraComponent`] — the multi-camera driver, selected by the
//!   `aether_kit@aether.kit.camera` export (ADR-0096). Its `aether.kit.camera.*`
//!   driver kinds live in [`camera`].
//! - [`camera::controller::CameraController`] — a keyboard driver that steers a
//!   peer [`camera::CameraComponent`] (WASD / arrows / zoom), selected by the
//!   `aether_kit@aether.kit.camera-controller` export. Its
//!   `aether.kit.camera-controller.config` init-config lives in
//!   [`camera::controller`].
//! - [`PlayerClient`] — the outbound player-session and authoritative
//!   presentation actor, selected by the `aether_kit@aether.kit.client`
//!   export. Its `aether.kit.client.config` init-config lives in [`client`].
//! - [`console::ConsoleOverlay`] — a primitive-rendered developer console
//!   overlay, selected by the `aether_kit@aether.kit.console` export. Its
//!   config and extension command vocabulary live in [`console`].
//! - [`mesh::MeshViewer`] — loads a `.dsl` / `.obj` mesh file and replays it
//!   to the render sink, selected by the `aether_kit@aether.kit.mesh`
//!   export. Its `aether.kit.mesh.load` kind lives in [`mesh`].
//! - [`TurnSim`] — the deterministic fixed-tick reference simulation,
//!   selected by the `aether_kit@aether.kit.sim` export. Its tick-native
//!   intent, trajectory, summary, and catch-up vocabulary lives in [`sim`].
//! - [`widget::Widget`] — the widget-compositing node (ADR-0117), selected
//!   by the `aether_kit@aether.kit.widget` export, with the reference
//!   [`widget::WidgetPanel`] and the concrete [`widget::set`] widgets. The
//!   widget-compositing vocabulary lives in [`widget`] and the visual tokens
//!   in [`widget::theme`].
//! - [`EditorShell`] — the input-only arbiter between independently
//!   rooted editor regions (ADR-0141).
//! - [`TerrainWorkbench`] — the peer-first terrain annotation assembly,
//!   selected by the `aether_kit@aether.kit.workbench` export.
//!
//! The terrain-authoring stack — the mark / world / terra / mover actors and
//! their `CellPos` / `WorldPoint` / octimeter position vocabulary — was
//! extracted to the sibling [`aether-kit-terrain`](aether_kit_terrain) crate
//! (iamacoffeepot/aether#3951); the sim, client, and workbench systems here
//! depend on it for that vocabulary.
//!
//! `export!` (below) packs the actors into one cdylib (ADR-0096 multi-actor
//! module); the explicit entry type is the bare-load target, and the FFI
//! shims it emits are wasm32-only and inert in a host rlib, so the integration
//! tests link the same artifact.

extern crate alloc;

pub mod camera;
pub mod client;
pub mod console;
pub mod mesh;
pub mod sim;
pub mod widget;
pub mod workbench;

pub use client::{PlayerClient, PlayerClientConfig};
pub use console::{
    ConsoleCommandInvoked, ConsoleCommandOutput, ConsoleConfig, ConsoleTheme, RegisterConsoleCommand,
    UnregisterConsoleCommand,
};
pub use sim::{
    CellPosition, EntityState, GridBounds, MoveDirection, MoveIntent, Poll, PollResult, SimConfig, Spawn, StateSummary,
    TickBundle, TrajectoryEvent, TrajectoryKind, TurnSim,
};
pub use widget::theme::{SetTheme, Theme, ThemeState};
pub use widget::{
    ButtonClicked, ButtonConfig, ChildrenChanged, Collect, EditorConfig, EditorKeyChord, EditorRegionRect, EditorShell,
    FocusGained, FocusLost, HoverGained, HoverLost, ImageConfig, ImageFit, LabelConfig, MembershipEntry,
    NumericChanged, NumericConfig, PanelConfig, RadioConfig, RadioSelected, RegionInputLanes, RegionSpec, ScrollConfig,
    ScrollDelta, ScrollExtent, ScrollOffset, ScrollOutcome, ScrollResidual, SegmentedConfig, SegmentedSelected,
    SetWidgetState, SliderChanged, SliderConfig, TextAreaConfig, TextCommitted, TextFieldConfig, ToggleChanged,
    ToggleConfig, VirtualListConfig, VirtualListSelected, WidgetChildSpec, WidgetClipRect, WidgetConfig,
    WidgetControlState, WidgetDrawItem, WidgetDrawList, WidgetFrame, WidgetKind, WidgetStateChanged, WidgetValidation,
};
pub use workbench::{
    TerrainToolPanel, TerrainViewport, TerrainWorkbench, WorkbenchCamera, WorkbenchConfig, WorkbenchControl,
    WorkbenchDraftState, WorkbenchFailure, WorkbenchInitialSettings, WorkbenchLayout, WorkbenchMarkMode,
    WorkbenchOperator, WorkbenchPanelSettings, WorkbenchProposalState, WorkbenchQuery, WorkbenchQueryResult,
};

// A cdylib carries one `export!` (the shared init/receive FFI entry); the
// macro emits the wasm32 FFI shims and the `aether.kinds` custom section for
// every listed actor. The kit is a subsystem library — a grab-bag of
// unrelated actors (camera, mesh viewer, widget set) each loaded
// independently — so ADR-0138's defaultless policy still governs every actor
// except the one explicitly named as the default. `console::ConsoleOverlay`
// is the kit's narrow bare-load target; all other actors stay selector-only by
// `module@actor` selector, never by list position. The `behavior` feature
// (ADR-0137, issue 2687) appends `aether-behavior`'s `BehaviorHost` so the
// panel's `WidgetKind::BehaviorHost` arm can spawn it by tag; the two
// invocations are cfg-exclusive, keeping the ordinary kit build's exported set
// (and its `aether.kinds` section) unchanged.
#[cfg(not(feature = "behavior"))]
aether_actor::export!(
    default = console::ConsoleOverlay,
    camera::CameraComponent,
    camera::controller::CameraController,
    PlayerClient,
    mesh::MeshViewer,
    TurnSim,
    widget::Widget,
    widget::ScrollWidget,
    widget::set::SliderWidget,
    widget::set::TextFieldWidget,
    widget::set::TextAreaWidget,
    widget::set::RadioGroupWidget,
    widget::set::ButtonWidget,
    widget::set::LabelWidget,
    widget::set::ImageWidget,
    widget::set::VirtualListWidget,
    widget::set::ToggleWidget,
    widget::set::SegmentedWidget,
    widget::set::NumericWidget,
    EditorShell,
    widget::WidgetPanel,
    TerrainToolPanel,
    TerrainViewport,
    TerrainWorkbench
);

#[cfg(feature = "behavior")]
aether_actor::export!(
    default = console::ConsoleOverlay,
    camera::CameraComponent,
    camera::controller::CameraController,
    PlayerClient,
    mesh::MeshViewer,
    TurnSim,
    widget::Widget,
    widget::ScrollWidget,
    widget::set::SliderWidget,
    widget::set::TextFieldWidget,
    widget::set::TextAreaWidget,
    widget::set::RadioGroupWidget,
    widget::set::ButtonWidget,
    widget::set::LabelWidget,
    widget::set::ImageWidget,
    widget::set::VirtualListWidget,
    widget::set::ToggleWidget,
    widget::set::SegmentedWidget,
    widget::set::NumericWidget,
    EditorShell,
    widget::WidgetPanel,
    TerrainToolPanel,
    TerrainViewport,
    TerrainWorkbench,
    aether_behavior::BehaviorHost
);
