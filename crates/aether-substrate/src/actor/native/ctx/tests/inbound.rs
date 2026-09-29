//! The request-context half of the inbound frame: a reply's correlation
//! recovers the typed context the request stored, exactly once, and a `Held`
//! the context carries comes back live (ADR-0243 §4).

use aether_data::wire::{self, HeldClaim, HeldLedger, LedgerEncoder};
use aether_data::{Kind, KindId};

use crate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use crate::chassis::error::BootError;
use crate::testing::{PumpedDriver, bare_substrate, boot_test_chassis_with};

use super::support::{
    Bouncer, HeldContext, HeldRig, HoldReq, LedgerRead, NativeRequestContext, ParkReq, Poke, Poked, TestReply,
};

/// Asks a [`Recaller`] to send [`Bouncer`] a [`Poke`] that stores a context.
#[aether_data::kind(name = "test.native_ctx.recall", copy)]
struct Recall {
    value: u32,
}

/// A pumped root that stores a context with each request it sends and takes
/// it twice from the reply's turn.
#[derive(Default)]
struct Recaller {
    /// What the reply turn's first and second takes returned.
    taken: Option<(Option<NativeRequestContext>, Option<NativeRequestContext>)>,
}

#[aether_actor::actor(singleton, root, depends(Bouncer))]
impl NativeActor for Recaller {
    const NAMESPACE: &'static str = "test.native_ctx.recaller";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self::default())
    }

    #[handler::single]
    fn on_recall(&mut self, ctx: &mut NativeCtx<'_>, recall: Recall) {
        let _ = self;
        let _request = ctx.send_with_context::<Bouncer>(&Poke, NativeRequestContext { value: recall.value });
    }

    #[handler::single]
    fn on_poked(&mut self, ctx: &mut NativeCtx<'_>, _poked: Poked) {
        self.taken = Some((ctx.take_context::<NativeRequestContext>(), ctx.take_context::<NativeRequestContext>()));
    }
}

/// Catches a take keyed by anything but the reply's correlation, or one that
/// leaves the context behind: the reply turn recovers the stored context,
/// and a second take finds nothing.
#[test]
fn native_ctx_take_context_consumes_stored_reply_context() {
    let (registry, mailer) = bare_substrate();
    let mut driver =
        PumpedDriver::<Recaller>::boot(boot_test_chassis_with::<Bouncer>(&registry, &mailer, (), ()), (), ());
    let recaller = driver.chassis().actor_ref::<Recaller>();

    driver.send_and_settle(recaller, &Recall { value: 9 }, None);

    assert_eq!(
        driver.read_state(|recaller| recaller.taken.clone()).flatten(),
        Some((Some(NativeRequestContext { value: 9 }), None)),
    );
}

/// Catches a `Held` drop that fires when its context parks, and a claim that
/// fails to rebuild the weak ledger link: the stored debt must drop silently
/// and stay parked, and the taken one must answer the caller the hold
/// captured, from the later turn the reply runs, echoing that caller's
/// correlation.
#[test]
fn parked_context_drops_silently_and_take_answers_the_original_caller() {
    let mut rig = HeldRig::boot();
    let (caller, replies) = rig.caller("test.held_context.caller");

    let root = rig.push(&ParkReq { tag: 4 }, Some(caller));
    rig.driver.settle(&[root]);

    let (parked, taken) = rig.driver.read_state(|host| (host.parked, host.taken)).expect("the host is live");
    assert_eq!(
        parked,
        Some(LedgerRead { entry: Some("parked"), held_open: 1 }),
        "storing the context parked the entry, which keeps the caller's chain open, and the stored ticket dropped silently",
    );
    assert_eq!(taken, Some((4, Some("held"))), "the take claims the entry back beside the context's other fields");
    assert_eq!(rig.held_open(root), 0, "answering the taken debt releases the hold");

    let reply = replies.try_recv().expect("the answer reaches the original caller");
    assert_eq!(reply.sender.correlation_id, 77, "the captured correlation is echoed, not the reply turn's");
    assert_eq!(TestReply::decode_from_bytes(reply.payload.bytes()), Some(TestReply { value: 4 }));
}

/// A ledger that accepts every park, so a test can read the bytes the
/// request-context table's encoder writes without storing them.
struct AcceptAll;

impl HeldLedger for AcceptAll {
    fn park(&mut self, _ticket: u64, _reply: KindId) -> Result<(), wire::Error> {
        Ok(())
    }

    fn claim(&mut self, _ticket: u64, reply: KindId) -> Result<HeldClaim, wire::Error> {
        Err(wire::Error::HeldUngranted { reply })
    }
}

/// Catches a stray codec path that defuses or claims a debt: a plain encode
/// refuses and leaves the entry held, and a plain decode of the stored bytes
/// refuses and leaves the entry parked for its context's take.
#[test]
fn held_encode_and_decode_outside_the_table_refuse() {
    let mut rig = HeldRig::boot();
    let root = rig.push(&HoldReq, None);
    rig.driver.pump_until("the host holds the request", |host| host.held.is_some());
    let held = rig.driver.host_turn(|host, _ctx| host.held.take()).flatten().expect("the host holds a ticket");
    let id = held.dispatch_id();
    let context = HeldContext { held, tag: 3 };
    let entry = |rig: &mut HeldRig| rig.driver.host_turn(|_host, ctx| ctx.binding.dispatch_state_of(id)).flatten();

    assert_eq!(
        context.encode_with(&mut Vec::new()),
        Err(wire::Error::HeldUngranted { reply: TestReply::ID }),
        "a plain buffer grants no ledger",
    );
    assert_eq!(entry(&mut rig), Some("held"), "a refused encode leaves the entry held");

    let mut ledger = AcceptAll;
    let mut encoder = LedgerEncoder::new(&mut ledger);
    context.encode_with(&mut encoder).expect("a granting encoder writes the ticket");
    let bytes = encoder.into_bytes();
    rig.driver
        .host_turn(|_host, ctx| {
            let _request = ctx.send_with_context::<Bouncer>(&Poke, context);
        })
        .expect("the host is live");
    assert_eq!(entry(&mut rig), Some("parked"), "storing the context parks its entry");

    assert!(HeldContext::decode_from_bytes(&bytes).is_none(), "a plain decode grants no claim");
    assert_eq!(entry(&mut rig), Some("parked"), "a refused decode leaves the entry parked");

    rig.driver.pump_until("the reply takes the stored context", |host| host.taken.is_some());
    let taken = rig.driver.read_state(|host| host.taken).flatten();
    assert_eq!(taken, Some((3, Some("held"))), "the table's take claims the parked entry back to held");
    rig.driver.settle(&[root]);
}
