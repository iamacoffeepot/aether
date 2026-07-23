// Fixed-point authoritative positions become floats only at the render boundary.
#![allow(clippy::cast_precision_loss)]
// `#[handler]` methods take decoded mail by value per the actor ABI.
#![allow(clippy::needless_pass_by_value)]

//! [`PlayerClient`] — the desktop guest for the authoritative player slice.
//!
//! The actor dials a configured game gateway through `aether.tcp`, performs the
//! recipient-free [`PlayerFrame`] handshake, emits allowlisted player intents,
//! atomically replaces a named-octimeter scene from authoritative tick bundles,
//! and draws that scene through `aether.render`. Projection remains owned by a
//! separately loaded `aether.kit.camera` actor.

mod kinds;
pub use kinds::*;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_codec::frame::encode_frame;
use aether_data::{Kind, MailboxId, wire};
use aether_game::{CellPosition, GridBounds, MoveDirection, MoveIntent, PlayerFrame, Spawn, TickBundle, WIRE_VERSION};
use aether_kinds::{Key, KeyRelease, Render, Tick, keycode};
use aether_lifecycle::{LifecycleCapability, LifecycleMailboxExt};
use aether_math::Rgb;
use aether_render::{DrawTriangle, RenderCapability, Vertex};
use aether_tcp::{ConnectResult, SessionClosed, SessionData, TcpCapability, TcpWasmExt};
use aether_window::{WindowCapability, WindowMailboxExt, WindowSelector};

use aether_kit_terrain::OCTIMETERS_PER_TILE;
use aether_kit_terrain::world::WorldPoint;

const MAX_RENDER_GRID_EDGE: i64 = 32;
const MAX_RENDER_ENTITIES: usize = 1_024;
const GRID_INSET_OCTIMETERS: i32 = 8;
const MARKER_HALF_EXTENT_OCTIMETERS: i32 = 72;
const GRID_Y_METERS: f32 = 0.0;
const MARKER_Y_METERS: f32 = 0.025;
const GRID_EVEN_COLOR: Rgb = Rgb::new(0.08, 0.12, 0.18);
const GRID_ODD_COLOR: Rgb = Rgb::new(0.11, 0.16, 0.23);
const ENTITY_COLOR: Rgb = Rgb::new(1.0, 0.0, 1.0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct EntityId {
    value: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectionPhase {
    Connecting,
    AwaitingHelloAck,
    Live,
    Closed,
}

impl ConnectionPhase {
    fn admits_session_mail(self) -> bool {
        matches!(self, Self::AwaitingHelloAck | Self::Live)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConnectedSession {
    name: String,
    id: MailboxId,
    peer: String,
}

impl ConnectedSession {
    fn admits(&self, source: Option<MailboxId>, session_name: &str, peer: &str) -> bool {
        source == Some(self.id) && self.name == session_name && self.peer == peer
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ServerClock {
    tick: u64,
    server_nanos: Option<u64>,
    interval_nanos: u64,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct HeldDirections {
    north: bool,
    east: bool,
    south: bool,
    west: bool,
}

impl HeldDirections {
    fn set(&mut self, code: u32, held: bool) {
        match code {
            keycode::KEY_W => self.north = held,
            keycode::KEY_D => self.east = held,
            keycode::KEY_S => self.south = held,
            keycode::KEY_A => self.west = held,
            _ => {}
        }
    }

    fn resolved(self) -> Option<MoveDirection> {
        let east_west = i8::from(self.east) - i8::from(self.west);
        let north_south = i8::from(self.south) - i8::from(self.north);
        match (east_west, north_south) {
            (0, -1) => Some(MoveDirection::North),
            (1, 0) => Some(MoveDirection::East),
            (0, 1) => Some(MoveDirection::South),
            (-1, 0) => Some(MoveDirection::West),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct AuthoritativeScene {
    tick: Option<u64>,
    superseded_through: u64,
    entities: BTreeMap<EntityId, WorldPoint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SceneApply {
    Committed,
    IgnoredStale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SceneApplyError {
    SummaryTickMismatch { bundle_tick: u64, summary_tick: u64 },
    ImpossibleWatermark { tick: u64, superseded_through: u64 },
    TooManyEntities { count: usize, maximum: usize },
    DuplicateEntity(EntityId),
    CellCenterOverflow { entity: EntityId, position: CellPosition },
}

impl AuthoritativeScene {
    fn apply(&mut self, bundle: &TickBundle) -> Result<SceneApply, SceneApplyError> {
        if self.tick.is_some_and(|tick| bundle.tick <= tick) {
            return Ok(SceneApply::IgnoredStale);
        }
        if bundle.summary.tick != bundle.tick {
            return Err(SceneApplyError::SummaryTickMismatch {
                bundle_tick: bundle.tick,
                summary_tick: bundle.summary.tick,
            });
        }
        if bundle.superseded_through > bundle.tick {
            return Err(SceneApplyError::ImpossibleWatermark {
                tick: bundle.tick,
                superseded_through: bundle.superseded_through,
            });
        }
        if bundle.summary.entities.len() > MAX_RENDER_ENTITIES {
            return Err(SceneApplyError::TooManyEntities {
                count: bundle.summary.entities.len(),
                maximum: MAX_RENDER_ENTITIES,
            });
        }

        let mut replacement = BTreeMap::new();
        for entity in &bundle.summary.entities {
            let entity_id = EntityId { value: entity.entity_id };
            let position = CellPosition { cell_x: entity.cell_x, cell_z: entity.cell_z };
            let Some(world_point) = cell_center(position) else {
                return Err(SceneApplyError::CellCenterOverflow { entity: entity_id, position });
            };
            if replacement.insert(entity_id, world_point).is_some() {
                return Err(SceneApplyError::DuplicateEntity(entity_id));
            }
        }

        self.entities = replacement;
        self.tick = Some(bundle.tick);
        self.superseded_through = bundle.superseded_through;
        Ok(SceneApply::Committed)
    }
}

fn cell_center(position: CellPosition) -> Option<WorldPoint> {
    let half_tile = OCTIMETERS_PER_TILE / 2;
    Some(WorldPoint {
        x_octimeters: position.cell_x.checked_mul(OCTIMETERS_PER_TILE)?.checked_add(half_tile)?,
        z_octimeters: position.cell_z.checked_mul(OCTIMETERS_PER_TILE)?.checked_add(half_tile)?,
    })
}

/// Outbound player protocol, authoritative scene, input policy, and presentation.
pub struct PlayerClient {
    server_addr: String,
    client_name: String,
    spawn_cell: CellPosition,
    grid_bounds: GridBounds,
    phase: ConnectionPhase,
    session: Option<ConnectedSession>,
    session_identity: Option<MailboxId>,
    server_clock: Option<ServerClock>,
    held: HeldDirections,
    scene: AuthoritativeScene,
}

#[actor]
impl WasmActor for PlayerClient {
    type Config = PlayerClientConfig;
    const NAMESPACE: &'static str = "aether.kit.client";

    fn init(config: PlayerClientConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self {
            server_addr: config.server_addr,
            client_name: config.client_name,
            spawn_cell: config.spawn_cell,
            grid_bounds: config.grid_bounds,
            phase: ConnectionPhase::Connecting,
            session: None,
            session_identity: None,
            server_clock: None,
            held: HeldDirections::default(),
            scene: AuthoritativeScene::default(),
        })
    }

    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        let window = ctx.actor::<WindowCapability>();
        window.subscribe::<Key>(WindowSelector::All);
        window.subscribe::<KeyRelease>(WindowSelector::All);
        let lifecycle = ctx.actor::<LifecycleCapability>();
        lifecycle.subscribe::<Tick>();
        lifecycle.subscribe::<Render>();
        ctx.actor::<TcpCapability>().connect(&self.server_addr, None, Some(ctx.mailbox_id()));
    }

    fn unwire(&mut self, ctx: &mut WasmCtx<'_>) {
        if self.phase != ConnectionPhase::Closed
            && let Some(session) = self.session.take()
        {
            ctx.actor::<TcpCapability>().connect_session_close(&session.name);
        }
        self.phase = ConnectionPhase::Closed;
    }

    #[handler::single]
    fn on_connect_result(&mut self, ctx: &mut WasmCtx<'_>, result: ConnectResult) {
        if self.phase != ConnectionPhase::Connecting || self.session.is_some() {
            return;
        }
        // Component replies are source-free. Require the host correlation so
        // an ordinary peer send cannot impersonate the deferred TCP result;
        // the one-shot connection phase identifies the only outstanding dial.
        if ctx.source_mailbox().is_some() || ctx.in_reply_to().is_none() {
            self.fail(ctx, "tcp connect result was not a correlated component reply");
            return;
        }
        match result {
            ConnectResult::Ok { session_name, session_id, peer } => {
                self.session = Some(ConnectedSession { name: session_name, id: session_id, peer });
                self.phase = ConnectionPhase::AwaitingHelloAck;
                self.send_frame(
                    ctx,
                    &PlayerFrame::Hello { wire_version: WIRE_VERSION, client_name: self.client_name.clone() },
                );
            }
            ConnectResult::Err { addr, reason } => {
                self.fail(ctx, &format!("tcp connect to {addr} failed: {reason}"));
            }
        }
    }

    #[handler::single]
    fn on_session_data(&mut self, ctx: &mut WasmCtx<'_>, mail: SessionData) {
        if !self.phase.admits_session_mail() {
            return;
        }
        if !self.admits_session(ctx.source_mailbox(), &mail.session_name, &mail.peer) {
            self.fail(ctx, "tcp session data did not match the retained session mailbox and metadata");
            return;
        }
        let frame = match wire::from_bytes::<PlayerFrame>(&mail.bytes) {
            Ok(frame) => frame,
            Err(error) => {
                self.fail(ctx, &format!("invalid player frame: {error}"));
                return;
            }
        };
        match self.phase {
            ConnectionPhase::AwaitingHelloAck => self.handle_hello_ack(ctx, frame),
            ConnectionPhase::Live => self.handle_live_frame(ctx, frame),
            ConnectionPhase::Connecting | ConnectionPhase::Closed => {}
        }
    }

    #[handler::single]
    fn on_session_closed(&mut self, ctx: &mut WasmCtx<'_>, mail: SessionClosed) {
        if !self.phase.admits_session_mail() {
            return;
        }
        if !self.admits_session(ctx.source_mailbox(), &mail.session_name, &mail.peer) {
            self.fail(ctx, "tcp close notice did not match the retained session mailbox and metadata");
            return;
        }
        tracing::warn!(
            target: "aether_kit_sim",
            session = %mail.session_name,
            peer = %mail.peer,
            reason = %mail.reason,
            "player client tcp session closed",
        );
        self.phase = ConnectionPhase::Closed;
    }

    #[handler::single]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_>, _tick: Tick) {
        let (Some(identity), Some(direction)) = (self.session_identity, self.held.resolved()) else {
            return;
        };
        if self.phase == ConnectionPhase::Live {
            self.send_intent(ctx, &MoveIntent { entity_id: identity.0, direction });
        }
    }

    #[handler::single]
    fn on_render(&mut self, ctx: &mut WasmCtx<'_>, _render: Render) {
        let triangles = render_triangles(self.grid_bounds, &self.scene.entities);
        if !triangles.is_empty() {
            ctx.actor::<RenderCapability>().send_many(&triangles);
        }
    }

    #[handler::single]
    fn on_key(&mut self, _ctx: &mut WasmCtx<'_>, key: Key) {
        self.held.set(key.code, true);
    }

    #[handler::single]
    fn on_key_release(&mut self, _ctx: &mut WasmCtx<'_>, key: KeyRelease) {
        self.held.set(key.code, false);
    }
}

impl PlayerClient {
    fn admits_session(&self, source: Option<MailboxId>, session_name: &str, peer: &str) -> bool {
        self.session.as_ref().is_some_and(|session| session.admits(source, session_name, peer))
    }

    fn handle_hello_ack(&mut self, ctx: &mut WasmCtx<'_>, frame: PlayerFrame) {
        let PlayerFrame::HelloAck { wire_version, session_identity, tick, interval_nanos } = frame else {
            self.fail(ctx, "expected HelloAck as first server frame");
            return;
        };
        if wire_version != WIRE_VERSION {
            self.fail(ctx, &format!("wire_version mismatch: server={wire_version}, client={WIRE_VERSION}"));
            return;
        }

        self.session_identity = Some(session_identity);
        self.server_clock = Some(ServerClock { tick, server_nanos: None, interval_nanos });
        self.phase = ConnectionPhase::Live;
        self.send_intent(
            ctx,
            &Spawn { entity_id: session_identity.0, cell_x: self.spawn_cell.cell_x, cell_z: self.spawn_cell.cell_z },
        );
    }

    fn handle_live_frame(&mut self, ctx: &mut WasmCtx<'_>, frame: PlayerFrame) {
        match frame {
            PlayerFrame::Fact { kind, payload } if kind == TickBundle::ID => {
                let Some(bundle) = TickBundle::decode_from_bytes(&payload) else {
                    self.fail(ctx, "malformed TickBundle fact payload");
                    return;
                };
                if let Err(error) = self.scene.apply(&bundle) {
                    self.fail(ctx, &format!("invalid authoritative TickBundle: {error:?}"));
                }
            }
            PlayerFrame::Fact { kind, .. } => {
                self.fail(ctx, &format!("unexpected player fact kind: {kind}"));
            }
            PlayerFrame::Beacon { tick, server_nanos, interval_nanos } => {
                self.server_clock = Some(ServerClock { tick, server_nanos: Some(server_nanos), interval_nanos });
            }
            PlayerFrame::Close { reason } => self.fail(ctx, &format!("server closed player session: {reason}")),
            PlayerFrame::Hello { .. } | PlayerFrame::HelloAck { .. } | PlayerFrame::Intent { .. } => {
                self.fail(ctx, "unexpected server player frame direction");
            }
        }
    }

    fn send_intent<K: Kind>(&mut self, ctx: &mut WasmCtx<'_>, intent: &K) {
        self.send_frame(ctx, &PlayerFrame::Intent { kind: K::ID, payload: intent.encode_into_bytes() });
    }

    fn send_frame(&mut self, ctx: &mut WasmCtx<'_>, frame: &PlayerFrame) {
        let bytes = match encode_frame(frame) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.fail(ctx, &format!("player frame encode failed: {error}"));
                return;
            }
        };
        let Some(session) = &self.session else {
            self.fail(ctx, "player frame send attempted without retained tcp session");
            return;
        };
        ctx.actor::<TcpCapability>().connect_session_write(&session.name, &bytes);
    }

    fn fail(&mut self, ctx: &mut WasmCtx<'_>, reason: &str) {
        tracing::warn!(target: "aether_kit_sim", reason, "player client closed");
        self.phase = ConnectionPhase::Closed;
        if let Some(session) = &self.session {
            ctx.actor::<TcpCapability>().connect_session_close(&session.name);
        }
    }
}

#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_field_names)] // Axis fields retain their octimeter unit at the render boundary.
struct RenderRect {
    min_x_octimeters: i32,
    max_x_octimeters: i32,
    min_z_octimeters: i32,
    max_z_octimeters: i32,
}

fn visible_grid(bounds: GridBounds) -> bool {
    if bounds.min_cell_x > bounds.max_cell_x || bounds.min_cell_z > bounds.max_cell_z {
        return false;
    }
    let width = i64::from(bounds.max_cell_x) - i64::from(bounds.min_cell_x) + 1;
    let depth = i64::from(bounds.max_cell_z) - i64::from(bounds.min_cell_z) + 1;
    width <= MAX_RENDER_GRID_EDGE && depth <= MAX_RENDER_GRID_EDGE
}

fn render_triangles(bounds: GridBounds, entities: &BTreeMap<EntityId, WorldPoint>) -> Vec<DrawTriangle> {
    if !visible_grid(bounds) {
        return Vec::new();
    }

    let width = i64::from(bounds.max_cell_x) - i64::from(bounds.min_cell_x) + 1;
    let depth = i64::from(bounds.max_cell_z) - i64::from(bounds.min_cell_z) + 1;
    let mut triangles = Vec::with_capacity(usize::try_from(width * depth * 2).unwrap_or(0) + entities.len() * 2);
    for cell_z in bounds.min_cell_z..=bounds.max_cell_z {
        for cell_x in bounds.min_cell_x..=bounds.max_cell_x {
            let Some(origin) = cell_origin(CellPosition { cell_x, cell_z }) else {
                return Vec::new();
            };
            let Some(rect) = grid_rect(origin) else {
                return Vec::new();
            };
            let color = if cell_x.wrapping_add(cell_z) & 1 == 0 {
                GRID_EVEN_COLOR
            } else {
                GRID_ODD_COLOR
            };
            push_quad(&mut triangles, rect, GRID_Y_METERS, color);
        }
    }
    for point in entities.values() {
        let Some(rect) = marker_rect(*point) else {
            continue;
        };
        push_quad(&mut triangles, rect, MARKER_Y_METERS, ENTITY_COLOR);
    }
    triangles
}

fn cell_origin(position: CellPosition) -> Option<WorldPoint> {
    Some(WorldPoint {
        x_octimeters: position.cell_x.checked_mul(OCTIMETERS_PER_TILE)?,
        z_octimeters: position.cell_z.checked_mul(OCTIMETERS_PER_TILE)?,
    })
}

fn grid_rect(origin: WorldPoint) -> Option<RenderRect> {
    let far_inset = OCTIMETERS_PER_TILE.checked_sub(GRID_INSET_OCTIMETERS)?;
    Some(RenderRect {
        min_x_octimeters: origin.x_octimeters.checked_add(GRID_INSET_OCTIMETERS)?,
        max_x_octimeters: origin.x_octimeters.checked_add(far_inset)?,
        min_z_octimeters: origin.z_octimeters.checked_add(GRID_INSET_OCTIMETERS)?,
        max_z_octimeters: origin.z_octimeters.checked_add(far_inset)?,
    })
}

fn marker_rect(point: WorldPoint) -> Option<RenderRect> {
    Some(RenderRect {
        min_x_octimeters: point.x_octimeters.checked_sub(MARKER_HALF_EXTENT_OCTIMETERS)?,
        max_x_octimeters: point.x_octimeters.checked_add(MARKER_HALF_EXTENT_OCTIMETERS)?,
        min_z_octimeters: point.z_octimeters.checked_sub(MARKER_HALF_EXTENT_OCTIMETERS)?,
        max_z_octimeters: point.z_octimeters.checked_add(MARKER_HALF_EXTENT_OCTIMETERS)?,
    })
}

fn push_quad(out: &mut Vec<DrawTriangle>, rect: RenderRect, y: f32, color: Rgb) {
    let meters = |octimeters: i32| octimeters as f32 / OCTIMETERS_PER_TILE as f32;
    let vertex =
        |x_octimeters: i32, z_octimeters: i32| Vertex { x: meters(x_octimeters), y, z: meters(z_octimeters), color };
    let north_west = vertex(rect.min_x_octimeters, rect.min_z_octimeters);
    let north_east = vertex(rect.max_x_octimeters, rect.min_z_octimeters);
    let south_east = vertex(rect.max_x_octimeters, rect.max_z_octimeters);
    let south_west = vertex(rect.min_x_octimeters, rect.max_z_octimeters);
    out.push(DrawTriangle { verts: [north_west, south_east, north_east] });
    out.push(DrawTriangle { verts: [north_west, south_west, south_east] });
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_game::{EntityState, StateSummary};

    fn bundle(tick: u64, superseded_through: u64, entities: Vec<EntityState>) -> TickBundle {
        TickBundle { tick, superseded_through, trajectory: Vec::new(), summary: StateSummary { tick, entities } }
    }

    fn seeded_scene() -> AuthoritativeScene {
        let mut scene = AuthoritativeScene::default();
        assert_eq!(
            scene.apply(&bundle(4, 4, vec![EntityState { entity_id: 1, cell_x: 0, cell_z: 0 }])),
            Ok(SceneApply::Committed)
        );
        scene
    }

    #[test]
    fn stale_bundle_is_ignored_without_mutating_scene() {
        let mut scene = seeded_scene();
        let before = scene.clone();

        assert_eq!(
            scene.apply(&bundle(4, 4, vec![EntityState { entity_id: 2, cell_x: 3, cell_z: 3 }])),
            Ok(SceneApply::IgnoredStale)
        );
        assert_eq!(scene, before);
    }

    #[test]
    fn inconsistent_tick_and_impossible_watermark_do_not_mutate_scene() {
        let mut scene = seeded_scene();
        let before = scene.clone();
        let mut mismatched = bundle(5, 5, Vec::new());
        mismatched.summary.tick = 6;

        assert!(matches!(scene.apply(&mismatched), Err(SceneApplyError::SummaryTickMismatch { .. })));
        assert_eq!(scene, before);
        assert!(matches!(scene.apply(&bundle(5, 6, Vec::new())), Err(SceneApplyError::ImpossibleWatermark { .. })));
        assert_eq!(scene, before);
    }

    #[test]
    fn duplicate_id_and_cell_center_overflow_do_not_partially_commit() {
        let mut scene = seeded_scene();
        let before = scene.clone();
        let duplicate = bundle(
            5,
            5,
            vec![
                EntityState { entity_id: 2, cell_x: 1, cell_z: 1 },
                EntityState { entity_id: 2, cell_x: 2, cell_z: 2 },
            ],
        );

        assert_eq!(scene.apply(&duplicate), Err(SceneApplyError::DuplicateEntity(EntityId { value: 2 })));
        assert_eq!(scene, before);
        let overflowing = bundle(5, 5, vec![EntityState { entity_id: 3, cell_x: i32::MAX, cell_z: 0 }]);
        assert!(matches!(scene.apply(&overflowing), Err(SceneApplyError::CellCenterOverflow { .. })));
        assert_eq!(scene, before);
    }

    #[test]
    fn oversized_summary_does_not_replace_the_scene() {
        let mut scene = seeded_scene();
        let before = scene.clone();
        let entities = vec![EntityState { entity_id: 2, cell_x: 1, cell_z: 1 }; MAX_RENDER_ENTITIES + 1];

        assert_eq!(
            scene.apply(&bundle(5, 5, entities)),
            Err(SceneApplyError::TooManyEntities { count: MAX_RENDER_ENTITIES + 1, maximum: MAX_RENDER_ENTITIES })
        );
        assert_eq!(scene, before);
    }

    #[test]
    fn newer_summary_replaces_the_whole_authoritative_scene() {
        let mut scene = seeded_scene();

        assert_eq!(
            scene.apply(&bundle(5, 5, vec![EntityState { entity_id: 2, cell_x: -2, cell_z: 3 }])),
            Ok(SceneApply::Committed)
        );
        assert_eq!(scene.tick, Some(5));
        assert_eq!(scene.superseded_through, 5);
        assert_eq!(
            scene.entities,
            BTreeMap::from([(EntityId { value: 2 }, WorldPoint { x_octimeters: -384, z_octimeters: 896 })])
        );
    }

    #[test]
    fn retained_session_admission_requires_exact_mailbox_name_and_peer() {
        let retained =
            ConnectedSession { name: String::from("conn-7"), id: MailboxId(41), peer: String::from("127.0.0.1:7777") };

        assert!(retained.admits(Some(MailboxId(41)), "conn-7", "127.0.0.1:7777"));
        assert!(!retained.admits(Some(MailboxId(42)), "conn-7", "127.0.0.1:7777"));
        assert!(!retained.admits(Some(MailboxId(41)), "conn-8", "127.0.0.1:7777"));
        assert!(!retained.admits(Some(MailboxId(41)), "conn-7", "127.0.0.1:8888"));
        assert!(!retained.admits(None, "conn-7", "127.0.0.1:7777"));
    }

    #[test]
    fn session_mail_is_admitted_only_after_connect_succeeds() {
        assert!(!ConnectionPhase::Connecting.admits_session_mail());
        assert!(ConnectionPhase::AwaitingHelloAck.admits_session_mail());
        assert!(ConnectionPhase::Live.admits_session_mail());
        assert!(!ConnectionPhase::Closed.admits_session_mail());
    }

    #[test]
    fn held_directions_emit_only_one_unopposed_cardinal_direction() {
        let mut held = HeldDirections::default();
        held.set(keycode::KEY_W, true);
        assert_eq!(held.resolved(), Some(MoveDirection::North));
        held.set(keycode::KEY_S, true);
        assert_eq!(held.resolved(), None);
        held.set(keycode::KEY_S, false);
        held.set(keycode::KEY_D, true);
        assert_eq!(held.resolved(), None, "diagonal input has no single cardinal resolution");
    }
}
