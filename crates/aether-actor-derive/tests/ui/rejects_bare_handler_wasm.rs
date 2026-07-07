//! Issue #2607 (ADR-0134): bare `#[handler]` on a mail-variant handler is a
//! pointed compile error — the reply class is no longer defaulted to
//! `Single`. Naming all three accepted spellings teaches the fix at the
//! error site.
//!
//! The attribute below carries an intentional inner space (`#[handler ]`)
//! rather than the canonical `#[handler]` spelling: syn parses it as the
//! same classless `Meta::Path` (whitespace inside the brackets carries no
//! token-stream meaning), so the macro sees exactly the bare form under
//! test — but written this way the fixture doesn't itself match the
//! tree-wide `git grep -P '^\s*#\[handler\]'` migration sweep this issue's
//! done-criterion runs, which would otherwise misreport this deliberately
//! unmigrated negative fixture as a leftover production site.

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

struct BareHandler;

#[actor]
impl aether_actor::WasmActor for BareHandler {
    const NAMESPACE: &'static str = "bare_handler";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError>
    {
        Ok(BareHandler)
    }

    #[handler ]
    fn on_ping(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, _ping: Ping) {}
}

fn main() {}
