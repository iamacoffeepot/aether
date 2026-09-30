//! `testing::PumpedDriver` waits the way a pumped chassis driver waits: it
//! drains only on a mail wake and returns from `settle` only once every
//! awaited root has settled. The pumped actor here counts what it dispatches,
//! and its pooled peer answers it, so a wait that returns early reads a count
//! the chain has not reached yet.

use aether_substrate::testing::{PumpedDriver, boot_test_chassis_with, fresh_substrate};
use aether_substrate::{BootError, NativeActor, NativeCtx, NativeInitCtx};

/// Adds `n` to the counter's bumps.
#[aether_data::kind(name = "test.pumped_driver.bump", copy)]
struct Bump {
    n: u32,
}

/// Asks the counter to bounce a mail off its pooled peer.
#[aether_data::kind(name = "test.pumped_driver.relay")]
struct Relay;

/// The counter's request to its pooled peer.
#[aether_data::kind(name = "test.pumped_driver.bounce")]
struct Bounce;

/// The peer's answer, which lands back on the counter's inbox, numbered by
/// how many bounces the peer has answered.
#[aether_data::kind(name = "test.pumped_driver.bounced", copy)]
struct Bounced {
    answered: u32,
}

/// A pooled actor that answers every [`Bounce`] with a [`Bounced`].
struct Echo {
    answered: u32,
}

#[aether_actor::actor(singleton, root)]
impl NativeActor for Echo {
    const NAMESPACE: &'static str = "test.pumped_driver.echo";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { answered: 0 })
    }

    #[aether_actor::handler::request]
    fn on_bounce(&mut self, _ctx: &mut NativeCtx<'_>, _bounce: Bounce) -> Bounced {
        self.answered += 1;
        Bounced { answered: self.answered }
    }
}

/// The pumped actor under the driver: it counts its bumps and its relays,
/// and keeps the number of the last answer its relays got back.
struct Counter {
    bumps: u32,
    relayed: u32,
    bounced: u32,
}

#[aether_actor::actor(singleton, root, depends(Echo))]
impl NativeActor for Counter {
    const NAMESPACE: &'static str = "test.pumped_driver.counter";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { bumps: 0, relayed: 0, bounced: 0 })
    }

    #[aether_actor::handler::tell]
    fn on_bump(&mut self, _ctx: &mut NativeCtx<'_>, bump: Bump) {
        self.bumps += bump.n;
    }

    #[aether_actor::handler::tell]
    fn on_relay(&mut self, ctx: &mut NativeCtx<'_>, _relay: Relay) {
        self.relayed += 1;
        ctx.send::<Echo>(&Bounce);
    }

    #[aether_actor::handler::response]
    fn on_bounced(&mut self, _ctx: &mut NativeCtx<'_>, bounced: Bounced) {
        self.bounced = bounced.answered;
    }
}

/// Boot [`Counter`] pumped under a driver, beside a pooled [`Echo`].
fn driver() -> PumpedDriver<Counter> {
    let (registry, mailer) = fresh_substrate();

    PumpedDriver::boot(boot_test_chassis_with::<Echo>(&registry, &mailer, (), ()), (), ())
}

/// `settle` over two roots returns only when both have settled. The bump's
/// root settles inside the first drain, before the relay's turn sends its
/// bounce, so its `Settled` wake is queued ahead of the answer's mail wake,
/// and the relay's root settles only once a later drain dispatches that
/// answer. A wait that returned on the first `Settled` would read no answer.
#[test]
fn settle_returns_only_once_every_root_has_settled() {
    let mut driver = driver();
    let counter = driver.chassis().actor_ref::<Counter>();

    let bump = driver.send_tracked(counter, &Bump { n: 3 }, None);
    let relay = driver.send_tracked(counter, &Relay, None);
    driver.settle(&[bump, relay]);

    assert_eq!(driver.read_state(|counter| (counter.bumps, counter.relayed, counter.bounced)), Some((3, 1, 1)));
}

/// `pump_until` wakes on mail no chain the test holds carries: a detached
/// self-wake lands on the pumped inbox and is dispatched by the drain its
/// mail wake triggers.
#[test]
fn pump_until_drains_a_detached_mail_on_its_wake() {
    let mut driver = driver();
    let wake = driver.host_turn(|_, ctx| ctx.self_wake::<Bump>()).expect("the counter is live");

    wake.wake(&Bump { n: 5 });

    driver.pump_until("the detached bump", |counter| counter.bumps == 5);
}
