//! The chassis-root door: every root is minted from the engine's one counter,
//! and a push that names a claimed inbox gets `R`'s reply there, joined to
//! the root it pushed.

use super::support::{DrivenTestChassis, StubLog};
use crate::actor::native::Dispatch;
use crate::actor::native::ctx::NativeCtx;
use crate::chassis::builder::{Builder, DriverCapability, DriverCtx, DriverRunning, RootPusher, RunError};
use crate::chassis::inbox::SettlingInbox;
use crate::mail::KindId;
use crate::testing::{TestChassis, bare_substrate};
use crate::{BootError, NativeActor, NativeInitCtx};
use aether_actor::{Addressable, HandlesKind, OutboundReply};
use aether_data::Kind;
use std::time::Duration;

pod_kind!(RootPing { tag: u32 }, "test.root_pusher.ping", 0xA1B2_C3D4_E5F6_0004);
pod_kind!(RootPong { tag: u32 }, "test.root_pusher.pong", 0xA1B2_C3D4_E5F6_0005);

/// Every chassis-root sender draws from the one counter on the engine's
/// `Mailer`: two doors to the same actor and the embedder's tracked send
/// never hand back the same root.
#[test]
fn two_senders_never_mint_the_same_root() {
    let (registry, mailer) = bare_substrate();
    let chassis = Builder::<TestChassis>::new(registry, mailer)
        .with_actor::<StubLog>(())
        .build_passive()
        .expect("the stub cap boots");
    let ping = RootPing { tag: 1 };

    let first = chassis.root_pusher::<StubLog>().push_root(&ping, None);
    let (tracked, _settled) =
        chassis.send_tracked(chassis.actor_ref::<StubLog>().erase(), RootPing::ID, ping.encode_into_bytes(), None);
    let second = chassis.root_pusher::<StubLog>().push_root(&ping, None);

    // Tripwire: any sender that keeps its own counter mints correlation 1
    // again and collides with the first push. The roots are minted, not
    // literals, so this drifts if the mint logic changes.
    assert_ne!(first, tracked, "a door and the tracked send share one counter");
    assert_ne!(first, second, "two doors share one counter");
    assert_ne!(tracked, second, "the tracked send and a later door share one counter");

    drop(chassis);
}

/// A root cap that answers each `RootPing` with a `RootPong` carrying the
/// same tag, through the reply target the push stamped.
struct EchoCap;

impl Addressable for EchoCap {
    const NAMESPACE: &'static str = "test.root_pusher.echo";
    type Resolver = aether_actor::One;
}

impl aether_actor::Root for EchoCap {}

impl HandlesKind<RootPing> for EchoCap {}

impl aether_actor::Lifecycle<Self> for EchoCap {
    type Config = ();
    type Params = ();
    type InitError = BootError;
    type InitCtx<'a> = NativeInitCtx<'a>;
    type Ctx<'a> = NativeCtx<'a, Self>;

    fn init((): (), _params: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }
}

impl NativeActor for EchoCap {
    type State = Self;
}

impl Dispatch<Self> for EchoCap {
    fn dispatch(
        _state: &mut Self,
        ctx: &mut NativeCtx<'_, Self, crate::Manual>,
        kind: KindId,
        payload: &[u8],
    ) -> Option<()> {
        if kind != RootPing::ID {
            return None;
        }
        let RootPing { tag } = RootPing::decode_from_bytes(payload)?;
        ctx.reply(&RootPong { tag });
        Some(())
    }
}

/// Test driver: takes a door to `EchoCap` and claims a reply inbox at boot,
/// then pushes one ping and asserts the pong lands in that inbox, rooted at
/// the push.
struct EchoDriver;

struct EchoDriverRunning {
    echo: RootPusher<EchoCap>,
    inbox: SettlingInbox,
}

impl DriverCapability for EchoDriver {
    type Running = EchoDriverRunning;

    fn boot(self, ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError> {
        let echo = ctx.root_pusher::<EchoCap>();
        let inbox = ctx.claim_mailbox("test.root_pusher.reply")?.inbox;
        Ok(EchoDriverRunning { echo, inbox })
    }
}

impl DriverRunning for EchoDriverRunning {
    fn run(self: Box<Self>) -> Result<(), RunError> {
        let root = self.echo.push_root(&RootPing { tag: 7 }, Some(&self.inbox));

        let reply =
            self.inbox.recv_timeout(Duration::from_secs(5)).expect("the echo's reply lands in the claimed inbox");
        assert_eq!(reply.kind(), RootPong::ID);
        assert_eq!(RootPong::decode_from_bytes(reply.payload()).map(|pong| pong.tag), Some(7));
        assert_eq!(reply.root(), Some(root), "the reply joins the root the door pushed");
        Ok(())
    }
}

/// `push_root` with a claimed inbox routes `R`'s reply into it, on the chain
/// of the root it returned. A dropped or misaddressed reply target, or an
/// unrooted push, wedges the desktop frame loop that waits on this reply,
/// which no CI run drives.
#[test]
fn root_pusher_reply_lands_in_the_claimed_inbox() {
    let (registry, mailer) = bare_substrate();
    let chassis = Builder::<DrivenTestChassis<EchoDriver>>::new(registry, mailer)
        .with_actor::<EchoCap>(())
        .driver(EchoDriver)
        .build()
        .expect("build succeeds");

    chassis.run().expect("the echo driver runs");
}
