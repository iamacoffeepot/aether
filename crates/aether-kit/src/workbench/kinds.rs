//! Public wire vocabulary for the terrain annotation workbench.

use alloc::{string::String, vec::Vec};
use core::f32::consts::{FRAC_PI_3, PI};

use aether_data::MailboxId;
use serde::{Deserialize, Serialize};

use crate::console::ConsoleConfig;
use crate::mark::MarkRef;
use crate::terra::TerraError;
use crate::widget::EditorRegionRect;
use crate::widget::theme::Theme;
use crate::world::{
    AutomatonRule, BrushParameters, MAX_TERRAIN_PICK_DISTANCE_METERS, OperatorBudget, ProposalDigest, ProposalError,
    ProposalId, TerrainPickError, WorldPoint, WorldPositionMeters,
};

/// Three independently-routed regions assembled by the workbench.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct WorkbenchLayout {
    pub tools: EditorRegionRect,
    pub viewport: EditorRegionRect,
    pub console: EditorRegionRect,
}

/// Perspective camera state owned by the terrain viewport.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct WorkbenchCamera {
    pub eye: WorldPositionMeters,
    pub target: WorldPositionMeters,
    pub vertical_field_of_view_radians: f32,
    pub near_clip_meters: f32,
    pub far_clip_meters: f32,
    pub maximum_pick_distance_meters: f32,
}

/// Geometry being authored from viewport terrain hits.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkbenchMarkMode {
    Point,
    #[default]
    Path,
    Area,
}

/// Existing bounded world operator selected for proposal staging.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkbenchOperator {
    #[default]
    Brush,
    Automaton,
}

/// Initial mark and operator values shown by the specialized panel.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WorkbenchInitialSettings {
    pub mark_mode: WorkbenchMarkMode,
    pub operator: WorkbenchOperator,
    pub brush: BrushParameters,
    pub automaton: AutomatonRule,
    pub budget: OperatorBudget,
}

impl Default for WorkbenchInitialSettings {
    fn default() -> Self {
        Self {
            mark_mode: WorkbenchMarkMode::Path,
            operator: WorkbenchOperator::Brush,
            brush: BrushParameters { radius_octimeters: 128, spacing_octimeters: 128, material: 3 },
            automaton: AutomatonRule::Grow { material: 3, generations: 1 },
            budget: OperatorBudget { max_steps: 64, max_subcells: 16_384 },
        }
    }
}

/// Tool-panel visual and font settings.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct WorkbenchPanelSettings {
    pub font_namespace: String,
    pub font_path: String,
    pub theme: Theme,
}

/// `aether.kit.workbench.config` — authoritative peers plus local assembly state.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.workbench.config")]
pub struct WorkbenchConfig {
    pub mark_book_mailbox: MailboxId,
    pub terra_mailbox: MailboxId,
    pub world_mailbox: MailboxId,
    pub layout: WorkbenchLayout,
    pub camera: WorkbenchCamera,
    pub panel: WorkbenchPanelSettings,
    pub console: ConsoleConfig,
    pub initial: WorkbenchInitialSettings,
}

impl Default for WorkbenchConfig {
    fn default() -> Self {
        Self {
            mark_book_mailbox: MailboxId::NONE,
            terra_mailbox: MailboxId::NONE,
            world_mailbox: MailboxId::NONE,
            layout: WorkbenchLayout {
                tools: EditorRegionRect { x_pixels: 0.0, y_pixels: 0.0, width_pixels: 240.0, height_pixels: 720.0 },
                viewport: EditorRegionRect {
                    x_pixels: 240.0,
                    y_pixels: 0.0,
                    width_pixels: 1040.0,
                    height_pixels: 600.0,
                },
                console: EditorRegionRect {
                    x_pixels: 240.0,
                    y_pixels: 600.0,
                    width_pixels: 1040.0,
                    height_pixels: 120.0,
                },
            },
            camera: WorkbenchCamera {
                eye: WorldPositionMeters { x_meters: 4.0, y_meters: 8.0, z_meters: 8.0 },
                target: WorldPositionMeters { x_meters: 4.0, y_meters: 0.0, z_meters: 4.0 },
                vertical_field_of_view_radians: FRAC_PI_3,
                near_clip_meters: 0.1,
                far_clip_meters: 100.0,
                maximum_pick_distance_meters: 32.0,
            },
            panel: WorkbenchPanelSettings::default(),
            console: ConsoleConfig::default(),
            initial: WorkbenchInitialSettings::default(),
        }
    }
}

/// Cached in-progress authoring values exposed without private request context.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WorkbenchDraftState {
    pub mark_mode: WorkbenchMarkMode,
    pub instruction: String,
    pub points: Vec<WorldPoint>,
    pub operator: WorkbenchOperator,
    pub brush: BrushParameters,
    pub automaton: AutomatonRule,
    pub budget: OperatorBudget,
}

impl From<&WorkbenchInitialSettings> for WorkbenchDraftState {
    fn from(initial: &WorkbenchInitialSettings) -> Self {
        Self {
            mark_mode: initial.mark_mode,
            instruction: String::new(),
            points: Vec::new(),
            operator: initial.operator,
            brush: initial.brush,
            automaton: initial.automaton,
            budget: initial.budget,
        }
    }
}

/// Cached staged proposal and whether its exact preview is active.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct WorkbenchProposalState {
    pub proposal_id: ProposalId,
    pub digest: ProposalDigest,
    pub preview_active: bool,
}

/// Workbench control that rejected an internal intent.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkbenchControl {
    Viewport,
    MarkMode,
    Instruction,
    Operator,
    Radius,
    Spacing,
    Material,
    MaximumSteps,
    MaximumSubcells,
    FinishMark,
    Stage,
    Preview,
    Accept,
    Discard,
    Protocol,
}

/// Typed workbench failure, preserving authoritative peer errors.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum WorkbenchFailure {
    TerrainPick { error: TerrainPickError },
    TerrainMiss,
    Terra { error: TerraError },
    Proposal { error: ProposalError },
    NoSelection,
    NoProposal,
    MissingMark { requested: MarkRef },
    UnsupportedGeometry { operator: WorkbenchOperator, mark_mode: WorkbenchMarkMode },
    Control { control: WorkbenchControl, reason: String },
}

/// `aether.kit.workbench.query` — read cached coordinator state.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, Default)]
#[kind(name = "aether.kit.workbench.query")]
pub struct WorkbenchQuery;

/// `aether.kit.workbench.query_result` — immediate, server-handled observability.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[kind(name = "aether.kit.workbench.query_result")]
pub struct WorkbenchQueryResult {
    pub selection: Vec<MarkRef>,
    pub draft: WorkbenchDraftState,
    pub proposal: Option<WorkbenchProposalState>,
    pub busy: bool,
    pub failure: Option<WorkbenchFailure>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WorkbenchConfigError {
    MissingMailbox,
    InvalidRegion,
    OverlappingRegions,
    NonFiniteCamera,
    DegenerateCamera,
    InvalidFieldOfView,
    InvalidClipRange,
    InvalidMaximumPickDistance,
}

impl WorkbenchConfig {
    pub(super) fn validate(&self) -> Result<(), WorkbenchConfigError> {
        if [self.mark_book_mailbox, self.terra_mailbox, self.world_mailbox]
            .into_iter()
            .any(|mailbox| mailbox == MailboxId::NONE)
        {
            return Err(WorkbenchConfigError::MissingMailbox);
        }
        let regions = [self.layout.tools, self.layout.viewport, self.layout.console];
        if regions.into_iter().any(|region| !valid_region(region)) {
            return Err(WorkbenchConfigError::InvalidRegion);
        }
        if overlaps(self.layout.tools, self.layout.viewport)
            || overlaps(self.layout.tools, self.layout.console)
            || overlaps(self.layout.viewport, self.layout.console)
        {
            return Err(WorkbenchConfigError::OverlappingRegions);
        }
        let camera = self.camera;
        let camera_scalars = [
            camera.eye.x_meters,
            camera.eye.y_meters,
            camera.eye.z_meters,
            camera.target.x_meters,
            camera.target.y_meters,
            camera.target.z_meters,
            camera.vertical_field_of_view_radians,
            camera.near_clip_meters,
            camera.far_clip_meters,
            camera.maximum_pick_distance_meters,
        ];
        if camera_scalars.into_iter().any(|value| !value.is_finite()) {
            return Err(WorkbenchConfigError::NonFiniteCamera);
        }
        let delta_x = camera.target.x_meters - camera.eye.x_meters;
        let delta_y = camera.target.y_meters - camera.eye.y_meters;
        let delta_z = camera.target.z_meters - camera.eye.z_meters;
        if delta_x.mul_add(delta_x, delta_y.mul_add(delta_y, delta_z * delta_z)) <= f32::EPSILON {
            return Err(WorkbenchConfigError::DegenerateCamera);
        }
        if !(0.0 < camera.vertical_field_of_view_radians && camera.vertical_field_of_view_radians < PI) {
            return Err(WorkbenchConfigError::InvalidFieldOfView);
        }
        if !(camera.near_clip_meters > 0.0 && camera.far_clip_meters > camera.near_clip_meters) {
            return Err(WorkbenchConfigError::InvalidClipRange);
        }
        if !(camera.maximum_pick_distance_meters > 0.0
            && camera.maximum_pick_distance_meters <= MAX_TERRAIN_PICK_DISTANCE_METERS)
        {
            return Err(WorkbenchConfigError::InvalidMaximumPickDistance);
        }
        Ok(())
    }
}

fn valid_region(region: EditorRegionRect) -> bool {
    [region.x_pixels, region.y_pixels, region.width_pixels, region.height_pixels].into_iter().all(f32::is_finite)
        && region.x_pixels >= 0.0
        && region.y_pixels >= 0.0
        && region.width_pixels > 0.0
        && region.height_pixels > 0.0
}

fn overlaps(left: EditorRegionRect, right: EditorRegionRect) -> bool {
    left.x_pixels < right.x_pixels + right.width_pixels
        && right.x_pixels < left.x_pixels + left.width_pixels
        && left.y_pixels < right.y_pixels + right.height_pixels
        && right.y_pixels < left.y_pixels + left.height_pixels
}

#[cfg(test)]
mod tests {
    use aether_data::Kind;

    use super::*;

    fn valid_config() -> WorkbenchConfig {
        WorkbenchConfig {
            mark_book_mailbox: MailboxId(1),
            terra_mailbox: MailboxId(2),
            world_mailbox: MailboxId(3),
            layout: WorkbenchLayout {
                tools: EditorRegionRect { x_pixels: 0.0, y_pixels: 0.0, width_pixels: 240.0, height_pixels: 640.0 },
                viewport: EditorRegionRect {
                    x_pixels: 240.0,
                    y_pixels: 0.0,
                    width_pixels: 720.0,
                    height_pixels: 540.0,
                },
                console: EditorRegionRect {
                    x_pixels: 240.0,
                    y_pixels: 540.0,
                    width_pixels: 720.0,
                    height_pixels: 100.0,
                },
            },
            camera: WorkbenchCamera {
                eye: WorldPositionMeters { x_meters: 4.0, y_meters: 8.0, z_meters: 7.0 },
                target: WorldPositionMeters { x_meters: 4.0, y_meters: 0.0, z_meters: 4.0 },
                vertical_field_of_view_radians: FRAC_PI_3,
                near_clip_meters: 0.1,
                far_clip_meters: 100.0,
                maximum_pick_distance_meters: 32.0,
            },
            panel: WorkbenchPanelSettings::default(),
            console: ConsoleConfig::default(),
            initial: WorkbenchInitialSettings::default(),
        }
    }

    #[test]
    fn exact_public_kind_names_are_stable() {
        assert_eq!(WorkbenchConfig::NAME, "aether.kit.workbench.config");
        assert_eq!(WorkbenchQuery::NAME, "aether.kit.workbench.query");
        assert_eq!(WorkbenchQueryResult::NAME, "aether.kit.workbench.query_result");
    }

    #[test]
    fn config_rejects_missing_peers_bad_regions_and_bad_camera_values() {
        assert_eq!(valid_config().validate(), Ok(()));

        let mut config = valid_config();
        config.world_mailbox = MailboxId::NONE;
        assert_eq!(config.validate(), Err(WorkbenchConfigError::MissingMailbox));

        let mut config = valid_config();
        config.layout.viewport.width_pixels = 0.0;
        assert_eq!(config.validate(), Err(WorkbenchConfigError::InvalidRegion));

        let mut config = valid_config();
        config.layout.viewport.x_pixels = 200.0;
        assert_eq!(config.validate(), Err(WorkbenchConfigError::OverlappingRegions));

        let mut config = valid_config();
        config.camera.eye.x_meters = f32::NAN;
        assert_eq!(config.validate(), Err(WorkbenchConfigError::NonFiniteCamera));

        let mut config = valid_config();
        config.camera.target = config.camera.eye;
        assert_eq!(config.validate(), Err(WorkbenchConfigError::DegenerateCamera));

        let mut config = valid_config();
        config.camera.near_clip_meters = config.camera.far_clip_meters;
        assert_eq!(config.validate(), Err(WorkbenchConfigError::InvalidClipRange));

        let mut config = valid_config();
        config.camera.maximum_pick_distance_meters = MAX_TERRAIN_PICK_DISTANCE_METERS + 1.0;
        assert_eq!(config.validate(), Err(WorkbenchConfigError::InvalidMaximumPickDistance));
    }

    #[test]
    fn semantic_public_fields_are_named_records_not_positional_stand_ins() {
        let WorkbenchLayout { tools, viewport, console } = valid_config().layout;
        assert!(tools.width_pixels > 0.0 && viewport.width_pixels > 0.0 && console.width_pixels > 0.0);
        let WorkbenchCamera {
            eye,
            target,
            vertical_field_of_view_radians,
            near_clip_meters,
            far_clip_meters,
            maximum_pick_distance_meters,
        } = valid_config().camera;
        assert!(eye != target);
        assert!(vertical_field_of_view_radians > 0.0);
        assert!(near_clip_meters < far_clip_meters);
        assert!(maximum_pick_distance_meters > 0.0);

        for line in include_str!("kinds.rs").lines().filter(|line| line.trim_start().starts_with("pub ")) {
            assert!(!line.contains(": ["), "new public semantic fields must not use fixed arrays: {line}");
            assert!(
                !line.contains("struct ") || line.trim_end().ends_with('{') || line.trim_end().ends_with(';'),
                "public structs must be named: {line}"
            );
        }
    }
}
