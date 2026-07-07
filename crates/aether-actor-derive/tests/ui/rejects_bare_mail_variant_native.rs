//! Issue #2607 (ADR-0134): the classless `#[handler(mail)]` paren form is
//! rejected exactly like bare `#[handler]` — the `mail` variant trigger
//! carries no reply class, so it hits the same "requires an explicit reply
//! class" error naming all three accepted spellings.

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
struct BareMailVariant;

#[actor]
impl aether_substrate::actor::native::NativeActor for BareMailVariant {
    type Config = ();

    const NAMESPACE: &'static str = "bare_mail_variant";

    fn init(
        _config: (),
        _ctx: &mut aether_substrate::actor::native::NativeInitCtx<'_>,
    ) -> Result<Self, aether_actor::ActorInitError> {
        unimplemented!()
    }

    #[handler(mail)]
    fn on_ping(
        &mut self,
        _ctx: &mut aether_substrate::actor::native::NativeCtx<'_>,
        _ping: Ping,
    ) {
    }
}

fn main() {}
