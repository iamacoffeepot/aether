//! Loadable terrain annotation workbench assembly.

#![allow(clippy::needless_pass_by_value)]

mod kinds;
mod panel;
mod viewport;

pub use kinds::*;
pub use panel::{TerrainToolPanel, TerrainToolPanelConfig};
pub use viewport::{TerrainViewport, TerrainViewportConfig};

use alloc::{format, string::String, vec, vec::Vec};

use aether_actor::{ActorInitError, Manual, OutboundReply, RequestId, Subname, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_capabilities::LifecycleCapability;
use aether_capabilities::lifecycle::LifecycleMailboxExt;
use aether_data::{Kind, MailboxId};
use aether_kinds::Tick;
use serde::{Deserialize, Serialize};

use crate::console::{ConsoleCommandOutput, ConsoleOverlay};
use crate::mark::{Mark, MarkGeometry, MarkGet, MarkGetResult, MarkRef};
use crate::terra::{CreateTerraMark, RelabelTerraSelection, TerraCommandResult, TerraError};
use crate::widget::{EditorConfig, EditorKeyChord, EditorShell, RegionInputLanes, RegionSpec};
use crate::world::{
    ApplyBrush, AutomatonRule, CellPos, CommitProposal, DiscardProposal, OperatorCell, PickTerrain, PickTerrainResult,
    ProposalId, ProposalOperation, ProposalResult, Propose, RunAutomaton, SetMarkOverlaySelection,
    SetMarkOverlaySelectionResult, SetMarkOverlayVisibility, SetProposalPreview,
};

use self::panel::{TerrainToolPanelState, WorkbenchIntent};
use self::viewport::{
    TerrainViewportEvent, TerrainViewportPickCompletion, TerrainViewportPickCompletionOutcome,
    TerrainViewportPickIntent,
};

const PANEL_SUBNAME: &str = "tools";
const VIEWPORT_SUBNAME: &str = "viewport";
const CONSOLE_SUBNAME: &str = "console";
const SHELL_SUBNAME: &str = "shell";

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[kind(name = "aether.kit.workbench.request_context")]
struct WorkbenchRequestContext {
    stage: WorkbenchRequestStage,
    sequence: u64,
}

#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
enum WorkbenchRequestStage {
    TerrainPick,
    TerraCreate,
    TerraRelabel,
    OverlaySelection,
    MarkGet,
    Propose,
    Preview,
    Commit,
    Discard,
}

#[derive(Debug, Clone, PartialEq)]
enum PendingAction {
    TerrainPick { viewport: MailboxId, sequence: u64 },
    TerraCreate,
    TerraRelabel,
    OverlaySelection { selected: Option<MarkRef>, retry_count: u8 },
    MarkGet { requested: MarkRef },
    Propose,
    Preview { proposal_id: ProposalId },
    Commit { proposal_id: ProposalId },
    Discard { proposal_id: ProposalId },
}

impl PendingAction {
    const fn stage(&self) -> WorkbenchRequestStage {
        match self {
            Self::TerrainPick { .. } => WorkbenchRequestStage::TerrainPick,
            Self::TerraCreate => WorkbenchRequestStage::TerraCreate,
            Self::TerraRelabel => WorkbenchRequestStage::TerraRelabel,
            Self::OverlaySelection { .. } => WorkbenchRequestStage::OverlaySelection,
            Self::MarkGet { .. } => WorkbenchRequestStage::MarkGet,
            Self::Propose => WorkbenchRequestStage::Propose,
            Self::Preview { .. } => WorkbenchRequestStage::Preview,
            Self::Commit { .. } => WorkbenchRequestStage::Commit,
            Self::Discard { .. } => WorkbenchRequestStage::Discard,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct PendingRequest {
    request: RequestId,
    context: WorkbenchRequestContext,
    action: PendingAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DeferredOverlaySelection {
    selected: Option<MarkRef>,
    retry_count: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkbenchChildren {
    panel: MailboxId,
    viewport: MailboxId,
    console: MailboxId,
    shell: MailboxId,
}

/// Root coordinator for one terrain annotation workbench.
pub struct TerrainWorkbench {
    config: WorkbenchConfig,
    children: Option<WorkbenchChildren>,
    selection: Vec<MarkRef>,
    draft: WorkbenchDraftState,
    proposal: Option<WorkbenchProposalState>,
    pending: Option<PendingRequest>,
    deferred_overlay_selection: Option<DeferredOverlaySelection>,
    next_sequence: u64,
    failure: Option<WorkbenchFailure>,
    status: String,
}

impl TerrainWorkbench {
    #[allow(clippy::too_many_lines)] // one literal peer-first assembly keeps all region lanes auditable together
    fn ensure_spawned(&mut self, ctx: &mut WasmCtx<'_, Manual>) {
        if self.children.is_some() {
            return;
        }
        let panel = match ctx.spawn_inline_child::<TerrainToolPanel>(
            Subname::Named(PANEL_SUBNAME),
            &TerrainToolPanelConfig {
                region: self.config.layout.tools,
                settings: self.config.panel.clone(),
                initial: self.config.initial.clone(),
            },
        ) {
            Ok(panel) => panel,
            Err(error) => {
                self.record_failure(
                    ctx,
                    WorkbenchFailure::Control {
                        control: WorkbenchControl::Protocol,
                        reason: format!("terrain tool panel spawn failed: {error:?}"),
                    },
                );
                return;
            }
        };
        let viewport = match ctx.spawn_inline_child::<TerrainViewport>(
            Subname::Named(VIEWPORT_SUBNAME),
            &TerrainViewportConfig {
                world_mailbox: self.config.world_mailbox,
                surface: viewport::layout_surface(self.config.layout),
                region: self.config.layout.viewport,
                camera: self.config.camera,
            },
        ) {
            Ok(viewport) => viewport,
            Err(error) => {
                self.record_failure(
                    ctx,
                    WorkbenchFailure::Control {
                        control: WorkbenchControl::Protocol,
                        reason: format!("terrain viewport spawn failed: {error:?}"),
                    },
                );
                return;
            }
        };
        let mut console_config = self.config.console.clone();
        console_config.owns_input = false;
        let console = match ctx.spawn_inline_child::<ConsoleOverlay>(Subname::Named(CONSOLE_SUBNAME), &console_config) {
            Ok(console) => console,
            Err(error) => {
                self.record_failure(
                    ctx,
                    WorkbenchFailure::Control {
                        control: WorkbenchControl::Protocol,
                        reason: format!("workbench console spawn failed: {error:?}"),
                    },
                );
                return;
            }
        };
        let shell_config = EditorConfig {
            regions: vec![
                RegionSpec {
                    name: String::from(PANEL_SUBNAME),
                    rect: self.config.layout.tools,
                    target: panel,
                    keyboard_focus_eligible: true,
                    input_lanes: RegionInputLanes {
                        pointer_press: true,
                        pointer_release: true,
                        pointer_motion: true,
                        wheel: false,
                        key_press: true,
                        key_release: true,
                        text_input: true,
                        ime_preedit: true,
                        modifiers: true,
                    },
                    activation_chord: None,
                },
                RegionSpec {
                    name: String::from(VIEWPORT_SUBNAME),
                    rect: self.config.layout.viewport,
                    target: viewport,
                    keyboard_focus_eligible: false,
                    input_lanes: RegionInputLanes {
                        pointer_press: true,
                        pointer_release: false,
                        pointer_motion: false,
                        wheel: false,
                        key_press: false,
                        key_release: false,
                        text_input: false,
                        ime_preedit: false,
                        modifiers: false,
                    },
                    activation_chord: None,
                },
                RegionSpec {
                    name: String::from(CONSOLE_SUBNAME),
                    rect: self.config.layout.console,
                    target: console,
                    keyboard_focus_eligible: true,
                    input_lanes: RegionInputLanes {
                        pointer_press: false,
                        pointer_release: false,
                        pointer_motion: false,
                        wheel: true,
                        key_press: true,
                        key_release: true,
                        text_input: true,
                        ime_preedit: false,
                        modifiers: false,
                    },
                    activation_chord: Some(EditorKeyChord {
                        key_code: self.config.console.activation_key_code,
                        shift: false,
                        ctrl: false,
                        alt: false,
                        meta: false,
                    }),
                },
            ],
        };
        let shell = match ctx.spawn_inline_child::<EditorShell>(Subname::Named(SHELL_SUBNAME), &shell_config) {
            Ok(shell) => shell,
            Err(error) => {
                self.record_failure(
                    ctx,
                    WorkbenchFailure::Control {
                        control: WorkbenchControl::Protocol,
                        reason: format!("editor shell spawn failed: {error:?}"),
                    },
                );
                return;
            }
        };
        self.children = Some(WorkbenchChildren { panel, viewport, console, shell });
        ctx.send_to(self.config.world_mailbox, &SetMarkOverlayVisibility { visible: true });
        self.publish_status(ctx, String::from("Ready"), false);
    }

    fn query_result(&self) -> WorkbenchQueryResult {
        WorkbenchQueryResult {
            selection: self.selection.clone(),
            draft: self.draft.clone(),
            proposal: self.proposal.clone(),
            busy: self.busy(),
            failure: self.failure.clone(),
        }
    }

    fn panel_source(&self, source: Option<MailboxId>) -> bool {
        self.children.is_some_and(|children| source == Some(children.panel))
    }

    fn viewport_source(&self, source: Option<MailboxId>) -> bool {
        self.children.is_some_and(|children| source == Some(children.viewport))
    }

    fn return_viewport_pick(
        ctx: &mut WasmCtx<'_, Manual>,
        viewport: MailboxId,
        sequence: u64,
        outcome: TerrainViewportPickCompletionOutcome,
    ) {
        ctx.send_to(viewport, &TerrainViewportPickCompletion { sequence, outcome });
    }

    fn begin<K: Kind>(
        &mut self,
        ctx: &mut WasmCtx<'_, Manual>,
        mailbox: MailboxId,
        payload: &K,
        action: PendingAction,
    ) {
        if self.pending.is_some() {
            return;
        }
        let context = WorkbenchRequestContext { stage: action.stage(), sequence: self.next_sequence };
        self.next_sequence = self.next_sequence.wrapping_add(1).max(1);
        let request = ctx.send_to_with_context(mailbox, payload, &context);
        self.pending = Some(PendingRequest { request, context, action });
        self.publish_panel(ctx);
    }

    fn busy(&self) -> bool {
        self.pending.is_some() || self.deferred_overlay_selection.is_some()
    }

    fn issue_deferred_overlay_selection(&mut self, ctx: &mut WasmCtx<'_, Manual>) {
        if self.pending.is_some() {
            return;
        }
        let Some(deferred) = self.deferred_overlay_selection.take() else {
            return;
        };
        self.begin(
            ctx,
            self.config.world_mailbox,
            &SetMarkOverlaySelection { selected: deferred.selected },
            PendingAction::OverlaySelection { selected: deferred.selected, retry_count: deferred.retry_count },
        );
        self.publish_status(ctx, String::from("Retrying overlay selection synchronization"), false);
    }

    fn accepts_envelope(&self, request: RequestId, context: WorkbenchRequestContext) -> bool {
        self.pending.as_ref().is_some_and(|pending| {
            pending.request == request && pending.context == context && pending.action.stage() == context.stage
        })
    }

    fn take_reply(
        &mut self,
        ctx: &mut WasmCtx<'_, Manual>,
        expected_stages: &[WorkbenchRequestStage],
    ) -> Option<PendingAction> {
        if ctx.source_mailbox().is_some() {
            return None;
        }
        let request = ctx.in_reply_to()?;
        if !self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.request == request && expected_stages.contains(&pending.action.stage()))
        {
            return None;
        }
        let context = ctx.take_context::<WorkbenchRequestContext>()?;
        if !self.accepts_envelope(request, context) {
            return None;
        }
        self.pending.take().map(|pending| pending.action)
    }

    fn publish_panel(&self, ctx: &mut WasmCtx<'_, Manual>) {
        if let Some(children) = self.children {
            ctx.send_to(
                children.panel,
                &TerrainToolPanelState {
                    busy: self.busy(),
                    proposal: self.proposal.clone(),
                    status: self.status.clone(),
                },
            );
        }
    }

    fn publish_status(&mut self, ctx: &mut WasmCtx<'_, Manual>, status: String, error: bool) {
        self.status.clone_from(&status);
        self.publish_panel(ctx);
        if let Some(children) = self.children {
            ctx.send_to(
                children.console,
                &ConsoleCommandOutput { command: String::from("workbench"), lines: vec![status], error },
            );
        }
    }

    fn clear_failure(&mut self, ctx: &mut WasmCtx<'_, Manual>, status: impl Into<String>) {
        self.failure = None;
        self.publish_status(ctx, status.into(), false);
    }

    fn record_failure(&mut self, ctx: &mut WasmCtx<'_, Manual>, failure: WorkbenchFailure) {
        let status = format!("{failure:?}");
        self.failure = Some(failure);
        self.publish_status(ctx, status, true);
    }

    fn require_idle(&mut self, ctx: &mut WasmCtx<'_, Manual>, control: WorkbenchControl) -> bool {
        if self.busy() {
            self.record_failure(
                ctx,
                WorkbenchFailure::Control { control, reason: String::from("workbench operation already in flight") },
            );
            false
        } else {
            true
        }
    }

    fn selected_reference(&self) -> Result<MarkRef, WorkbenchFailure> {
        self.selection.last().copied().ok_or(WorkbenchFailure::NoSelection)
    }

    fn current_proposal(&self) -> Result<&WorkbenchProposalState, WorkbenchFailure> {
        self.proposal.as_ref().ok_or(WorkbenchFailure::NoProposal)
    }

    fn instruction_relabel(&self) -> Option<RelabelTerraSelection> {
        (!self.selection.is_empty()).then(|| RelabelTerraSelection { label: self.draft.instruction.clone() })
    }

    fn create_mark(&mut self, ctx: &mut WasmCtx<'_, Manual>, geometry: MarkGeometry) {
        self.begin(
            ctx,
            self.config.terra_mailbox,
            &CreateTerraMark { geometry, label: self.draft.instruction.clone() },
            PendingAction::TerraCreate,
        );
        self.publish_status(ctx, String::from("Creating terrain mark"), false);
    }

    fn finish_draft_geometry(&self) -> Result<MarkGeometry, WorkbenchFailure> {
        match self.draft.mark_mode {
            WorkbenchMarkMode::Point => Err(WorkbenchFailure::Control {
                control: WorkbenchControl::FinishMark,
                reason: String::from("point marks are created immediately"),
            }),
            WorkbenchMarkMode::Path if self.draft.points.len() >= 2 => {
                Ok(MarkGeometry::Path(self.draft.points.clone()))
            }
            WorkbenchMarkMode::Area if self.draft.points.len() >= 3 => {
                Ok(MarkGeometry::Area(self.draft.points.clone()))
            }
            WorkbenchMarkMode::Path => Err(WorkbenchFailure::Control {
                control: WorkbenchControl::FinishMark,
                reason: String::from("a path mark needs at least two terrain hits"),
            }),
            WorkbenchMarkMode::Area => Err(WorkbenchFailure::Control {
                control: WorkbenchControl::FinishMark,
                reason: String::from("an area mark needs at least three terrain hits"),
            }),
        }
    }

    fn operation_for_mark(&self, mark: &Mark) -> Result<ProposalOperation, WorkbenchFailure> {
        match self.draft.operator {
            WorkbenchOperator::Brush => {
                let path = match &mark.geometry {
                    MarkGeometry::Point(point) => vec![*point],
                    MarkGeometry::Path(points) | MarkGeometry::Area(points) => points.clone(),
                };
                Ok(ProposalOperation::ApplyBrush {
                    request: ApplyBrush {
                        source: mark.reference(),
                        path,
                        brush: self.draft.brush,
                        budget: self.draft.budget,
                    },
                })
            }
            WorkbenchOperator::Automaton => {
                let MarkGeometry::Point(point) = &mark.geometry else {
                    return Err(WorkbenchFailure::UnsupportedGeometry {
                        operator: WorkbenchOperator::Automaton,
                        mark_mode: mark_mode(&mark.geometry),
                    });
                };
                let cell = CellPos::from_octimeters(point.x_octimeters, point.z_octimeters);
                Ok(ProposalOperation::RunAutomaton {
                    request: RunAutomaton {
                        source: mark.reference(),
                        seed: OperatorCell { cell_x: cell.x, cell_z: cell.z },
                        rule: self.draft.automaton,
                        budget: self.draft.budget,
                    },
                })
            }
        }
    }

    fn mirror_overlay_selection(&mut self, ctx: &mut WasmCtx<'_, Manual>) {
        let selected = self.selection.last().copied();
        self.begin(
            ctx,
            self.config.world_mailbox,
            &SetMarkOverlaySelection { selected },
            PendingAction::OverlaySelection { selected, retry_count: 0 },
        );
    }

    fn apply_proposal_result(
        &mut self,
        action: &PendingAction,
        result: ProposalResult,
    ) -> Result<String, WorkbenchFailure> {
        if let ProposalResult::Rejected { error } = result {
            if matches!(action, PendingAction::Propose) {
                self.proposal = None;
            }
            return Err(WorkbenchFailure::Proposal { error });
        }
        match (action, result) {
            (PendingAction::Propose, ProposalResult::Staged { proposal_id, digest, .. }) => {
                self.proposal = Some(WorkbenchProposalState { proposal_id, digest, preview_active: false });
                Ok(String::from("Proposal staged"))
            }
            (
                PendingAction::Preview { proposal_id: expected },
                ProposalResult::PreviewSet { active_proposal_id: Some(observed), digest: Some(digest) },
            ) if expected == &observed => {
                self.proposal = Some(WorkbenchProposalState { proposal_id: observed, digest, preview_active: true });
                Ok(String::from("Proposal preview active"))
            }
            (
                PendingAction::Commit { proposal_id: expected },
                ProposalResult::Committed { proposal_id: observed, .. },
            ) if expected == &observed => {
                self.proposal = None;
                Ok(String::from("Proposal accepted"))
            }
            (PendingAction::Discard { proposal_id: expected }, ProposalResult::Discarded { proposal_id: observed })
                if expected == &observed =>
            {
                self.proposal = None;
                Ok(String::from("Proposal discarded"))
            }
            _ => Err(WorkbenchFailure::Control {
                control: WorkbenchControl::Protocol,
                reason: String::from("proposal reply did not match the active request stage"),
            }),
        }
    }

    #[allow(clippy::too_many_lines)] // exhaustive closed intent translation is clearer as one dispatcher
    fn handle_intent(&mut self, ctx: &mut WasmCtx<'_, Manual>, intent: WorkbenchIntent) {
        if let WorkbenchIntent::Failed { failure } = intent {
            self.record_failure(ctx, failure);
            return;
        }
        let control = intent_control(&intent);
        if !self.require_idle(ctx, control) {
            return;
        }
        match intent {
            WorkbenchIntent::SetMarkMode { mark_mode } => {
                self.draft.mark_mode = mark_mode;
                self.draft.points.clear();
                self.clear_failure(ctx, String::from("Mark mode updated"));
            }
            WorkbenchIntent::SetInstruction { instruction } => {
                self.draft.instruction.clone_from(&instruction);
                if let Some(command) = self.instruction_relabel() {
                    self.begin(ctx, self.config.terra_mailbox, &command, PendingAction::TerraRelabel);
                    self.publish_status(ctx, String::from("Updating mark instruction"), false);
                } else {
                    self.clear_failure(ctx, String::from("Instruction saved for the next mark"));
                }
            }
            WorkbenchIntent::SetOperator { operator } => {
                self.draft.operator = operator;
                self.clear_failure(ctx, String::from("Operator updated"));
            }
            WorkbenchIntent::SetRadius { radius_octimeters } => {
                self.draft.brush.radius_octimeters = radius_octimeters;
                self.clear_failure(ctx, String::from("Brush radius updated"));
            }
            WorkbenchIntent::SetSpacing { spacing_octimeters } => {
                self.draft.brush.spacing_octimeters = spacing_octimeters;
                self.clear_failure(ctx, String::from("Brush spacing updated"));
            }
            WorkbenchIntent::SetMaterial { material } => {
                self.draft.brush.material = material;
                self.draft.automaton = match self.draft.automaton {
                    AutomatonRule::Grow { generations, .. } => AutomatonRule::Grow { material, generations },
                };
                self.clear_failure(ctx, String::from("Operator material updated"));
            }
            WorkbenchIntent::SetMaximumSteps { maximum_steps } => {
                self.draft.budget.max_steps = maximum_steps;
                self.clear_failure(ctx, String::from("Step budget updated"));
            }
            WorkbenchIntent::SetMaximumSubcells { maximum_subcells } => {
                self.draft.budget.max_subcells = maximum_subcells;
                self.clear_failure(ctx, String::from("Subcell budget updated"));
            }
            WorkbenchIntent::FinishMark => match self.finish_draft_geometry() {
                Ok(geometry) => self.create_mark(ctx, geometry),
                Err(failure) => self.record_failure(ctx, failure),
            },
            WorkbenchIntent::Stage => {
                if self.proposal.is_some() {
                    self.record_failure(
                        ctx,
                        WorkbenchFailure::Control {
                            control: WorkbenchControl::Stage,
                            reason: String::from("accept or discard the current proposal before staging another"),
                        },
                    );
                    return;
                }
                match self.selected_reference() {
                    Ok(requested) => self.begin(
                        ctx,
                        self.config.mark_book_mailbox,
                        &MarkGet { id: requested.id },
                        PendingAction::MarkGet { requested },
                    ),
                    Err(failure) => self.record_failure(ctx, failure),
                }
                if self.pending.is_some() {
                    self.publish_status(ctx, String::from("Resolving selected mark"), false);
                }
            }
            WorkbenchIntent::Preview => match self.current_proposal().cloned() {
                Ok(proposal) => self.begin(
                    ctx,
                    self.config.world_mailbox,
                    &SetProposalPreview { proposal_id: Some(proposal.proposal_id) },
                    PendingAction::Preview { proposal_id: proposal.proposal_id },
                ),
                Err(failure) => self.record_failure(ctx, failure),
            },
            WorkbenchIntent::Accept => match self.current_proposal().cloned() {
                Ok(proposal) => self.begin(
                    ctx,
                    self.config.world_mailbox,
                    &CommitProposal { proposal_id: proposal.proposal_id },
                    PendingAction::Commit { proposal_id: proposal.proposal_id },
                ),
                Err(failure) => self.record_failure(ctx, failure),
            },
            WorkbenchIntent::Discard => match self.current_proposal().cloned() {
                Ok(proposal) => self.begin(
                    ctx,
                    self.config.world_mailbox,
                    &DiscardProposal { proposal_id: proposal.proposal_id },
                    PendingAction::Discard { proposal_id: proposal.proposal_id },
                ),
                Err(failure) => self.record_failure(ctx, failure),
            },
            WorkbenchIntent::Failed { .. } => unreachable!(),
        }
    }
}

#[actor(instanced)]
impl WasmActor for TerrainWorkbench {
    type Config = WorkbenchConfig;
    const NAMESPACE: &'static str = "aether.kit.workbench";

    fn init(config: WorkbenchConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        config.validate().map_err(|error| ActorInitError::from(format!("invalid workbench config: {error:?}")))?;
        let draft = WorkbenchDraftState::from(&config.initial);
        Ok(Self {
            config,
            children: None,
            selection: Vec::new(),
            draft,
            proposal: None,
            pending: None,
            deferred_overlay_selection: None,
            next_sequence: 1,
            failure: None,
            status: String::from("Starting"),
        })
    }

    fn wire(&mut self, ctx: &mut WasmCtx<'_>) {
        ctx.actor::<LifecycleCapability>().subscribe::<Tick>();
    }

    #[handler::manual]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_, Manual>, _tick: Tick) {
        self.ensure_spawned(ctx);
        self.issue_deferred_overlay_selection(ctx);
    }

    #[handler::manual]
    fn on_query(&mut self, ctx: &mut WasmCtx<'_, Manual>, _query: WorkbenchQuery) {
        ctx.reply(&self.query_result());
    }

    #[handler::manual]
    fn on_intent(&mut self, ctx: &mut WasmCtx<'_, Manual>, intent: WorkbenchIntent) {
        if self.panel_source(ctx.source_mailbox()) {
            self.handle_intent(ctx, intent);
        }
    }

    #[handler::manual]
    fn on_viewport_pick_intent(&mut self, ctx: &mut WasmCtx<'_, Manual>, intent: TerrainViewportPickIntent) {
        let Some(viewport) = ctx.source_mailbox() else {
            return;
        };
        if !self.viewport_source(Some(viewport)) {
            return;
        }
        if self.busy() {
            let failure = WorkbenchFailure::Control {
                control: WorkbenchControl::Viewport,
                reason: String::from("workbench operation already in flight"),
            };
            Self::return_viewport_pick(
                ctx,
                viewport,
                intent.sequence,
                TerrainViewportPickCompletionOutcome::Failed { failure },
            );
            return;
        }
        self.begin(
            ctx,
            self.config.world_mailbox,
            &PickTerrain { ray: intent.ray },
            PendingAction::TerrainPick { viewport, sequence: intent.sequence },
        );
        self.publish_status(ctx, String::from("Picking terrain"), false);
    }

    #[handler::manual]
    fn on_viewport_event(&mut self, ctx: &mut WasmCtx<'_, Manual>, event: TerrainViewportEvent) {
        if !self.viewport_source(ctx.source_mailbox()) {
            return;
        }
        match event {
            TerrainViewportEvent::Failed { failure } => self.record_failure(ctx, failure),
            TerrainViewportEvent::Hit { hit } => {
                if !self.require_idle(ctx, WorkbenchControl::Viewport) {
                    return;
                }
                let point = hit.surface.mark_point;
                match self.draft.mark_mode {
                    WorkbenchMarkMode::Point => self.create_mark(ctx, MarkGeometry::Point(point)),
                    WorkbenchMarkMode::Path | WorkbenchMarkMode::Area => {
                        self.draft.points.push(point);
                        self.clear_failure(ctx, format!("Captured {} mark points", self.draft.points.len()));
                    }
                }
            }
        }
    }

    #[handler::manual]
    fn on_pick_terrain_result(&mut self, ctx: &mut WasmCtx<'_, Manual>, result: PickTerrainResult) {
        let Some(PendingAction::TerrainPick { viewport, sequence }) =
            self.take_reply(ctx, &[WorkbenchRequestStage::TerrainPick])
        else {
            return;
        };
        Self::return_viewport_pick(ctx, viewport, sequence, TerrainViewportPickCompletionOutcome::World { result });
    }

    #[handler::manual]
    fn on_terra_command_result(&mut self, ctx: &mut WasmCtx<'_, Manual>, result: TerraCommandResult) {
        let Some(action) =
            self.take_reply(ctx, &[WorkbenchRequestStage::TerraCreate, WorkbenchRequestStage::TerraRelabel])
        else {
            return;
        };
        match result {
            TerraCommandResult::Applied { selection, .. } => {
                self.selection = selection;
                if matches!(action, PendingAction::TerraCreate) {
                    self.draft.points.clear();
                }
                self.failure = None;
                self.mirror_overlay_selection(ctx);
            }
            TerraCommandResult::PartiallyApplied { selection, error, .. }
            | TerraCommandResult::Rejected { selection, error } => {
                self.selection = selection;
                self.record_failure(ctx, WorkbenchFailure::Terra { error });
            }
        }
    }

    #[handler::manual]
    fn on_overlay_selection_result(&mut self, ctx: &mut WasmCtx<'_, Manual>, result: SetMarkOverlaySelectionResult) {
        let Some(PendingAction::OverlaySelection { selected, retry_count }) =
            self.take_reply(ctx, &[WorkbenchRequestStage::OverlaySelection])
        else {
            return;
        };
        match result {
            SetMarkOverlaySelectionResult::Selected { .. } | SetMarkOverlaySelectionResult::Cleared => {
                self.clear_failure(ctx, String::from("Selection synchronized"));
            }
            SetMarkOverlaySelectionResult::Unsynchronized { .. } if retry_count == 0 => {
                self.deferred_overlay_selection = Some(DeferredOverlaySelection { selected, retry_count: 1 });
                self.publish_status(ctx, String::from("Waiting for mark overlay synchronization"), false);
            }
            SetMarkOverlaySelectionResult::Unsynchronized { .. } => self.record_failure(
                ctx,
                WorkbenchFailure::Control {
                    control: WorkbenchControl::Protocol,
                    reason: String::from("mark overlay remained unsynchronized after the exact retry"),
                },
            ),
            SetMarkOverlaySelectionResult::Stale { requested, current } => self.record_failure(
                ctx,
                WorkbenchFailure::Terra { error: TerraError::StaleReference { requested, current } },
            ),
        }
    }

    #[handler::manual]
    fn on_mark_get_result(&mut self, ctx: &mut WasmCtx<'_, Manual>, result: MarkGetResult) {
        let Some(PendingAction::MarkGet { requested }) = self.take_reply(ctx, &[WorkbenchRequestStage::MarkGet]) else {
            return;
        };
        let Some(mark) = result.mark else {
            self.record_failure(ctx, WorkbenchFailure::MissingMark { requested });
            return;
        };
        if mark.reference() != requested {
            self.record_failure(ctx, WorkbenchFailure::MissingMark { requested });
            return;
        }
        let operation = match self.operation_for_mark(&mark) {
            Ok(operation) => operation,
            Err(failure) => {
                self.record_failure(ctx, failure);
                return;
            }
        };
        self.begin(ctx, self.config.world_mailbox, &Propose { operation }, PendingAction::Propose);
        self.publish_status(ctx, String::from("Staging proposal"), false);
    }

    #[handler::manual]
    fn on_proposal_result(&mut self, ctx: &mut WasmCtx<'_, Manual>, result: ProposalResult) {
        let Some(action) = self.take_reply(
            ctx,
            &[
                WorkbenchRequestStage::Propose,
                WorkbenchRequestStage::Preview,
                WorkbenchRequestStage::Commit,
                WorkbenchRequestStage::Discard,
            ],
        ) else {
            return;
        };
        match self.apply_proposal_result(&action, result) {
            Ok(status) => self.clear_failure(ctx, status),
            Err(failure) => self.record_failure(ctx, failure),
        }
    }
}

fn mark_mode(geometry: &MarkGeometry) -> WorkbenchMarkMode {
    match geometry {
        MarkGeometry::Point(_) => WorkbenchMarkMode::Point,
        MarkGeometry::Path(_) => WorkbenchMarkMode::Path,
        MarkGeometry::Area(_) => WorkbenchMarkMode::Area,
    }
}

fn intent_control(intent: &WorkbenchIntent) -> WorkbenchControl {
    match intent {
        WorkbenchIntent::SetMarkMode { .. } => WorkbenchControl::MarkMode,
        WorkbenchIntent::SetInstruction { .. } => WorkbenchControl::Instruction,
        WorkbenchIntent::SetOperator { .. } => WorkbenchControl::Operator,
        WorkbenchIntent::SetRadius { .. } => WorkbenchControl::Radius,
        WorkbenchIntent::SetSpacing { .. } => WorkbenchControl::Spacing,
        WorkbenchIntent::SetMaterial { .. } => WorkbenchControl::Material,
        WorkbenchIntent::SetMaximumSteps { .. } => WorkbenchControl::MaximumSteps,
        WorkbenchIntent::SetMaximumSubcells { .. } => WorkbenchControl::MaximumSubcells,
        WorkbenchIntent::FinishMark => WorkbenchControl::FinishMark,
        WorkbenchIntent::Stage => WorkbenchControl::Stage,
        WorkbenchIntent::Preview => WorkbenchControl::Preview,
        WorkbenchIntent::Accept => WorkbenchControl::Accept,
        WorkbenchIntent::Discard => WorkbenchControl::Discard,
        WorkbenchIntent::Failed { .. } => WorkbenchControl::Protocol,
    }
}

#[cfg(test)]
mod tests {
    use crate::console::{ConsoleConfig, ConsoleTheme};
    use crate::mark::{MarkId, MarkRef};
    use crate::widget::EditorRegionRect;
    use crate::widget::theme::Theme;
    use crate::world::{
        AutomatonRule, BrushParameters, OperatorBudget, OperatorChunk, ProposalDigest, ProposalError, ProposalId,
        ProposalOperationResult, WorldPoint,
    };

    use super::*;

    fn reference(id: u32, revision: u32) -> MarkRef {
        MarkRef { id: MarkId::new(id), revision }
    }

    fn config() -> WorkbenchConfig {
        WorkbenchConfig {
            mark_book_mailbox: MailboxId(10),
            terra_mailbox: MailboxId(11),
            world_mailbox: MailboxId(12),
            layout: WorkbenchLayout {
                tools: EditorRegionRect { x_pixels: 0.0, y_pixels: 0.0, width_pixels: 200.0, height_pixels: 600.0 },
                viewport: EditorRegionRect {
                    x_pixels: 200.0,
                    y_pixels: 0.0,
                    width_pixels: 600.0,
                    height_pixels: 500.0,
                },
                console: EditorRegionRect {
                    x_pixels: 200.0,
                    y_pixels: 500.0,
                    width_pixels: 600.0,
                    height_pixels: 100.0,
                },
            },
            camera: WorkbenchCamera::default(),
            panel: WorkbenchPanelSettings {
                font_namespace: String::new(),
                font_path: String::new(),
                theme: Theme::default(),
            },
            console: ConsoleConfig { theme: ConsoleTheme::default(), ..ConsoleConfig::default() },
            initial: WorkbenchInitialSettings {
                mark_mode: WorkbenchMarkMode::Path,
                operator: WorkbenchOperator::Brush,
                brush: BrushParameters { radius_octimeters: 128, spacing_octimeters: 128, material: 3 },
                automaton: AutomatonRule::Grow { material: 3, generations: 1 },
                budget: OperatorBudget { max_steps: 32, max_subcells: 8192 },
            },
        }
    }

    fn workbench() -> TerrainWorkbench {
        let config = config();
        TerrainWorkbench {
            draft: WorkbenchDraftState::from(&config.initial),
            config,
            children: None,
            selection: Vec::new(),
            proposal: None,
            pending: None,
            deferred_overlay_selection: None,
            next_sequence: 1,
            failure: None,
            status: String::new(),
        }
    }

    fn digest() -> ProposalDigest {
        ProposalDigest {
            touched_chunks: vec![OperatorChunk { chunk_x: 0, chunk_z: 0 }],
            triangle_count: 9,
            changed_geometry_bounds: None,
        }
    }

    #[test]
    fn wrong_duplicate_and_stale_reply_contexts_cannot_advance_state() {
        let mut workbench = workbench();
        let context = WorkbenchRequestContext { stage: WorkbenchRequestStage::MarkGet, sequence: 7 };
        workbench.pending = Some(PendingRequest {
            request: RequestId(20),
            context,
            action: PendingAction::MarkGet { requested: reference(1, 1) },
        });
        assert!(workbench.accepts_envelope(RequestId(20), context));
        assert!(!workbench.accepts_envelope(RequestId(19), context));
        assert!(!workbench.accepts_envelope(RequestId(20), WorkbenchRequestContext { sequence: 6, ..context }));
        assert!(!workbench.accepts_envelope(
            RequestId(20),
            WorkbenchRequestContext { stage: WorkbenchRequestStage::Propose, sequence: 7 }
        ));
        assert!(workbench.pending.is_some());
    }

    #[test]
    fn terrain_pick_intents_require_the_exact_child_and_pending_retains_its_route() {
        let mut workbench = workbench();
        workbench.children = Some(WorkbenchChildren {
            panel: MailboxId(40),
            viewport: MailboxId(41),
            console: MailboxId(42),
            shell: MailboxId(43),
        });
        assert!(workbench.viewport_source(Some(MailboxId(41))));
        assert!(!workbench.viewport_source(Some(MailboxId(40))));
        assert!(!workbench.viewport_source(Some(MailboxId(42))));
        assert!(!workbench.viewport_source(None));

        let action = PendingAction::TerrainPick { viewport: MailboxId(41), sequence: 9 };
        assert!(matches!(action, PendingAction::TerrainPick { viewport: MailboxId(41), sequence: 9 }));
    }

    #[test]
    fn deferred_overlay_retry_remains_busy_until_the_next_tick_can_issue_it() {
        let mut workbench = workbench();
        workbench.deferred_overlay_selection =
            Some(DeferredOverlaySelection { selected: Some(reference(4, 2)), retry_count: 1 });
        assert!(workbench.busy());
        assert!(workbench.query_result().busy);
        assert_eq!(
            workbench.deferred_overlay_selection,
            Some(DeferredOverlaySelection { selected: Some(reference(4, 2)), retry_count: 1 })
        );
    }

    #[test]
    fn point_path_and_area_finish_rules_are_explicit() {
        let mut workbench = workbench();
        workbench.draft.mark_mode = WorkbenchMarkMode::Point;
        assert!(matches!(workbench.finish_draft_geometry(), Err(WorkbenchFailure::Control { .. })));

        workbench.draft.mark_mode = WorkbenchMarkMode::Path;
        workbench.draft.points = vec![WorldPoint::new(1, 2)];
        assert!(matches!(workbench.finish_draft_geometry(), Err(WorkbenchFailure::Control { .. })));
        workbench.draft.points.push(WorldPoint::new(3, 4));
        assert!(matches!(workbench.finish_draft_geometry(), Ok(MarkGeometry::Path(points)) if points.len() == 2));

        workbench.draft.mark_mode = WorkbenchMarkMode::Area;
        assert!(matches!(workbench.finish_draft_geometry(), Err(WorkbenchFailure::Control { .. })));
        workbench.draft.points.push(WorldPoint::new(5, 6));
        assert!(matches!(workbench.finish_draft_geometry(), Ok(MarkGeometry::Area(points)) if points.len() == 3));
    }

    #[test]
    fn authoritative_geometry_maps_to_brush_path_or_point_automaton_seed() {
        let mut workbench = workbench();
        let mark = Mark {
            id: MarkId::new(3),
            revision: 2,
            geometry: MarkGeometry::Path(vec![WorldPoint::new(256, 512), WorldPoint::new(768, 1024)]),
            label: String::from("ridge"),
        };
        assert!(matches!(
            workbench.operation_for_mark(&mark),
            Ok(ProposalOperation::ApplyBrush { request })
                if request.source == mark.reference() && request.path == match &mark.geometry {
                    MarkGeometry::Path(points) => points.clone(),
                    _ => Vec::new(),
                }
        ));

        workbench.draft.operator = WorkbenchOperator::Automaton;
        let point = Mark { geometry: MarkGeometry::Point(WorldPoint::new(640, -128)), ..mark.clone() };
        assert!(matches!(
            workbench.operation_for_mark(&point),
            Ok(ProposalOperation::RunAutomaton { request }) if request.seed == OperatorCell { cell_x: 2, cell_z: -1 }
        ));
        assert!(matches!(
            workbench.operation_for_mark(&mark),
            Err(WorkbenchFailure::UnsupportedGeometry {
                operator: WorkbenchOperator::Automaton,
                mark_mode: WorkbenchMarkMode::Path,
            })
        ));
    }

    #[test]
    fn selection_and_proposal_guards_are_typed() {
        let mut workbench = workbench();
        assert_eq!(workbench.selected_reference(), Err(WorkbenchFailure::NoSelection));
        assert_eq!(workbench.current_proposal(), Err(WorkbenchFailure::NoProposal));
        workbench.selection.push(reference(7, 1));
        assert_eq!(workbench.selected_reference(), Ok(reference(7, 1)));
    }

    #[test]
    fn staged_rejected_stale_commit_and_clear_transitions_preserve_lifecycle() {
        let mut workbench = workbench();
        let staged_id = ProposalId { value: 5 };
        assert_eq!(
            workbench.apply_proposal_result(
                &PendingAction::Propose,
                ProposalResult::Rejected {
                    error: ProposalError::NoTouchedChunks { operation_result: ProposalOperationResult::Mutation },
                },
            ),
            Err(WorkbenchFailure::Proposal {
                error: ProposalError::NoTouchedChunks { operation_result: ProposalOperationResult::Mutation },
            })
        );
        assert!(workbench.proposal.is_none(), "rejection creates no acceptable candidate");

        workbench
            .apply_proposal_result(
                &PendingAction::Propose,
                ProposalResult::Staged {
                    proposal_id: staged_id,
                    operation_result: ProposalOperationResult::Mutation,
                    digest: digest(),
                },
            )
            .expect("stage");
        assert_eq!(workbench.proposal.as_ref().map(|proposal| proposal.proposal_id), Some(staged_id));

        let stale =
            ProposalError::StaleProposal { proposal_id: staged_id, proposed_at_revision: 1, committed_revision: 2 };
        assert_eq!(
            workbench.apply_proposal_result(
                &PendingAction::Commit { proposal_id: staged_id },
                ProposalResult::Rejected { error: stale.clone() },
            ),
            Err(WorkbenchFailure::Proposal { error: stale })
        );
        assert!(workbench.proposal.is_some(), "stale commit remains discardable");

        workbench
            .apply_proposal_result(
                &PendingAction::Discard { proposal_id: staged_id },
                ProposalResult::Discarded { proposal_id: staged_id },
            )
            .expect("discard");
        assert!(workbench.proposal.is_none());

        workbench.proposal =
            Some(WorkbenchProposalState { proposal_id: staged_id, digest: digest(), preview_active: true });
        workbench
            .apply_proposal_result(
                &PendingAction::Commit { proposal_id: staged_id },
                ProposalResult::Committed { proposal_id: staged_id, digest: digest() },
            )
            .expect("commit");
        assert!(workbench.proposal.is_none(), "commit clears local preview state");
    }

    #[test]
    fn instruction_commit_relabels_only_when_selection_exists() {
        let mut empty = workbench();
        empty.draft.instruction = String::from("ridge");
        assert!(empty.instruction_relabel().is_none());
        let mut selected = workbench();
        selected.selection.push(reference(2, 1));
        selected.draft.instruction = String::from("ridge");
        assert_eq!(selected.instruction_relabel().map(|command| command.label), Some(String::from("ridge")));
    }

    #[test]
    fn selector_and_peer_component_name_are_exact() {
        assert_eq!(<TerrainWorkbench as aether_actor::Addressable>::NAMESPACE, "aether.kit.workbench");
        assert_eq!(<aether_capabilities::WasmTrampoline as aether_actor::Addressable>::NAMESPACE, "aether.embedded");
    }
}
