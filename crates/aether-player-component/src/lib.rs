//! Minimal player component: a world-space position that advances by a
//! per-tick velocity, emits a small triangle at its location to the
//! substrate's `render` sink, and pushes `TopdownSetCenter` to the
//! top-down camera each tick so the camera follows.
//!
//! Controllable via `aether.player.set_*` mail:
//!
//! - `PlayerSetPosition { x, y }` — teleport.
//! - `PlayerSetVelocity { vx, vy }` — per-tick drift in world units.
//!
//! The camera sink is hardcoded to the mailbox name `"topdown"` — load
//! the top-down camera example under that name for follow to work. A
//! different name (or no camera loaded) makes the `TopdownSetCenter`
//! mail unresolved, which surfaces as an `UnresolvedMail` diagnostic
//! but is otherwise inert.

use aether_component::{Component, Ctx, InitCtx, Sink, handlers};
use aether_kinds::{
    DrawTriangle, PlayerSetPosition, PlayerSetVelocity, Tick, TopdownSetCenter, Vertex,
};

/// Half-extent of the player's triangular body in world units. The
/// triangle is an isoceles apex-up around the player position.
const PLAYER_HALF: f32 = 0.25;
/// RGB color of the triangle body. Bright magenta — reads distinctly
/// against the sokoban grid and any hello-component triangle.
const PLAYER_R: f32 = 1.0;
const PLAYER_G: f32 = 0.3;
const PLAYER_B: f32 = 0.9;

pub struct Player {
    render: Sink<DrawTriangle>,
    camera_follow: Sink<TopdownSetCenter>,
    pos_x: f32,
    pos_y: f32,
    vel_x: f32,
    vel_y: f32,
}

/// A player body with world-space position and per-tick velocity.
/// Draws a small apex-up triangle at its position every tick and
/// publishes `TopdownSetCenter` to a mailbox named `"topdown"` so an
/// attached top-down camera follows.
///
/// # Agent
/// Load alongside `aether-camera-component`'s `topdown` example (under
/// the name `"topdown"`) and optionally `aether-demo-sokoban` for a
/// backdrop. Controls:
///
/// - `PlayerSetPosition { x, y }` — teleport. Velocity is untouched;
///   send `PlayerSetVelocity { 0, 0 }` separately to stop.
/// - `PlayerSetVelocity { vx, vy }` — per-tick drift in world units.
///   `(0, 0)` stops motion.
///
/// Use `capture_frame` between sends to verify each change.
#[handlers]
impl Component for Player {
    fn init(ctx: &mut InitCtx<'_>) -> Self {
        Player {
            render: ctx.resolve_sink::<DrawTriangle>("render"),
            camera_follow: ctx.resolve_sink::<TopdownSetCenter>("topdown"),
            pos_x: 0.0,
            pos_y: 0.0,
            vel_x: 0.0,
            vel_y: 0.0,
        }
    }

    /// Advance position, draw body, and push a camera follow target.
    ///
    /// # Agent
    /// Tick-driven; not useful to send manually.
    #[handler]
    fn on_tick(&mut self, ctx: &mut Ctx<'_>, _tick: Tick) {
        self.pos_x += self.vel_x;
        self.pos_y += self.vel_y;

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

    /// Replace per-tick velocity. `(0, 0)` stops.
    #[handler]
    fn on_set_velocity(&mut self, _ctx: &mut Ctx<'_>, msg: PlayerSetVelocity) {
        self.vel_x = msg.vx;
        self.vel_y = msg.vy;
    }
}

aether_component::export!(Player);
