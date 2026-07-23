//! Workbench-local perspective camera and terrain-ray owner.

#![allow(clippy::needless_pass_by_value)]

use alloc::{format, string::String};
use core::f32::consts::PI;

use aether_actor::{ActorInitError, Manual, RequestId, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_data::MailboxId;
use aether_kinds::{MouseButton, Render, mouse_button};
use aether_lifecycle::LifecycleCapability;
use aether_lifecycle::LifecycleMailboxExt;
use aether_math::{Mat4, Vec3};
use aether_render::{RenderCapability, ViewProjection};
use serde::{Deserialize, Serialize};

use crate::widget::EditorRegionRect;
use aether_kit_terrain::world::{
    MAX_TERRAIN_PICK_DISTANCE_METERS, PickTerrain, PickTerrainResult, TerrainRay, TerrainSurfaceHit, WorldDirection,
    WorldPositionMeters,
};

use super::{WorkbenchCamera, WorkbenchControl, WorkbenchFailure, valid_region};

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
#[kind(name = "aether.kit.workbench.viewport.config")]
pub struct TerrainViewportConfig {
    pub world_mailbox: MailboxId,
    pub surface: EditorRegionRect,
    pub region: EditorRegionRect,
    pub camera: WorkbenchCamera,
}

impl Default for TerrainViewportConfig {
    fn default() -> Self {
        let workbench = super::WorkbenchConfig::default();
        Self {
            world_mailbox: workbench.world_mailbox,
            surface: layout_surface(workbench.layout),
            region: workbench.layout.viewport,
            camera: workbench.camera,
        }
    }
}

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[kind(name = "aether.kit.workbench.viewport.pick_intent")]
pub(super) struct TerrainViewportPickIntent {
    pub sequence: u64,
    pub ray: TerrainRay,
}

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[kind(name = "aether.kit.workbench.viewport.pick_context")]
struct TerrainViewportPickContext {
    sequence: u64,
}

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[kind(name = "aether.kit.workbench.viewport.event")]
pub(super) enum TerrainViewportEvent {
    Hit { hit: TerrainSurfaceHit },
    Failed { failure: WorkbenchFailure },
}

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[kind(name = "aether.kit.workbench.viewport.pick_completion")]
pub(super) struct TerrainViewportPickCompletion {
    pub sequence: u64,
    pub outcome: TerrainViewportPickCompletionOutcome,
}

#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(super) enum TerrainViewportPickCompletionOutcome {
    World { result: PickTerrainResult },
    Failed { failure: WorkbenchFailure },
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct CameraBasis {
    forward: Vec3,
    right: Vec3,
    up: Vec3,
    reference_up: Vec3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewportConfigError {
    MissingWorld,
    InvalidRegion,
    NonFiniteCamera,
    DegenerateCamera,
    InvalidFieldOfView,
    InvalidClipRange,
    InvalidMaximumPickDistance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RayBuildError {
    OutsideRegion,
    InvalidCamera,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingPick {
    Direct { request: RequestId, context: TerrainViewportPickContext },
    Parent { sequence: u64 },
}

/// Viewport actor that owns the exact camera state used for render and pick.
pub struct TerrainViewport {
    config: TerrainViewportConfig,
    pending: Option<PendingPick>,
    next_sequence: u64,
}

impl TerrainViewportConfig {
    fn validate(self) -> Result<(), ViewportConfigError> {
        if self.world_mailbox == MailboxId::NONE {
            return Err(ViewportConfigError::MissingWorld);
        }
        if !valid_region(self.region) {
            return Err(ViewportConfigError::InvalidRegion);
        }
        if !valid_region(self.surface)
            || self.region.x_pixels < self.surface.x_pixels
            || self.region.y_pixels < self.surface.y_pixels
            || self.region.x_pixels + self.region.width_pixels > self.surface.x_pixels + self.surface.width_pixels
            || self.region.y_pixels + self.region.height_pixels > self.surface.y_pixels + self.surface.height_pixels
        {
            return Err(ViewportConfigError::InvalidRegion);
        }
        let camera = self.camera;
        if [
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
        ]
        .into_iter()
        .any(|value| !value.is_finite())
        {
            return Err(ViewportConfigError::NonFiniteCamera);
        }
        if camera_basis(camera).is_none() {
            return Err(ViewportConfigError::DegenerateCamera);
        }
        if !(camera.vertical_field_of_view_radians > 0.0 && camera.vertical_field_of_view_radians < PI) {
            return Err(ViewportConfigError::InvalidFieldOfView);
        }
        if !(camera.near_clip_meters > 0.0 && camera.far_clip_meters > camera.near_clip_meters) {
            return Err(ViewportConfigError::InvalidClipRange);
        }
        if !(camera.maximum_pick_distance_meters > 0.0
            && camera.maximum_pick_distance_meters <= MAX_TERRAIN_PICK_DISTANCE_METERS)
        {
            return Err(ViewportConfigError::InvalidMaximumPickDistance);
        }
        Ok(())
    }
}

impl TerrainViewport {
    fn view_projection(&self) -> Option<ViewProjection> {
        view_projection(self.config.surface, self.config.region, self.config.camera)
    }

    fn ray_for_pixel(&self, x_pixels: f32, y_pixels: f32) -> Result<TerrainRay, RayBuildError> {
        ray_for_pixel(self.config.region, self.config.camera, x_pixels, y_pixels)
    }

    fn accepts_direct_result(&self, request: RequestId, context: TerrainViewportPickContext) -> bool {
        self.pending.as_ref().is_some_and(|pending| {
            matches!(pending, PendingPick::Direct { request: expected_request, context: expected_context }
                if *expected_request == request && *expected_context == context)
        })
    }

    fn accepts_parent_completion(&self, source: Option<MailboxId>, parent: MailboxId, sequence: u64) -> bool {
        source == Some(parent)
            && self.pending.as_ref().is_some_and(
                |pending| matches!(pending, PendingPick::Parent { sequence: expected } if *expected == sequence),
            )
    }

    fn send_parent(ctx: &mut WasmCtx<'_, Manual>, event: &TerrainViewportEvent) {
        if let Some(parent) = ctx.parent() {
            parent.send(event);
        }
    }
}

#[actor(instanced)]
impl WasmActor for TerrainViewport {
    type Config = TerrainViewportConfig;
    const NAMESPACE: &'static str = "aether.kit.workbench.viewport";

    fn init(config: TerrainViewportConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        config.validate().map_err(|error| ActorInitError::from(format!("invalid terrain viewport: {error:?}")))?;
        Ok(Self { config, pending: None, next_sequence: 1 })
    }

    fn wire(&mut self, ctx: &mut WasmCtx<'_>) {
        ctx.actor::<LifecycleCapability>().subscribe::<Render>();
    }

    #[handler::single]
    fn on_render(&mut self, ctx: &mut WasmCtx<'_>, _render: Render) {
        if let Some(view_projection) = self.view_projection() {
            ctx.actor::<RenderCapability>().send(&view_projection);
        }
    }

    #[handler::manual]
    fn on_mouse_button(&mut self, ctx: &mut WasmCtx<'_, Manual>, press: MouseButton) {
        if press.button != mouse_button::LEFT {
            return;
        }
        if self.pending.is_some() {
            Self::send_parent(
                ctx,
                &TerrainViewportEvent::Failed {
                    failure: WorkbenchFailure::Control {
                        control: WorkbenchControl::Viewport,
                        reason: String::from("terrain pick already in flight"),
                    },
                },
            );
            return;
        }
        let ray = match self.ray_for_pixel(press.x, press.y) {
            Ok(ray) => ray,
            Err(RayBuildError::OutsideRegion) => {
                Self::send_parent(
                    ctx,
                    &TerrainViewportEvent::Failed {
                        failure: WorkbenchFailure::Control {
                            control: WorkbenchControl::Viewport,
                            reason: String::from("pointer is outside the configured viewport"),
                        },
                    },
                );
                return;
            }
            Err(RayBuildError::InvalidCamera) => {
                Self::send_parent(
                    ctx,
                    &TerrainViewportEvent::Failed {
                        failure: WorkbenchFailure::Control {
                            control: WorkbenchControl::Viewport,
                            reason: String::from("viewport camera is degenerate"),
                        },
                    },
                );
                return;
            }
        };
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1).max(1);
        if let Some(parent) = ctx.parent() {
            self.pending = Some(PendingPick::Parent { sequence });
            parent.send(&TerrainViewportPickIntent { sequence, ray });
        } else {
            let context = TerrainViewportPickContext { sequence };
            let request = ctx.send_to_with_context(self.config.world_mailbox, &PickTerrain { ray }, &context);
            self.pending = Some(PendingPick::Direct { request, context });
        }
    }

    #[handler::manual]
    fn on_pick_terrain_result(&mut self, ctx: &mut WasmCtx<'_, Manual>, result: PickTerrainResult) {
        if ctx.source_mailbox().is_some() {
            return;
        }
        let Some(request) = ctx.in_reply_to() else {
            return;
        };
        let Some(context) = ctx.take_context::<TerrainViewportPickContext>() else {
            return;
        };
        if !self.accepts_direct_result(request, context) {
            return;
        }
        self.pending = None;
        Self::send_parent(ctx, &viewport_event(TerrainViewportPickCompletionOutcome::World { result }));
    }

    #[handler::manual]
    fn on_pick_completion(&mut self, ctx: &mut WasmCtx<'_, Manual>, reply: TerrainViewportPickCompletion) {
        let Some(parent) = ctx.parent() else {
            return;
        };
        if !self.accepts_parent_completion(ctx.source_mailbox(), parent.mailbox_id(), reply.sequence) {
            return;
        }
        self.pending = None;
        Self::send_parent(ctx, &viewport_event(reply.outcome));
    }
}

fn viewport_event(outcome: TerrainViewportPickCompletionOutcome) -> TerrainViewportEvent {
    match outcome {
        TerrainViewportPickCompletionOutcome::World { result: PickTerrainResult::Hit { hit } } => {
            TerrainViewportEvent::Hit { hit }
        }
        TerrainViewportPickCompletionOutcome::World { result: PickTerrainResult::Miss } => {
            TerrainViewportEvent::Failed { failure: WorkbenchFailure::TerrainMiss }
        }
        TerrainViewportPickCompletionOutcome::World { result: PickTerrainResult::Rejected { error } } => {
            TerrainViewportEvent::Failed { failure: WorkbenchFailure::TerrainPick { error } }
        }
        TerrainViewportPickCompletionOutcome::Failed { failure } => TerrainViewportEvent::Failed { failure },
    }
}

pub(super) fn layout_surface(layout: super::WorkbenchLayout) -> EditorRegionRect {
    let width_pixels = [layout.tools, layout.viewport, layout.console]
        .into_iter()
        .map(|region| region.x_pixels + region.width_pixels)
        .fold(0.0, f32::max);
    let height_pixels = [layout.tools, layout.viewport, layout.console]
        .into_iter()
        .map(|region| region.y_pixels + region.height_pixels)
        .fold(0.0, f32::max);
    EditorRegionRect { x_pixels: 0.0, y_pixels: 0.0, width_pixels, height_pixels }
}

fn position(value: WorldPositionMeters) -> Vec3 {
    Vec3::new(value.x_meters, value.y_meters, value.z_meters)
}

fn camera_basis(camera: WorkbenchCamera) -> Option<CameraBasis> {
    let forward_delta = position(camera.target) - position(camera.eye);
    if forward_delta.length_squared() <= f32::EPSILON {
        return None;
    }
    let forward = forward_delta.normalize();
    let reference_up = if forward.dot(Vec3::Y).abs() > 0.999 {
        -Vec3::Z
    } else {
        Vec3::Y
    };
    let right = forward.cross(reference_up).normalize();
    if right.length_squared() <= f32::EPSILON {
        return None;
    }
    Some(CameraBasis { forward, right, up: right.cross(forward), reference_up })
}

fn view_projection(
    surface: EditorRegionRect,
    region: EditorRegionRect,
    camera: WorkbenchCamera,
) -> Option<ViewProjection> {
    let basis = camera_basis(camera)?;
    let aspect = region.width_pixels / region.height_pixels;
    let view = Mat4::look_at_rh(position(camera.eye), position(camera.target), basis.reference_up);
    let projection = Mat4::perspective_rh(
        camera.vertical_field_of_view_radians,
        aspect,
        camera.near_clip_meters,
        camera.far_clip_meters,
    );
    let scale_x = region.width_pixels / surface.width_pixels;
    let scale_y = region.height_pixels / surface.height_pixels;
    let center_x = (region.width_pixels.mul_add(0.5, region.x_pixels - surface.x_pixels) / surface.width_pixels)
        .mul_add(2.0, -1.0);
    let center_y = (region.height_pixels.mul_add(0.5, region.y_pixels - surface.y_pixels) / surface.height_pixels)
        .mul_add(-2.0, 1.0);
    let viewport_transform = Mat4::from_cols(
        aether_math::Vec4::new(scale_x, 0.0, 0.0, 0.0),
        aether_math::Vec4::new(0.0, scale_y, 0.0, 0.0),
        aether_math::Vec4::new(0.0, 0.0, 1.0, 0.0),
        aether_math::Vec4::new(center_x, center_y, 0.0, 1.0),
    );
    Some(ViewProjection { view_proj: (viewport_transform * projection * view).to_cols_array() })
}

fn ray_for_pixel(
    region: EditorRegionRect,
    camera: WorkbenchCamera,
    x_pixels: f32,
    y_pixels: f32,
) -> Result<TerrainRay, RayBuildError> {
    if x_pixels < region.x_pixels
        || y_pixels < region.y_pixels
        || x_pixels >= region.x_pixels + region.width_pixels
        || y_pixels >= region.y_pixels + region.height_pixels
    {
        return Err(RayBuildError::OutsideRegion);
    }
    let basis = camera_basis(camera).ok_or(RayBuildError::InvalidCamera)?;
    let normalized_x = ((x_pixels - region.x_pixels) / region.width_pixels).mul_add(2.0, -1.0);
    let normalized_y = ((y_pixels - region.y_pixels) / region.height_pixels).mul_add(-2.0, 1.0);
    let tangent = (camera.vertical_field_of_view_radians * 0.5).tan();
    let aspect = region.width_pixels / region.height_pixels;
    let direction =
        (basis.forward + basis.right * (normalized_x * tangent * aspect) + basis.up * (normalized_y * tangent))
            .normalize();
    Ok(TerrainRay {
        origin: camera.eye,
        direction: WorldDirection { x_unitless: direction.x, y_unitless: direction.y, z_unitless: direction.z },
        max_distance_meters: camera.maximum_pick_distance_meters,
    })
}

#[cfg(test)]
mod tests {
    use core::f32::consts::FRAC_PI_3;

    use aether_math::Vec4;

    use super::*;

    fn region() -> EditorRegionRect {
        EditorRegionRect { x_pixels: 120.0, y_pixels: 40.0, width_pixels: 400.0, height_pixels: 200.0 }
    }

    fn surface() -> EditorRegionRect {
        EditorRegionRect { x_pixels: 0.0, y_pixels: 0.0, width_pixels: 640.0, height_pixels: 320.0 }
    }

    fn camera() -> WorkbenchCamera {
        WorkbenchCamera {
            eye: WorldPositionMeters { x_meters: 4.0, y_meters: 8.0, z_meters: 10.0 },
            target: WorldPositionMeters { x_meters: 4.0, y_meters: 1.0, z_meters: 4.0 },
            vertical_field_of_view_radians: FRAC_PI_3,
            near_clip_meters: 0.1,
            far_clip_meters: 100.0,
            maximum_pick_distance_meters: 32.0,
        }
    }

    fn direction(ray: TerrainRay) -> Vec3 {
        Vec3::new(ray.direction.x_unitless, ray.direction.y_unitless, ray.direction.z_unitless)
    }

    #[test]
    fn center_and_off_axis_rays_use_region_local_pixels_and_extent_aspect() {
        let center = ray_for_pixel(region(), camera(), 320.0, 140.0).expect("center ray");
        let basis = camera_basis(camera()).expect("camera basis");
        assert!((direction(center) - basis.forward).length() < 0.000_01);

        let right = ray_for_pixel(region(), camera(), 420.0, 140.0).expect("right ray");
        let upper = ray_for_pixel(region(), camera(), 320.0, 90.0).expect("upper ray");
        assert!(direction(right).dot(basis.right) > 0.0);
        assert!(direction(upper).dot(basis.up) > 0.0);

        let square = EditorRegionRect { width_pixels: 200.0, ..region() };
        let wide_right = direction(ray_for_pixel(region(), camera(), 420.0, 140.0).expect("wide right"));
        let square_right = direction(ray_for_pixel(square, camera(), 270.0, 140.0).expect("square right"));
        assert!(wide_right.dot(basis.right) > square_right.dot(basis.right));
    }

    #[test]
    fn outside_region_and_eye_target_degeneracy_are_rejected() {
        assert_eq!(ray_for_pixel(region(), camera(), 119.0, 140.0), Err(RayBuildError::OutsideRegion));
        let mut degenerate = camera();
        degenerate.target = degenerate.eye;
        assert_eq!(ray_for_pixel(region(), degenerate, 320.0, 140.0), Err(RayBuildError::InvalidCamera));
        assert!(view_projection(surface(), region(), degenerate).is_none());
    }

    #[test]
    fn render_matrix_and_pick_ray_share_basis_field_of_view_and_nonzero_origin() {
        let sample_x = 430.0;
        let sample_y = 100.0;
        let ray = ray_for_pixel(region(), camera(), sample_x, sample_y).expect("sample ray");
        let matrix =
            Mat4::from_cols_array(view_projection(surface(), region(), camera()).expect("view projection").view_proj);
        let direction = direction(ray);
        let eye = position(ray.origin);
        let world_sample = eye + direction * 10.0;
        let clip = matrix * Vec4::new(world_sample.x, world_sample.y, world_sample.z, 1.0);
        let normalized_x = clip.x / clip.w;
        let normalized_y = clip.y / clip.w;
        let expected_x = (sample_x / surface().width_pixels).mul_add(2.0, -1.0);
        let expected_y = (sample_y / surface().height_pixels).mul_add(-2.0, 1.0);
        assert!((normalized_x - expected_x).abs() < 0.000_1, "x: {normalized_x} != {expected_x}");
        assert!((normalized_y - expected_y).abs() < 0.000_1, "y: {normalized_y} != {expected_y}");
    }

    #[test]
    fn vertical_camera_uses_a_stable_nonparallel_up_axis() {
        let mut top_down = camera();
        top_down.eye = WorldPositionMeters { x_meters: 4.0, y_meters: 10.0, z_meters: 4.0 };
        top_down.target = WorldPositionMeters { x_meters: 4.0, y_meters: 0.0, z_meters: 4.0 };
        let basis = camera_basis(top_down).expect("top-down basis");
        assert!(basis.right.length_squared() > 0.99);
        assert!(view_projection(surface(), region(), top_down).is_some());
    }

    #[test]
    fn direct_and_parent_pick_guards_reject_wrong_duplicate_and_stale_replies() {
        let viewport = TerrainViewport {
            config: TerrainViewportConfig {
                world_mailbox: MailboxId(7),
                surface: surface(),
                region: region(),
                camera: camera(),
            },
            pending: Some(PendingPick::Parent { sequence: 4 }),
            next_sequence: 5,
        };
        assert!(viewport.accepts_parent_completion(Some(MailboxId(9)), MailboxId(9), 4));
        assert!(!viewport.accepts_parent_completion(Some(MailboxId(8)), MailboxId(9), 4));
        assert!(!viewport.accepts_parent_completion(Some(MailboxId(9)), MailboxId(9), 3));

        let context = TerrainViewportPickContext { sequence: 7 };
        let direct =
            TerrainViewport { pending: Some(PendingPick::Direct { request: RequestId(12), context }), ..viewport };
        assert!(direct.accepts_direct_result(RequestId(12), context));
        assert!(!direct.accepts_direct_result(RequestId(11), context));
        assert!(!direct.accepts_direct_result(RequestId(12), TerrainViewportPickContext { sequence: 6 }));
    }
}
