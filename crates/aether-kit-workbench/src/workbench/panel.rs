//! Specialized terrain tool panel built from the stock widget primitives.

#![allow(clippy::needless_pass_by_value)]

use alloc::{format, string::String, vec, vec::Vec};

use aether_actor::{ActorInitError, Addressable, Manual, Subname, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_data::MailboxId;
use aether_kinds::keycode::KEY_TAB;
use aether_kinds::mouse_button;
use aether_kinds::{
    ImePreedit, Key, KeyRelease, Modifiers, MouseButton, MouseButtonRelease, MouseMove, TextInput, Tick,
};
use aether_lifecycle::LifecycleCapability;
use aether_lifecycle::LifecycleMailboxExt;
use aether_math::Vec2;
use aether_text::{LoadFont, LoadFontResult, TextCapability};
use serde::{Deserialize, Serialize};

use aether_kit_terrain::world::AutomatonRule;
use aether_kit_widget::composite::Composite;
use aether_kit_widget::focus::{
    AvailabilityEffects, Focus, FocusDirection, FocusEligibility, FocusRect, FocusTransition, HoverTransition,
};
use aether_kit_widget::set::{ButtonWidget, LabelWidget, NumericWidget, SegmentedWidget, TextFieldWidget};
use aether_kit_widget::theme::SetTheme;
use aether_kit_widget::theme::Theme;
use aether_kit_widget::{
    ButtonClicked, ButtonConfig, Collect, EditorRegionRect, FocusGained, FocusLost, HoverGained, HoverLost,
    LabelConfig, NumericChanged, NumericConfig, SegmentedConfig, SegmentedSelected, SetWidgetState, TextCommitted,
    TextFieldConfig, WidgetClipRect, WidgetControlState, WidgetDrawItem, WidgetDrawList, WidgetFrame,
    WidgetStateChanged, emit,
};

use super::{
    TerrainWorkbench, WorkbenchControl, WorkbenchFailure, WorkbenchInitialSettings, WorkbenchMarkMode,
    WorkbenchOperator, WorkbenchPanelSettings, WorkbenchProposalState,
};

const MARK_MODE_SUBNAME: &str = "mark_mode";
const INSTRUCTION_SUBNAME: &str = "instruction";
const OPERATOR_SUBNAME: &str = "operator";
const RADIUS_SUBNAME: &str = "radius";
const SPACING_SUBNAME: &str = "spacing";
const MATERIAL_SUBNAME: &str = "material";
const MAX_STEPS_SUBNAME: &str = "max_steps";
const MAX_SUBCELLS_SUBNAME: &str = "max_subcells";
const FINISH_MARK_SUBNAME: &str = "finish_mark";
const STAGE_SUBNAME: &str = "stage";
const PREVIEW_SUBNAME: &str = "preview";
const ACCEPT_SUBNAME: &str = "accept";
const DISCARD_SUBNAME: &str = "discard";
const STATUS_SUBNAME: &str = "status";

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[kind(name = "aether.kit.workbench.panel.config")]
pub struct TerrainToolPanelConfig {
    pub region: EditorRegionRect,
    pub settings: WorkbenchPanelSettings,
    pub initial: WorkbenchInitialSettings,
}

impl Default for TerrainToolPanelConfig {
    fn default() -> Self {
        Self {
            region: EditorRegionRect { x_pixels: 0.0, y_pixels: 0.0, width_pixels: 240.0, height_pixels: 720.0 },
            settings: WorkbenchPanelSettings::default(),
            initial: WorkbenchInitialSettings::default(),
        }
    }
}

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[kind(name = "aether.kit.workbench.panel.state")]
pub(super) struct TerrainToolPanelState {
    pub busy: bool,
    pub proposal: Option<WorkbenchProposalState>,
    pub status: String,
}

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[kind(name = "aether.kit.workbench.intent")]
pub(super) enum WorkbenchIntent {
    SetMarkMode { mark_mode: WorkbenchMarkMode },
    SetInstruction { instruction: String },
    SetOperator { operator: WorkbenchOperator },
    SetRadius { radius_octimeters: u32 },
    SetSpacing { spacing_octimeters: u32 },
    SetMaterial { material: u8 },
    SetMaximumSteps { maximum_steps: u32 },
    SetMaximumSubcells { maximum_subcells: u32 },
    FinishMark,
    Stage,
    Preview,
    Accept,
    Discard,
    Failed { failure: WorkbenchFailure },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanelChild {
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
    Status,
}

impl PanelChild {
    const fn subname(self) -> &'static str {
        match self {
            Self::MarkMode => MARK_MODE_SUBNAME,
            Self::Instruction => INSTRUCTION_SUBNAME,
            Self::Operator => OPERATOR_SUBNAME,
            Self::Radius => RADIUS_SUBNAME,
            Self::Spacing => SPACING_SUBNAME,
            Self::Material => MATERIAL_SUBNAME,
            Self::MaximumSteps => MAX_STEPS_SUBNAME,
            Self::MaximumSubcells => MAX_SUBCELLS_SUBNAME,
            Self::FinishMark => FINISH_MARK_SUBNAME,
            Self::Stage => STAGE_SUBNAME,
            Self::Preview => PREVIEW_SUBNAME,
            Self::Accept => ACCEPT_SUBNAME,
            Self::Discard => DISCARD_SUBNAME,
            Self::Status => STATUS_SUBNAME,
        }
    }

    const fn pointer_eligible(self) -> bool {
        !matches!(self, Self::Status)
    }

    const fn focus_eligible(self) -> bool {
        !matches!(self, Self::Status)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PanelChildRef {
    id: MailboxId,
    child: PanelChild,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // one named availability bit per action group is the panel state contract
struct PanelActionAvailability {
    common: bool,
    finish: bool,
    stage: bool,
    preview: bool,
    accept: bool,
    discard: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct NumericRange {
    minimum: f32,
    maximum: f32,
    step: f32,
}

impl PanelActionAvailability {
    fn from_state(state: &TerrainToolPanelState) -> Self {
        if state.busy {
            return Self { common: false, finish: false, stage: false, preview: false, accept: false, discard: false };
        }
        let Some(proposal) = &state.proposal else {
            return Self { common: true, finish: true, stage: true, preview: false, accept: false, discard: false };
        };
        Self {
            common: true,
            finish: true,
            stage: false,
            preview: !proposal.preview_active,
            accept: true,
            discard: true,
        }
    }

    const fn enabled(self, child: PanelChild) -> bool {
        match child {
            PanelChild::MarkMode
            | PanelChild::Instruction
            | PanelChild::Operator
            | PanelChild::Radius
            | PanelChild::Spacing
            | PanelChild::Material
            | PanelChild::MaximumSteps
            | PanelChild::MaximumSubcells => self.common,
            PanelChild::FinishMark => self.finish,
            PanelChild::Stage => self.stage,
            PanelChild::Preview => self.preview,
            PanelChild::Accept => self.accept,
            PanelChild::Discard => self.discard,
            PanelChild::Status => true,
        }
    }
}

/// Widget-cluster root dedicated to terrain authoring controls.
pub struct TerrainToolPanel {
    config: TerrainToolPanelConfig,
    theme: Theme,
    composite: Composite,
    focus: Focus,
    children: Vec<PanelChildRef>,
    spawned: bool,
    frame_open: bool,
    modifiers: Modifiers,
    panel_state: TerrainToolPanelState,
}

impl TerrainToolPanel {
    #[allow(clippy::too_many_lines)] // the fixed stable control stack is clearest in authored row order
    fn ensure_spawned(&mut self, ctx: &mut WasmCtx<'_, Manual>) {
        if self.spawned {
            return;
        }
        self.spawned = true;
        let initial = self.config.initial.clone();
        let normal = WidgetControlState::default();
        let unavailable = WidgetControlState { enabled: false, ..WidgetControlState::default() };
        let mark_mode_index = match initial.mark_mode {
            WorkbenchMarkMode::Point => 0,
            WorkbenchMarkMode::Path => 1,
            WorkbenchMarkMode::Area => 2,
        };
        let operator_index = match initial.operator {
            WorkbenchOperator::Brush => 0,
            WorkbenchOperator::Automaton => 1,
        };
        let automaton_material = match initial.automaton {
            AutomatonRule::Grow { material, .. } => material,
        };
        let material = if initial.operator == WorkbenchOperator::Brush {
            initial.brush.material
        } else {
            automaton_material
        };

        self.spawn_child::<SegmentedWidget>(
            ctx,
            PanelChild::MarkMode,
            &SegmentedConfig {
                options: vec![String::from("Point"), String::from("Path"), String::from("Area")],
                initial_index: mark_mode_index,
                theme: self.theme.clone(),
                state: normal.clone(),
            },
        );
        self.spawn_child::<TextFieldWidget>(
            ctx,
            PanelChild::Instruction,
            &TextFieldConfig {
                initial: String::new(),
                max_chars: 512,
                theme: self.theme.clone(),
                state: normal.clone(),
            },
        );
        self.spawn_child::<SegmentedWidget>(
            ctx,
            PanelChild::Operator,
            &SegmentedConfig {
                options: vec![String::from("Brush"), String::from("Automaton")],
                initial_index: operator_index,
                theme: self.theme.clone(),
                state: normal.clone(),
            },
        );
        let ordinary_range = NumericRange { minimum: 0.0, maximum: 65_535.0, step: 1.0 };
        self.spawn_numeric(ctx, PanelChild::Radius, ordinary_range, initial.brush.radius_octimeters, &normal);
        self.spawn_numeric(ctx, PanelChild::Spacing, ordinary_range, initial.brush.spacing_octimeters, &normal);
        self.spawn_numeric(
            ctx,
            PanelChild::Material,
            NumericRange { minimum: 0.0, maximum: 255.0, step: 1.0 },
            u32::from(material),
            &normal,
        );
        self.spawn_numeric(
            ctx,
            PanelChild::MaximumSteps,
            NumericRange { minimum: 0.0, maximum: 1_000_000.0, step: 1.0 },
            initial.budget.max_steps,
            &normal,
        );
        self.spawn_numeric(
            ctx,
            PanelChild::MaximumSubcells,
            NumericRange { minimum: 0.0, maximum: 16_000_000.0, step: 1.0 },
            initial.budget.max_subcells,
            &normal,
        );
        for (child, label, state) in [
            (PanelChild::FinishMark, "Finish mark", normal.clone()),
            (PanelChild::Stage, "Stage", normal),
            (PanelChild::Preview, "Preview", unavailable.clone()),
            (PanelChild::Accept, "Accept", unavailable.clone()),
            (PanelChild::Discard, "Discard", unavailable),
        ] {
            self.spawn_child::<ButtonWidget>(
                ctx,
                child,
                &ButtonConfig { label: String::from(label), theme: self.theme.clone(), state },
            );
        }
        self.spawn_child::<LabelWidget>(
            ctx,
            PanelChild::Status,
            &LabelConfig {
                text: self.panel_state.status.clone(),
                theme: self.theme.clone(),
                state: WidgetControlState::default(),
            },
        );

        self.layout_children(ctx);
        self.apply_panel_state(ctx);
    }

    #[allow(clippy::cast_precision_loss)]
    fn spawn_numeric(
        &mut self,
        ctx: &mut WasmCtx<'_, Manual>,
        child: PanelChild,
        range: NumericRange,
        initial: u32,
        state: &WidgetControlState,
    ) {
        self.spawn_child::<NumericWidget>(
            ctx,
            child,
            &NumericConfig {
                min: range.minimum,
                max: range.maximum,
                step: range.step,
                initial: initial as f32,
                theme: self.theme.clone(),
                state: state.clone(),
            },
        );
    }

    fn spawn_child<A>(&mut self, ctx: &mut WasmCtx<'_, Manual>, child: PanelChild, config: &A::Config)
    where
        A: aether_actor::Instanced + WasmActor + aether_actor::ErasedWasmActor,
        <A as WasmActor>::State: aether_actor::ErasedWasmActor,
    {
        match ctx.spawn_inline_child::<A>(Subname::Named(child.subname()), config) {
            Ok(id) => self.children.push(PanelChildRef { id, child }),
            Err(error) => Self::send_intent(
                ctx,
                &WorkbenchIntent::Failed {
                    failure: WorkbenchFailure::Control {
                        control: WorkbenchControl::Protocol,
                        reason: format!("{} control spawn failed: {error:?}", child.subname()),
                    },
                },
            ),
        }
    }

    fn layout_children(&mut self, ctx: &mut WasmCtx<'_, Manual>) {
        let region = self.config.region;
        let row_height = self.theme.row_height;
        let mut y_pixels = region.y_pixels;
        for child in &self.children {
            let frame = WidgetFrame { x: region.x_pixels, y: y_pixels, width: region.width_pixels, height: row_height };
            let rect = FocusRect { x: frame.x, y: frame.y, width: frame.width, height: frame.height };
            let clip = WidgetClipRect {
                x: frame.x,
                y: frame.y,
                width: frame.width,
                height: frame.height.min((region.y_pixels + region.height_pixels - frame.y).max(0.0)),
            };
            self.composite.register_slot(
                child.id,
                Vec2::new(frame.x, frame.y),
                Some(clip),
                child.child.subname(),
                child_namespace(child.child),
            );
            self.focus.register(
                child.id,
                rect,
                FocusEligibility { pointer: child.child.pointer_eligible(), keyboard: child.child.focus_eligible() },
                &WidgetControlState::default(),
            );
            ctx.send_to(child.id, &frame);
            y_pixels += row_height + self.theme.gap;
        }
    }

    fn child(&self, source: Option<MailboxId>) -> Option<PanelChild> {
        source.and_then(|source| self.children.iter().find(|child| child.id == source).map(|child| child.child))
    }

    fn child_id(&self, wanted: PanelChild) -> Option<MailboxId> {
        self.children.iter().find(|child| child.child == wanted).map(|child| child.id)
    }

    fn finish_frame(&mut self, ctx: &mut WasmCtx<'_, Manual>) {
        if !self.frame_open || !self.composite.is_complete() {
            return;
        }
        emit(ctx, &self.composite.flatten(None));
        self.frame_open = false;
    }

    fn send_intent(ctx: &mut WasmCtx<'_, Manual>, intent: &WorkbenchIntent) {
        if let Some(parent) = ctx.parent() {
            parent.send(intent);
        }
    }

    fn fan_theme(&self, ctx: &mut WasmCtx<'_>) {
        for child in &self.children {
            ctx.send_to(child.id, &SetTheme { theme: self.theme.clone() });
        }
    }

    fn apply_panel_state(&self, ctx: &mut WasmCtx<'_, Manual>) {
        let availability = PanelActionAvailability::from_state(&self.panel_state);
        for child in &self.children {
            ctx.send_to(
                child.id,
                &SetWidgetState {
                    state: WidgetControlState {
                        enabled: availability.enabled(child.child),
                        ..WidgetControlState::default()
                    },
                },
            );
        }
        if let Some(status) = self.child_id(PanelChild::Status) {
            ctx.send_to(
                status,
                &LabelConfig {
                    text: self.panel_state.status.clone(),
                    theme: self.theme.clone(),
                    state: WidgetControlState::default(),
                },
            );
        }
    }
}

#[actor(instanced, child_of(TerrainWorkbench))]
impl WasmActor for TerrainToolPanel {
    type Config = TerrainToolPanelConfig;
    const NAMESPACE: &'static str = "aether.kit.workbench.panel";

    fn init(config: TerrainToolPanelConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        let region = config.region;
        if ![region.x_pixels, region.y_pixels, region.width_pixels, region.height_pixels]
            .into_iter()
            .all(f32::is_finite)
            || region.width_pixels <= 0.0
            || region.height_pixels <= 0.0
        {
            return Err(ActorInitError::from("terrain tool panel region must be finite and positive"));
        }
        Ok(Self {
            theme: config.settings.theme.clone(),
            config,
            composite: Composite::new(),
            focus: Focus::new(),
            children: Vec::new(),
            spawned: false,
            frame_open: false,
            modifiers: Modifiers::default(),
            panel_state: TerrainToolPanelState { busy: false, proposal: None, status: String::from("Ready") },
        })
    }

    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        ctx.actor::<LifecycleCapability>().subscribe::<Tick>();
        if !self.config.settings.font_path.is_empty() {
            ctx.actor::<TextCapability>().send(&LoadFont {
                namespace: self.config.settings.font_namespace.clone(),
                path: self.config.settings.font_path.clone(),
            });
        }
    }

    #[handler::manual]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_, Manual>, _tick: Tick) {
        self.ensure_spawned(ctx);
        self.composite.begin_frame();
        self.frame_open = true;
        self.composite.extend_chrome([WidgetDrawItem::Quad {
            x: self.config.region.x_pixels,
            y: self.config.region.y_pixels,
            width: self.config.region.width_pixels,
            height: self.config.region.height_pixels,
            color: self.theme.surface,
            clip: Some(WidgetClipRect {
                x: self.config.region.x_pixels,
                y: self.config.region.y_pixels,
                width: self.config.region.width_pixels,
                height: self.config.region.height_pixels,
            }),
        }]);
        for child in &self.children {
            ctx.send_to(child.id, &Collect);
        }
        self.finish_frame(ctx);
    }

    #[handler::manual]
    fn on_draw_list(&mut self, ctx: &mut WasmCtx<'_, Manual>, list: WidgetDrawList) {
        if !self.frame_open {
            return;
        }
        let Some(source) = ctx.source_mailbox() else {
            return;
        };
        if self.composite.fill(source, list) {
            self.finish_frame(ctx);
        }
    }

    #[handler::single]
    fn on_mouse_button(&mut self, ctx: &mut WasmCtx<'_>, press: MouseButton) {
        let target = if press.button == mouse_button::LEFT {
            let hit = self.focus.hit_test(press.x, press.y);
            if let Some(child) = hit {
                self.focus.begin_capture(child);
            }
            if let Some(transition) = self.focus.focus_hit(press.x, press.y) {
                apply_focus(ctx, transition);
            }
            hit
        } else {
            self.focus.pointer_target(press.x, press.y)
        };
        if let Some(target) = target {
            ctx.send_to(target, &press);
        }
    }

    #[handler::single]
    fn on_mouse_button_release(&mut self, ctx: &mut WasmCtx<'_>, release: MouseButtonRelease) {
        if let Some(target) = self.focus.pointer_target(release.x, release.y) {
            ctx.send_to(target, &release);
        }
        if release.button == mouse_button::LEFT
            && let Some(transition) = self.focus.release_capture(release.x, release.y)
        {
            apply_hover(ctx, transition);
        }
    }

    #[handler::single]
    fn on_mouse_move(&mut self, ctx: &mut WasmCtx<'_>, moved: MouseMove) {
        if let Some(transition) = self.focus.update_hover(moved.x, moved.y) {
            apply_hover(ctx, transition);
        }
        if let Some(target) = self.focus.pointer_target(moved.x, moved.y) {
            ctx.send_to(target, &moved);
        }
    }

    #[handler::single]
    fn on_key(&mut self, ctx: &mut WasmCtx<'_>, key: Key) {
        if key.code == KEY_TAB {
            let direction = if self.modifiers.shift {
                FocusDirection::Backward
            } else {
                FocusDirection::Forward
            };
            if let Some(transition) = self.focus.move_focus(direction) {
                apply_focus(ctx, transition);
            }
            return;
        }
        if let Some(target) = self.focus.keyboard_target() {
            ctx.send_to(target, &key);
        }
    }

    #[handler::single]
    fn on_key_release(&mut self, ctx: &mut WasmCtx<'_>, release: KeyRelease) {
        if let Some(target) = self.focus.keyboard_target() {
            ctx.send_to(target, &release);
        }
    }

    #[handler::single]
    fn on_text_input(&mut self, ctx: &mut WasmCtx<'_>, input: TextInput) {
        if let Some(target) = self.focus.keyboard_target() {
            ctx.send_to(target, &input);
        }
    }

    #[handler::single]
    fn on_ime_preedit(&mut self, ctx: &mut WasmCtx<'_>, preedit: ImePreedit) {
        if let Some(target) = self.focus.keyboard_target() {
            ctx.send_to(target, &preedit);
        }
    }

    #[handler::single]
    fn on_modifiers(&mut self, ctx: &mut WasmCtx<'_>, modifiers: Modifiers) {
        self.modifiers = modifiers;
        if let Some(target) = self.focus.keyboard_target() {
            ctx.send_to(target, &modifiers);
        }
    }

    #[handler::manual]
    fn on_widget_state_changed(&mut self, ctx: &mut WasmCtx<'_, Manual>, changed: WidgetStateChanged) {
        let Some(source) = ctx.source_mailbox() else {
            return;
        };
        apply_availability(ctx, self.focus.update_availability(source, &changed.state));
    }

    #[handler::manual]
    fn on_segmented_selected(&mut self, ctx: &mut WasmCtx<'_, Manual>, selected: SegmentedSelected) {
        let intent = match (self.child(ctx.source_mailbox()), selected.index) {
            (Some(PanelChild::MarkMode), 0) => {
                Some(WorkbenchIntent::SetMarkMode { mark_mode: WorkbenchMarkMode::Point })
            }
            (Some(PanelChild::MarkMode), 1) => {
                Some(WorkbenchIntent::SetMarkMode { mark_mode: WorkbenchMarkMode::Path })
            }
            (Some(PanelChild::MarkMode), 2) => {
                Some(WorkbenchIntent::SetMarkMode { mark_mode: WorkbenchMarkMode::Area })
            }
            (Some(PanelChild::Operator), 0) => {
                Some(WorkbenchIntent::SetOperator { operator: WorkbenchOperator::Brush })
            }
            (Some(PanelChild::Operator), 1) => {
                Some(WorkbenchIntent::SetOperator { operator: WorkbenchOperator::Automaton })
            }
            _ => None,
        };
        if let Some(intent) = intent {
            Self::send_intent(ctx, &intent);
        }
    }

    #[handler::manual]
    fn on_text_committed(&mut self, ctx: &mut WasmCtx<'_, Manual>, committed: TextCommitted) {
        if self.child(ctx.source_mailbox()) == Some(PanelChild::Instruction) {
            Self::send_intent(ctx, &WorkbenchIntent::SetInstruction { instruction: committed.text });
        }
    }

    #[handler::manual]
    fn on_numeric_changed(&mut self, ctx: &mut WasmCtx<'_, Manual>, changed: NumericChanged) {
        let Some(child) = self.child(ctx.source_mailbox()) else {
            return;
        };
        let Some(intent) = numeric_intent(child, changed) else {
            return;
        };
        Self::send_intent(ctx, &intent);
    }

    #[handler::manual]
    fn on_button_clicked(&mut self, ctx: &mut WasmCtx<'_, Manual>, _clicked: ButtonClicked) {
        let intent = match self.child(ctx.source_mailbox()) {
            Some(PanelChild::FinishMark) => Some(WorkbenchIntent::FinishMark),
            Some(PanelChild::Stage) => Some(WorkbenchIntent::Stage),
            Some(PanelChild::Preview) => Some(WorkbenchIntent::Preview),
            Some(PanelChild::Accept) => Some(WorkbenchIntent::Accept),
            Some(PanelChild::Discard) => Some(WorkbenchIntent::Discard),
            _ => None,
        };
        if let Some(intent) = intent {
            Self::send_intent(ctx, &intent);
        }
    }

    #[handler::manual]
    fn on_panel_state(&mut self, ctx: &mut WasmCtx<'_, Manual>, state: TerrainToolPanelState) {
        self.panel_state = state;
        if self.spawned {
            self.apply_panel_state(ctx);
        }
    }

    #[handler::single]
    fn on_load_font_result(&mut self, ctx: &mut WasmCtx<'_>, result: LoadFontResult) {
        match result {
            LoadFontResult::Ok { font_id, .. } => {
                self.theme.font_id = font_id;
                self.fan_theme(ctx);
            }
            LoadFontResult::Err { namespace, path, error } => {
                tracing::warn!(
                    target: "aether_kit_workbench",
                    %namespace,
                    %path,
                    %error,
                    "terrain tool panel font load failed",
                );
            }
        }
    }

    #[handler::single]
    fn on_set_theme(&mut self, ctx: &mut WasmCtx<'_>, set: SetTheme) {
        self.theme = set.theme;
        self.fan_theme(ctx);
    }
}

fn child_namespace(child: PanelChild) -> &'static str {
    match child {
        PanelChild::MarkMode | PanelChild::Operator => <SegmentedWidget as Addressable>::NAMESPACE,
        PanelChild::Instruction => <TextFieldWidget as Addressable>::NAMESPACE,
        PanelChild::Radius
        | PanelChild::Spacing
        | PanelChild::Material
        | PanelChild::MaximumSteps
        | PanelChild::MaximumSubcells => <NumericWidget as Addressable>::NAMESPACE,
        PanelChild::FinishMark | PanelChild::Stage | PanelChild::Preview | PanelChild::Accept | PanelChild::Discard => {
            <ButtonWidget as Addressable>::NAMESPACE
        }
        PanelChild::Status => <LabelWidget as Addressable>::NAMESPACE,
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss, clippy::cast_sign_loss)]
fn rounded_u32(value: f32) -> u32 {
    value.round().clamp(0.0, u32::MAX as f32) as u32
}

fn numeric_intent(child: PanelChild, changed: NumericChanged) -> Option<WorkbenchIntent> {
    if !changed.committed || !changed.value.is_finite() {
        return None;
    }
    let value = rounded_u32(changed.value);
    match child {
        PanelChild::Radius => Some(WorkbenchIntent::SetRadius { radius_octimeters: value }),
        PanelChild::Spacing => Some(WorkbenchIntent::SetSpacing { spacing_octimeters: value }),
        PanelChild::Material => Some(WorkbenchIntent::SetMaterial { material: u8::try_from(value).unwrap_or(u8::MAX) }),
        PanelChild::MaximumSteps => Some(WorkbenchIntent::SetMaximumSteps { maximum_steps: value }),
        PanelChild::MaximumSubcells => Some(WorkbenchIntent::SetMaximumSubcells { maximum_subcells: value }),
        _ => None,
    }
}

fn apply_focus<M: aether_actor::ReplyMode>(ctx: &mut WasmCtx<'_, M>, transition: FocusTransition) {
    if let Some(previous) = transition.previous {
        ctx.send_to(previous, &FocusLost);
    }
    if let Some(next) = transition.next {
        ctx.send_to(next, &FocusGained);
    }
}

fn apply_hover<M: aether_actor::ReplyMode>(ctx: &mut WasmCtx<'_, M>, transition: HoverTransition) {
    if let Some(previous) = transition.previous {
        ctx.send_to(previous, &HoverLost);
    }
    if let Some(next) = transition.next {
        ctx.send_to(next, &HoverGained);
    }
}

fn apply_availability<M: aether_actor::ReplyMode>(ctx: &mut WasmCtx<'_, M>, effects: AvailabilityEffects) {
    if let Some(hover) = effects.hover {
        apply_hover(ctx, hover);
    }
    if let Some(focus) = effects.focus {
        apply_focus(ctx, focus);
    }
}

#[cfg(test)]
mod tests {
    use aether_kit_terrain::mark::{MarkId, MarkRef};
    use aether_kit_terrain::world::{OperatorChunk, ProposalDigest, ProposalId};

    use super::*;

    fn proposal(preview_active: bool) -> WorkbenchProposalState {
        WorkbenchProposalState {
            proposal_id: ProposalId { value: 3 },
            digest: ProposalDigest {
                touched_chunks: vec![OperatorChunk { chunk_x: 0, chunk_z: 0 }],
                triangle_count: 8,
                changed_geometry_bounds: None,
            },
            preview_active,
        }
    }

    #[test]
    fn stable_subnames_map_to_named_intents_and_numeric_preview_is_ignored() {
        assert_eq!(PanelChild::MarkMode.subname(), "mark_mode");
        assert_eq!(PanelChild::Instruction.subname(), "instruction");
        assert_eq!(PanelChild::MaximumSubcells.subname(), "max_subcells");
        assert_eq!(PanelChild::FinishMark.subname(), "finish_mark");
        assert_eq!(PanelChild::Status.subname(), "status");
        assert_eq!(numeric_intent(PanelChild::Radius, NumericChanged { value: 256.0, committed: false }), None);
        assert_eq!(
            numeric_intent(PanelChild::Radius, NumericChanged { value: 256.0, committed: true }),
            Some(WorkbenchIntent::SetRadius { radius_octimeters: 256 })
        );
    }

    #[test]
    fn action_availability_tracks_idle_busy_staged_and_preview_states() {
        let idle = PanelActionAvailability::from_state(&TerrainToolPanelState {
            busy: false,
            proposal: None,
            status: String::new(),
        });
        assert!(idle.stage && !idle.preview && !idle.accept && !idle.discard);

        let busy = PanelActionAvailability::from_state(&TerrainToolPanelState {
            busy: true,
            proposal: None,
            status: String::new(),
        });
        assert!(!busy.common && !busy.finish && !busy.stage && !busy.preview);

        let staged = PanelActionAvailability::from_state(&TerrainToolPanelState {
            busy: false,
            proposal: Some(proposal(false)),
            status: String::new(),
        });
        assert!(!staged.stage && staged.preview && staged.accept && staged.discard);

        let preview = PanelActionAvailability::from_state(&TerrainToolPanelState {
            busy: false,
            proposal: Some(proposal(true)),
            status: String::new(),
        });
        assert!(!preview.preview && preview.accept && preview.discard);
    }

    #[test]
    fn focus_and_capture_route_within_the_panel_scope() {
        let mut focus = Focus::new();
        let state = WidgetControlState::default();
        focus.register(
            MailboxId(1),
            FocusRect { x: 0.0, y: 0.0, width: 100.0, height: 20.0 },
            FocusEligibility { pointer: true, keyboard: true },
            &state,
        );
        focus.register(
            MailboxId(2),
            FocusRect { x: 0.0, y: 30.0, width: 100.0, height: 20.0 },
            FocusEligibility { pointer: true, keyboard: true },
            &state,
        );
        assert_eq!(focus.focus_hit(10.0, 10.0).expect("focus").next, Some(MailboxId(1)));
        focus.begin_capture(MailboxId(1));
        assert_eq!(focus.pointer_target(10.0, 40.0), Some(MailboxId(1)));
        focus.release_capture(10.0, 40.0);
        assert_eq!(focus.pointer_target(10.0, 40.0), Some(MailboxId(2)));
        assert_eq!(focus.keyboard_target(), Some(MailboxId(1)));
    }

    #[test]
    fn proposal_fixture_uses_named_identity_records() {
        let reference = MarkRef { id: MarkId::new(4), revision: 2 };
        let MarkRef { id, revision } = reference;
        assert_eq!(id.get(), 4);
        assert_eq!(revision, 2);
    }
}
