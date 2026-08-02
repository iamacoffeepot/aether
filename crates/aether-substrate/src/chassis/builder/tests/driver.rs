//! The driver build path: passives boot, the driver runs and tears them down,
//! the claim-only terminal stops before Init, and a Claim-stage mailbox
//! reservation is recovered at Start.

use super::support::{DrivenTestChassis, RanDriver, StubLog};
use crate::actor::native::Dispatch;
use crate::actor::native::ctx::NativeCtx;
use crate::chassis::builder::{Builder, DriverCapability, DriverCtx, DriverRunning, RunError};
use crate::chassis::ctx::ChassisCtx;
use crate::mail::KindId;
use crate::mail::MailboxId;
use crate::mail::registry;
use crate::testing::bare_substrate;
use crate::testing::boot_authority;
use crate::{BootError, NativeActor, NativeInitCtx};
use aether_actor::Addressable;
use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

/// Driver build path: passives boot, driver runs, passives tear
/// down on chassis drop. Per-cap dispatch coverage lives in the
/// individual cap modules; this test exercises the chassis-level
/// boot + run + teardown sequence.
#[test]
fn driver_build_runs_driver_and_tears_down_passives() {
    let (registry, mailer) = bare_substrate();
    let ran = Arc::new(AtomicBool::new(false));

    let chassis = Builder::<DrivenTestChassis<RanDriver>>::new(registry, mailer)
        .with_actor::<StubLog>(())
        .driver(RanDriver { ran: Arc::clone(&ran) })
        .build()
        .expect("build succeeds");

    chassis.run().expect("driver run succeeds");
    assert!(ran.load(Ordering::SeqCst));
}

/// Test driver whose value-free ADR-0155 claim hook reserves a
/// driver-as-actor mailbox (the shape the desktop driver's
/// `aether.window` claim will take once the Env split lands). `boot` is
/// never reached by the claim-only terminal — the driver value is never
/// constructed.
struct ClaimingDriver;
struct ClaimingDriverRunning;

impl DriverCapability for ClaimingDriver {
    type Running = ClaimingDriverRunning;

    fn claim(ctx: &mut ChassisCtx<'_>) -> Result<(), BootError> {
        ctx.claim_mailbox_with_override("test.claim_only.window")?;
        Ok(())
    }

    fn boot(self, _ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError> {
        Ok(ClaimingDriverRunning)
    }
}

impl DriverRunning for ClaimingDriverRunning {
    fn run(self: Box<Self>) -> Result<(), RunError> {
        Ok(())
    }
}

/// ADR-0155 claim-only terminal: `claim_namespaces` reports exactly the
/// namespaces the three registration contributors reserve — the
/// `with_actor` chain, an inline sink registered directly on the shared
/// registry, and the driver type's value-free claim hook — and runs ONLY
/// the Claim stage, never advancing to Init (a cap's `init` side effect
/// stays unfired). The un-fired `init` is the load-bearing proof that no
/// OS resource is touched and no worker pool starts: Init is the first
/// stage that touches OS resources (ADR-0155), and Start (dispatcher
/// threads, the pool) is strictly after it in the fused boot path, so a
/// terminal that stops before Init spawns no thread by construction.
#[test]
fn claim_namespaces_reports_all_contributors_and_skips_init() {
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    // A cap whose `init` increments a counter — the tripwire for "Claim
    // ran, Init did not".
    struct InitTripwireCap {
        _init_count: Arc<AtomicU32>,
    }
    impl Addressable for InitTripwireCap {
        const NAMESPACE: &'static str = "test.claim_only.init_tripwire";
        type Resolver = aether_actor::One;
    }
    impl aether_actor::Root for InitTripwireCap {}
    impl aether_actor::Lifecycle<Self> for InitTripwireCap {
        type Config = ();
        type Params = Arc<AtomicU32>;
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init((): (), params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            params.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(Self { _init_count: params })
        }
    }
    impl NativeActor for InitTripwireCap {
        type State = Self;
    }
    impl Dispatch<Self> for InitTripwireCap {
        fn dispatch(
            _state: &mut Self,
            _ctx: &mut NativeCtx<'_, crate::Manual, Self>,
            _kind: KindId,
            _payload: &[u8],
        ) -> Option<()> {
            None
        }
    }

    let (registry, mailer) = bare_substrate();
    let init_count = Arc::new(AtomicU32::new(0));

    // Inline sink registered directly on the shared registry — the
    // headless chassis's `aether.audio` fail-fast sink takes this path,
    // outside the `with_actor` chain.
    registry.register_inline(
        &boot_authority(),
        "test.claim_only.inline_sink",
        Arc::new(|_dispatch: registry::MailDispatch<'_>| {}),
    );

    let claimed = Builder::<DrivenTestChassis<ClaimingDriver>>::new(registry, Arc::clone(&mailer))
        .with_actor::<StubLog>(())
        .with_actor::<InitTripwireCap>(Arc::clone(&init_count))
        .claim_namespaces()
        .expect("claim-only succeeds");

    let expected: BTreeSet<String> = [
        "test.chassis_builder.stub_log",
        "test.claim_only.init_tripwire",
        "test.claim_only.inline_sink",
        "test.claim_only.window",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(claimed, expected, "claim-only reports every claimed namespace and nothing else");

    assert_eq!(
        init_count.load(AtomicOrdering::SeqCst),
        0,
        "claim-only stops at Claim — no cap's init runs, so no OS resource is touched and no pool starts",
    );
}

/// Test driver whose value-free ADR-0155 §4 `claim` hook reserves a
/// driver-as-actor mailbox with `claim_driver_mailbox`, and whose
/// Start-stage `boot` recovers the live claim with
/// `DriverCtx::take_claimed_mailbox` — the desktop `aether.window` split's
/// shape. `boot` records the recovered mailbox id so the test can assert it
/// is the mailbox the Claim hook reserved.
struct ReserveRecoverDriver {
    recovered: Arc<Mutex<Option<MailboxId>>>,
}
struct ReserveRecoverDriverRunning;

impl DriverCapability for ReserveRecoverDriver {
    type Running = ReserveRecoverDriverRunning;

    fn claim(ctx: &mut ChassisCtx<'_>) -> Result<(), BootError> {
        ctx.claim_driver_mailbox("test.reserve_recover.window")
    }

    fn boot(self, ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError> {
        let claim = ctx.take_claimed_mailbox("test.reserve_recover.window").ok_or_else(|| {
            BootError::Other(Box::new(io::Error::other(
                "reserve/recover: the Claim-stage reservation was missing at Start",
            )))
        })?;
        *self.recovered.lock().expect("recovered mutex is never poisoned") = Some(claim.id);
        Ok(ReserveRecoverDriverRunning)
    }
}

impl DriverRunning for ReserveRecoverDriverRunning {
    fn run(self: Box<Self>) -> Result<(), RunError> {
        Ok(())
    }
}

/// ADR-0155 §4 Claim-reserve / Start-recover handoff. The fused `build()`
/// path must run the driver's value-free `claim` hook in the Claim stage
/// (Pass 1 of `boot_passives`) and thread the reserved `MailboxClaim` to the
/// driver's Start-stage `boot`, which recovers it via
/// `DriverCtx::take_claimed_mailbox`. Tripwire: the recovered claim's id
/// must be the id the registry registered for the reserved namespace — so
/// the reservation and the recovery address the same mailbox. A broken
/// handoff (the driver claim never running at Claim, the claim not stashed,
/// or `take` not finding it) leaves `boot` with `None` and aborts the build.
#[test]
fn driver_claim_reserved_at_claim_is_recovered_at_start() {
    let (registry, mailer) = bare_substrate();
    let registry_probe = Arc::clone(&registry);
    let recovered = Arc::new(Mutex::new(None));

    let chassis = Builder::<DrivenTestChassis<ReserveRecoverDriver>>::new(registry, mailer)
        .driver(ReserveRecoverDriver { recovered: Arc::clone(&recovered) })
        .build()
        .expect("build succeeds — the driver recovered its Claim-stage reservation at Start");

    let expected_id = registry_probe
        .list_mailbox_descriptors()
        .into_iter()
        .find(|descriptor| descriptor.name == "test.reserve_recover.window")
        .map(|descriptor| descriptor.id)
        .expect("the reserved namespace is registered on the chassis registry");
    let recovered_id = recovered.lock().expect("recovered mutex is never poisoned").expect("boot recovered a claim");
    assert_eq!(recovered_id, expected_id, "the recovered claim addresses the reserved mailbox");

    chassis.run().expect("driver run succeeds");
}
