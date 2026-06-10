//! `loco-motion` — the standalone game build (#1518).
//!
//! A desktop substrate that auto-loads the game component embedded at build
//! time, so the binary runs hub-less and double-click-to-play. No MCP, no hub,
//! no netcode. `cargo xtask bundle` builds the component wasm and embeds it
//! (see the crate `build.rs`); a plain build embeds an empty placeholder.

use aether_substrate::Chassis;
use aether_substrate_bundle::desktop::{AutoloadComponent, DesktopChassis, DesktopEnv};

/// The game component wasm, embedded at build time. `build.rs` stages it into
/// `OUT_DIR/game.wasm` from `AETHER_GAME_WASM` (the bundle flow) or an empty
/// placeholder (a normal build).
const GAME_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/game.wasm"));

fn main() -> anyhow::Result<()> {
    // Resolve the chassis env as the desktop bin does — so an injected
    // `AETHER_RPC_PORT` (e.g. when the hub spawns this for a capture) still
    // wires up — then override the title and queue the embedded component.
    let mut env = DesktopEnv::from_env()?;
    "loco-motion".clone_into(&mut env.boot_title);
    env.autoload = vec![AutoloadComponent {
        wasm: GAME_WASM.to_vec(),
        config: Vec::new(),
        name: None,
        export: None,
    }];
    let chassis = DesktopChassis::build(env)?;
    chassis.run()?;
    Ok(())
}
