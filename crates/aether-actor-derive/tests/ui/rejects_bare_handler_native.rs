//! Issue #2607 (ADR-0134): bare `#[handler]` on a mail-variant handler is a
//! pointed compile error on the native path too — the reply class is no
//! longer defaulted to `Single`. See `rejects_bare_handler_wasm.rs` for why
//! the attribute below carries an intentional inner space (`#[handler ]`):
//! it is the same classless `Meta::Path` syn sees for the canonical
//! `#[handler]` spelling, but doesn't itself match the tree-wide
//! `git grep -P '^\s*#\[handler\]'` migration sweep this issue's
//! done-criterion runs.

use aether_actor::actor;

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.ping")]
struct Ping {
    seq: u32,
}

#[allow(dead_code)]
struct BareHandler;

#[actor]
impl aether_substrate::actor::native::NativeActor for BareHandler {
    type Config = ();

    const NAMESPACE: &'static str = "bare_handler";

    fn init(
        _config: (),
        _ctx: &mut aether_substrate::actor::native::NativeInitCtx<'_>,
    ) -> Result<Self, aether_actor::ActorInitError> {
        unimplemented!()
    }

    #[handler ]
    fn on_ping(
        &mut self,
        _ctx: &mut aether_substrate::actor::native::NativeCtx<'_>,
        _ping: Ping,
    ) {
    }
}

fn main() {}
