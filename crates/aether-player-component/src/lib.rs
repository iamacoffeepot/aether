//! Player component. A world-space position + a small triangular body
//! that renders itself and publishes `CameraTopdownSet` each tick so a
//! topdown-mode camera follows the player on the world `xy` plane.
//!
//! Two motion modes, swapped at runtime via `PlayerSetMode`:
//!
//! - **Continuous** (default). WASD / arrow keys drive per-tick
//!   velocity (`KEY_SPEED` world units/tick while held); release
//!   clears the flag and velocity falls back to whatever
//!   `PlayerSetVelocity` last set. The body is free-floating — it
//!   knows nothing about walls or world topology. Good for free-look
//!   testing and non-grid scenes.
//! - **Tile-step**. WASD press emits a `PlayerRequestStep { dx, dy }`
//!   to a mailbox named `"world"` — the scene's world authority —
//!   with integer cell deltas (W: `(0, +1)`, S: `(0, -1)`, D:
//!   `(+1, 0)`, A: `(-1, 0)`). The player does **not** move itself;
//!   it waits for `PlayerStepResult` back and overwrites its position
//!   from the authority's reply. Releases are ignored. Continuous
//!   velocity is suppressed. This is the shape sokoban-style grid
//!   games use.
//!
//! Scripted control surfaces (`PlayerSetPosition`, `PlayerSetVelocity`)
//! remain available in both modes — useful for smoke tests over MCP.
//! In tile-step mode, `PlayerSetVelocity` has no visible effect
//! because velocity is not applied.
//!
//! Sink dependencies: `"aether.sink.render"` (substrate), `"camera"`
//! (the multi-camera component, addressed by its conventional load
//! name), `"world"` (the world authority in tile-step mode). Missing
//! sinks surface as `UnresolvedMail` diagnostics but don't crash the
//! player — the corresponding emissions just go to the abyss.

use aether_component::{Component, Ctx, InitCtx, Sink, handlers};
use aether_kinds::{
    CameraTopdownSet, DrawTriangle, Key, KeyRelease, PlayerRequestStep, PlayerSetMode,
    PlayerSetPosition, PlayerSetVelocity, PlayerStepResult, Tick, TopdownParams, Vertex, keycode,
};

const PLAYER_HALF: f32 = 0.25;
const PLAYER_R: f32 = 1.0;
const PLAYER_G: f32 = 0.3;
const PLAYER_B: f32 = 0.9;
/// Per-tick continuous-mode speed in world units.
const KEY_SPEED: f32 = 0.05;
/// World-z for the player body. Larger than floor/backdrop z so the
/// desktop substrate's `LessEqual` depth test draws the player on top
/// of overlapping grid geometry. See `crates/aether-substrate-desktop/
/// src/render.rs` for the z-convention.
const PLAYER_Z: f32 = 0.1;

/// `PlayerSetMode.mode` values. Kept as raw `u32` on the wire so the
/// kind stays cast-tier; these constants give the component a readable
/// match surface.
const MODE_CONTINUOUS: u32 = 0;
const MODE_TILE_STEP: u32 = 1;

pub struct Player {
    render: Sink<DrawTriangle>,
    camera_follow: Sink<CameraTopdownSet>,
    /// Cached camera-follow envelope. The `name` field is set once at
    /// init and reused every tick to avoid re-allocating the String;
    /// only `params.center` is mutated per frame.
    follow_msg: CameraTopdownSet,
    world: Sink<PlayerRequestStep>,
    pos_x: f32,
    pos_y: f32,
    base_vx: f32,
    base_vy: f32,
    moving_up: bool,
    moving_down: bool,
    moving_left: bool,
    moving_right: bool,
    /// Active motion model. See `MODE_*` constants.
    mode: u32,
}

impl Player {
    fn any_key_held(&self) -> bool {
        self.moving_up || self.moving_down || self.moving_left || self.moving_right
    }

    /// Translate a mapped keycode into a direction flag update.
    /// Continuous-mode path — unrelated to tile-step motion.
    fn apply_key(&mut self, code: u32, pressed: bool) {
        match code {
            keycode::KEY_W | keycode::KEY_UP => self.moving_up = pressed,
            keycode::KEY_S | keycode::KEY_DOWN => self.moving_down = pressed,
            keycode::KEY_A | keycode::KEY_LEFT => self.moving_left = pressed,
            keycode::KEY_D | keycode::KEY_RIGHT => self.moving_right = pressed,
            _ => {}
        }
    }

    /// Map a mapped keycode to a tile-step delta. Returns `None` for
    /// keys that aren't bound to movement.
    fn step_delta(code: u32) -> Option<(i32, i32)> {
        match code {
            keycode::KEY_W | keycode::KEY_UP => Some((0, 1)),
            keycode::KEY_S | keycode::KEY_DOWN => Some((0, -1)),
            keycode::KEY_D | keycode::KEY_RIGHT => Some((1, 0)),
            keycode::KEY_A | keycode::KEY_LEFT => Some((-1, 0)),
            _ => None,
        }
    }
}

/// Player body with runtime-switchable motion model.
///
/// # Agent
/// Load alongside the multi-camera component (`aether-camera-component`),
/// loaded as `"camera"` — the player publishes `CameraTopdownSet
/// { name: "main", params: { center: [px, py] } }` each tick to follow
/// the player on the world `xy` plane, so the camera component should
/// have a topdown-mode camera named `"main"` (the bootstrap default
/// is orbit, so send `set_mode { name: "main", mode: Topdown(..) }`
/// after load). For grid-game shapes, also load a world authority
/// (e.g. `aether-demo-sokoban`, named `"world"`) and send
/// `PlayerSetMode { mode: 1 }` to switch to tile-step motion.
///
/// **Control surface**
///
/// - `PlayerSetMode { mode }` — `0` continuous, `1` tile-step.
/// - `PlayerSetPosition { x, y }` — teleport (both modes).
/// - `PlayerSetVelocity { vx, vy }` — baseline velocity, applied each
///   tick in continuous mode when no key is held. No-op visually in
///   tile-step mode.
///
/// **Keyboard** (requires window focus for live keys; for MCP smoke
/// tests send `Key` / `KeyRelease` directly to the player):
///
/// - Continuous mode: hold to move, release to stop.
/// - Tile-step mode: press to request one cell step via the world
///   authority; release does nothing. The world's reply updates the
///   player's position, so a blocked step leaves the player where it
///   was.
#[handlers]
impl Component for Player {
    fn init(ctx: &mut InitCtx<'_>) -> Self {
        Player {
            render: ctx.resolve_sink::<DrawTriangle>("aether.sink.render"),
            camera_follow: ctx.resolve_sink::<CameraTopdownSet>("camera"),
            follow_msg: CameraTopdownSet {
                name: "main".to_owned(),
                params: TopdownParams {
                    center: Some([0.0, 0.0]),
                    extent: None,
                },
            },
            world: ctx.resolve_sink::<PlayerRequestStep>("world"),
            pos_x: 0.0,
            pos_y: 0.0,
            base_vx: 0.0,
            base_vy: 0.0,
            moving_up: false,
            moving_down: false,
            moving_left: false,
            moving_right: false,
            mode: MODE_CONTINUOUS,
        }
    }

    /// Advance position (continuous mode only), draw the body, push a
    /// camera follow target.
    ///
    /// # Agent
    /// Tick-driven; not useful to send manually.
    #[handler]
    fn on_tick(&mut self, ctx: &mut Ctx<'_>, _tick: Tick) {
        if self.mode == MODE_CONTINUOUS {
            let (vx, vy) = if self.any_key_held() {
                let dx = (self.moving_right as i32 - self.moving_left as i32) as f32;
                let dy = (self.moving_up as i32 - self.moving_down as i32) as f32;
                (dx * KEY_SPEED, dy * KEY_SPEED)
            } else {
                (self.base_vx, self.base_vy)
            };
            self.pos_x += vx;
            self.pos_y += vy;
        }

        let body = DrawTriangle {
            verts: [
                Vertex {
                    x: self.pos_x,
                    y: self.pos_y + PLAYER_HALF,
                    z: PLAYER_Z,
                    r: PLAYER_R,
                    g: PLAYER_G,
                    b: PLAYER_B,
                },
                Vertex {
                    x: self.pos_x - PLAYER_HALF,
                    y: self.pos_y - PLAYER_HALF,
                    z: PLAYER_Z,
                    r: PLAYER_R,
                    g: PLAYER_G,
                    b: PLAYER_B,
                },
                Vertex {
                    x: self.pos_x + PLAYER_HALF,
                    y: self.pos_y - PLAYER_HALF,
                    z: PLAYER_Z,
                    r: PLAYER_R,
                    g: PLAYER_G,
                    b: PLAYER_B,
                },
            ],
        };
        ctx.send(&self.render, &body);

        self.follow_msg.params.center = Some([self.pos_x, self.pos_y]);
        ctx.send(&self.camera_follow, &self.follow_msg);
    }

    /// Teleport. Honors both modes — in tile-step mode this is the
    /// "force position" override (normally the world authority is the
    /// only one driving position changes).
    #[handler]
    fn on_set_position(&mut self, _ctx: &mut Ctx<'_>, msg: PlayerSetPosition) {
        self.pos_x = msg.x;
        self.pos_y = msg.y;
    }

    /// Set continuous-mode baseline velocity. No-op visually in
    /// tile-step mode (velocity isn't applied to position there).
    #[handler]
    fn on_set_velocity(&mut self, _ctx: &mut Ctx<'_>, msg: PlayerSetVelocity) {
        self.base_vx = msg.vx;
        self.base_vy = msg.vy;
    }

    /// Switch motion mode. Clears direction flags on transition to
    /// avoid stale "still held" state leaking across modes.
    ///
    /// # Agent
    /// `0` = continuous (default, free-float), `1` = tile-step (grid
    /// games with a `"world"` authority). Other values are ignored.
    #[handler]
    fn on_set_mode(&mut self, _ctx: &mut Ctx<'_>, msg: PlayerSetMode) {
        if msg.mode == MODE_CONTINUOUS || msg.mode == MODE_TILE_STEP {
            self.mode = msg.mode;
            self.moving_up = false;
            self.moving_down = false;
            self.moving_left = false;
            self.moving_right = false;
        }
    }

    /// Handle a key-press mapped to movement. Dispatches by mode:
    /// continuous sets a direction flag, tile-step fires off one
    /// `PlayerRequestStep` to the world authority.
    ///
    /// # Agent
    /// Publish-subscribe; the substrate delivers this on every mapped
    /// press. In tile-step mode, hold doesn't auto-repeat — one press
    /// = one step request. Send a fresh `Key` for each step.
    #[handler]
    fn on_key(&mut self, ctx: &mut Ctx<'_>, key: Key) {
        if self.mode == MODE_TILE_STEP {
            if let Some((dx, dy)) = Self::step_delta(key.code) {
                ctx.send(&self.world, &PlayerRequestStep { dx, dy });
            }
        } else {
            self.apply_key(key.code, true);
        }
    }

    /// Key-release. Continuous mode clears the direction flag;
    /// tile-step mode ignores the event (each step is a fresh press).
    #[handler]
    fn on_key_release(&mut self, _ctx: &mut Ctx<'_>, key: KeyRelease) {
        if self.mode == MODE_CONTINUOUS {
            self.apply_key(key.code, false);
        }
    }

    /// World-authority reply to a `PlayerRequestStep`. The authority
    /// is the source of truth on where the player ended up — rejected
    /// steps still carry a position (the unchanged original), so the
    /// player overwrites its position unconditionally.
    ///
    /// # Agent
    /// Replied by whichever component is loaded as `"world"`. No need
    /// to send this manually unless simulating a world.
    #[handler]
    fn on_step_result(&mut self, _ctx: &mut Ctx<'_>, result: PlayerStepResult) {
        self.pos_x = result.new_x;
        self.pos_y = result.new_y;
    }
}

aether_component::export!(Player);
