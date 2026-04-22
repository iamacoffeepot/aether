//! Minimal player component: a world-space position that advances by a
//! per-tick velocity, emits a small triangle at its location to the
//! substrate's `render` sink, and pushes `TopdownSetCenter` to the
//! top-down camera each tick so the camera follows.
//!
//! Two control surfaces feed velocity:
//!
//! - **Direct MCP**: `PlayerSetPosition { x, y }` / `PlayerSetVelocity
//!   { vx, vy }`. Useful for scripted movement and smoke tests.
//! - **Keyboard**: WASD or arrow keys from the substrate's `aether.key`
//!   / `aether.key_release` streams. While any direction key is held,
//!   the derived velocity overrides whatever `PlayerSetVelocity` last
//!   set; when all directional keys are released, velocity falls back
//!   to the stored baseline.
//!
//! The camera sink is hardcoded to the mailbox name `"topdown"` — load
//! the top-down camera example under that name for follow to work. A
//! different name (or no camera loaded) makes the `TopdownSetCenter`
//! mail unresolved, which surfaces as an `UnresolvedMail` diagnostic
//! but is otherwise inert.

use aether_component::{Component, Ctx, InitCtx, Sink, handlers};
use aether_kinds::{
    DrawTriangle, Key, KeyRelease, PlayerSetPosition, PlayerSetVelocity, Tick, TopdownSetCenter,
    Vertex, keycode,
};

/// Half-extent of the player's triangular body in world units. The
/// triangle is an isoceles apex-up around the player position.
const PLAYER_HALF: f32 = 0.25;
/// RGB color of the triangle body. Bright magenta — reads distinctly
/// against the sokoban grid and any hello-component triangle.
const PLAYER_R: f32 = 1.0;
const PLAYER_G: f32 = 0.3;
const PLAYER_B: f32 = 0.9;
/// Per-tick movement speed when a WASD/arrow key is held, in world
/// units. 0.05 at 60Hz ≈ 3 units/second — slow enough to watch the
/// camera track, fast enough not to feel sluggish.
const KEY_SPEED: f32 = 0.05;

pub struct Player {
    render: Sink<DrawTriangle>,
    camera_follow: Sink<TopdownSetCenter>,
    pos_x: f32,
    pos_y: f32,
    // Baseline velocity set via `PlayerSetVelocity`. Applied every
    // tick when no directional key is held.
    base_vx: f32,
    base_vy: f32,
    // Per-direction key-held flags. Tick derives velocity from these
    // when any is set; diagonals fall out naturally (holding W + D
    // gives up-right).
    moving_up: bool,
    moving_down: bool,
    moving_left: bool,
    moving_right: bool,
}

impl Player {
    /// True while any WASD/arrow key is currently held. Used each tick
    /// to decide whether WASD-derived velocity overrides the stored
    /// baseline from `PlayerSetVelocity`.
    fn any_key_held(&self) -> bool {
        self.moving_up || self.moving_down || self.moving_left || self.moving_right
    }

    /// Update a direction flag when a mapped keycode arrives. Returns
    /// the same flag value that got assigned so a caller that wants
    /// to know whether the event was consumed could check, though the
    /// player doesn't today.
    fn apply_key(&mut self, code: u32, pressed: bool) {
        match code {
            keycode::KEY_W | keycode::KEY_UP => self.moving_up = pressed,
            keycode::KEY_S | keycode::KEY_DOWN => self.moving_down = pressed,
            keycode::KEY_A | keycode::KEY_LEFT => self.moving_left = pressed,
            keycode::KEY_D | keycode::KEY_RIGHT => self.moving_right = pressed,
            _ => {}
        }
    }
}

/// A player body with world-space position and per-tick velocity.
/// Draws a small apex-up triangle at its position every tick and
/// publishes `TopdownSetCenter` to a mailbox named `"topdown"` so an
/// attached top-down camera follows. Keyboard-controllable via WASD
/// or arrow keys (hold-to-move), or scripted via `PlayerSetPosition` /
/// `PlayerSetVelocity`.
///
/// # Agent
/// Load alongside `aether-camera-component`'s `topdown` example (under
/// the name `"topdown"`) and optionally `aether-demo-sokoban` for a
/// backdrop. Scripted controls:
///
/// - `PlayerSetPosition { x, y }` — teleport. Velocity is untouched;
///   send `PlayerSetVelocity { 0, 0 }` separately to stop.
/// - `PlayerSetVelocity { vx, vy }` — per-tick drift in world units.
///   `(0, 0)` stops motion. Overridden while any WASD/arrow key is
///   held; restored when all are released.
///
/// Keyboard controls require the substrate window to have focus —
/// `capture_frame` alone won't deliver keys. For MCP-only smoke tests,
/// send `Key { code: KEY_W }` / `KeyRelease { code: KEY_W }` directly
/// to the player mailbox; the flag update path is the same.
#[handlers]
impl Component for Player {
    fn init(ctx: &mut InitCtx<'_>) -> Self {
        Player {
            render: ctx.resolve_sink::<DrawTriangle>("render"),
            camera_follow: ctx.resolve_sink::<TopdownSetCenter>("topdown"),
            pos_x: 0.0,
            pos_y: 0.0,
            base_vx: 0.0,
            base_vy: 0.0,
            moving_up: false,
            moving_down: false,
            moving_left: false,
            moving_right: false,
        }
    }

    /// Advance position, draw body, and push a camera follow target.
    ///
    /// # Agent
    /// Tick-driven; not useful to send manually.
    #[handler]
    fn on_tick(&mut self, ctx: &mut Ctx<'_>, _tick: Tick) {
        let (vx, vy) = if self.any_key_held() {
            let dx = (self.moving_right as i32 - self.moving_left as i32) as f32;
            let dy = (self.moving_up as i32 - self.moving_down as i32) as f32;
            (dx * KEY_SPEED, dy * KEY_SPEED)
        } else {
            (self.base_vx, self.base_vy)
        };
        self.pos_x += vx;
        self.pos_y += vy;

        let body = DrawTriangle {
            verts: [
                Vertex {
                    x: self.pos_x,
                    y: self.pos_y + PLAYER_HALF,
                    z: 0.0,
                    r: PLAYER_R,
                    g: PLAYER_G,
                    b: PLAYER_B,
                },
                Vertex {
                    x: self.pos_x - PLAYER_HALF,
                    y: self.pos_y - PLAYER_HALF,
                    z: 0.0,
                    r: PLAYER_R,
                    g: PLAYER_G,
                    b: PLAYER_B,
                },
                Vertex {
                    x: self.pos_x + PLAYER_HALF,
                    y: self.pos_y - PLAYER_HALF,
                    z: 0.0,
                    r: PLAYER_R,
                    g: PLAYER_G,
                    b: PLAYER_B,
                },
            ],
        };
        ctx.send(&self.render, &body);

        ctx.send(
            &self.camera_follow,
            &TopdownSetCenter {
                x: self.pos_x,
                y: self.pos_y,
            },
        );
    }

    /// Teleport to a new world-space position.
    #[handler]
    fn on_set_position(&mut self, _ctx: &mut Ctx<'_>, msg: PlayerSetPosition) {
        self.pos_x = msg.x;
        self.pos_y = msg.y;
    }

    /// Replace the baseline per-tick velocity. `(0, 0)` stops.
    /// Overridden while any WASD/arrow key is held; restored when all
    /// are released.
    #[handler]
    fn on_set_velocity(&mut self, _ctx: &mut Ctx<'_>, msg: PlayerSetVelocity) {
        self.base_vx = msg.vx;
        self.base_vy = msg.vy;
    }

    /// Record a key-down for WASD/arrow keys (other keys ignored).
    ///
    /// # Agent
    /// Publish-subscribe; the substrate delivers this for every mapped
    /// press. Holding a key produces one `Key` on the initial press
    /// (winit suppresses auto-repeat) and one `KeyRelease` when
    /// released — the component tracks hold state via that pair.
    #[handler]
    fn on_key(&mut self, _ctx: &mut Ctx<'_>, key: Key) {
        self.apply_key(key.code, true);
    }

    /// Record a key-up for WASD/arrow keys (other keys ignored).
    ///
    /// # Agent
    /// Publish-subscribe; the substrate delivers this on release.
    #[handler]
    fn on_key_release(&mut self, _ctx: &mut Ctx<'_>, key: KeyRelease) {
        self.apply_key(key.code, false);
    }
}

aether_component::export!(Player);
