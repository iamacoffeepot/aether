//! Namespace claiming and registry-owner handoff at boot: duplicate claims
//! abort the build, the owner is retained for the chassis lifetime and applies
//! after the direct boot claims, a failed `init` withdraws what it claimed, and
//! a second type at a held namespace fails the build.

use super::support::StubLog;
use crate::actor::native::Dispatch;
use crate::actor::native::ctx::NativeCtx;
use crate::chassis::builder::Builder;
use crate::mail::KindId;
use crate::mail::registry::NativeHoldRefusal;
use crate::testing::{TestChassis, bare_substrate};
use crate::{BootError, NativeActor, NativeInitCtx};
use aether_actor::Addressable;
use std::io;
use std::sync::Arc;
use std::time::Duration;

/// Boot-time mailbox-claim collision aborts the build (and runs
/// the prior cap's drop). Two `StubLog` instances both claim
/// `test.chassis_builder.stub_log`; the second hits the
/// duplicate-claim guard.
#[test]
fn duplicate_passive_mailbox_aborts_build_and_shuts_down_prior() {
    let (registry, mailer) = bare_substrate();
    let registry_probe = Arc::clone(&registry);

    let err = Builder::<TestChassis>::new(registry, mailer)
        .with_actor::<StubLog>(())
        .with_actor::<StubLog>(())
        .build_passive()
        .expect_err("second passive must fail with duplicate claim");

    assert!(matches!(err, BootError::MailboxAlreadyClaimed { .. }));
    assert!(!registry_probe.owner_accepting(), "boot rollback closes the additive registry owner");
}

#[test]
fn registry_owner_is_retained_for_the_chassis_lifetime() {
    let (registry, mailer) = bare_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), mailer)
        .build_passive()
        .expect("empty passive chassis boots");

    assert!(registry.owner_accepting(), "boot attaches the scheduler-backed owner before returning");
    drop(chassis);
    assert!(!registry.owner_accepting(), "chassis teardown closes owner submission before pool teardown");
}

#[test]
fn registry_owner_applies_after_direct_boot_claims() {
    use crate::mail::registry::effect::{EffectBatch, RegistryApplied, RegistryEffect};

    let (registry, mailer) = bare_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), mailer)
        .with_actor::<StubLog>(())
        .build_passive()
        .expect("passive chassis boots with a direct claim");

    let boot_id = registry.lookup(StubLog::NAMESPACE).expect("direct boot claim is published before return");
    let completion = registry
        .submit(EffectBatch::new(vec![RegistryEffect::RegisterKind {
            descriptor: aether_data::KindDescriptor {
                name: "test.registry-owner.queued".to_owned(),
                schema: aether_data::SchemaType::Bytes,
            },
            reject_conflict: true,
        }]))
        .expect("retained owner accepts the queued mutation");
    let queued_kind = match completion
        .wait_timeout(Duration::from_secs(1))
        .expect("scheduler-backed owner completes without blocking teardown")
        .expect("queued mutation applies after the direct boot table")
        .as_slice()
    {
        [RegistryApplied::Kind(id)] => *id,
        applied => panic!("unexpected registry apply result: {applied:?}"),
    };

    assert_eq!(registry.lookup(StubLog::NAMESPACE), Some(boot_id));
    assert_eq!(registry.kind_id("test.registry-owner.queued"), Some(queued_kind));
    drop(chassis);
}

/// A hand-written singleton at one shared namespace: `Cap<true>` fails its
/// `init`, `Cap<false>` boots. The two are distinct types, and neither has a
/// link-time type row, so the publication table first hears of their
/// namespace at the first birth.
struct Cap<const FAIL: bool>;

impl<const FAIL: bool> Addressable for Cap<FAIL> {
    const NAMESPACE: &'static str = "test.phase7.failing_cap";
    type Resolver = aether_actor::One;
}
impl<const FAIL: bool> aether_actor::Root for Cap<FAIL> {}

impl<const FAIL: bool> aether_actor::Lifecycle<Self> for Cap<FAIL> {
    type Config = ();
    type Params = ();
    type InitError = BootError;
    type InitCtx<'a> = NativeInitCtx<'a>;
    type Ctx<'a> = NativeCtx<'a, Self>;
    fn init((): (), _params: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        if FAIL {
            return Err(BootError::Other(Box::new(io::Error::other(
                "intentional init failure for Phase 7 cleanup test",
            ))));
        }
        Ok(Self)
    }
}
impl<const FAIL: bool> aether_actor::Declared for Cap<FAIL> {
    type Depends = ();
    type Spawns = ();
}
impl<const FAIL: bool> NativeActor for Cap<FAIL> {
    type State = Self;
}
impl<const FAIL: bool> Dispatch<Self> for Cap<FAIL> {
    fn dispatch(
        _state: &mut Self,
        _ctx: &mut NativeCtx<'_, Self, crate::Unchecked>,
        _kind: KindId,
        _payload: &[u8],
    ) -> Option<()> {
        None
    }
}

/// Issue 607 Phase 7: a singleton whose `init` returns `Err` fails the build
/// with that error and withdraws its sink. The namespace hold is never
/// released (a failed boot fails the build, R-0046), so what may boot the
/// same namespace next is a fresh chassis on a fresh registry.
// Catches: a hold kept outside the engine's registry (a process-wide table),
// so one engine's failed boot refuses the namespace to every later engine.
#[test]
fn failed_singleton_init_fails_the_build_and_withdraws_its_sink() {
    let (registry, mailer) = bare_substrate();
    let err = Builder::<TestChassis>::new(Arc::clone(&registry), mailer)
        .with_actor::<Cap<true>>(())
        .build_passive()
        .expect_err("init failure must propagate");

    assert!(format!("{err:?}").contains("intentional init failure"), "expected init error to propagate, got {err:?}");
    assert!(
        registry.lookup(Cap::<true>::NAMESPACE).is_none(),
        "sink at {} should be removed after failed init",
        Cap::<true>::NAMESPACE,
    );

    let (registry, mailer) = bare_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), mailer)
        .with_actor::<Cap<false>>(())
        .build_passive()
        .expect("a fresh engine boots another type at the failed namespace");
    assert!(registry.lookup(Cap::<false>::NAMESPACE).is_some());
    drop(chassis);
}

// Catches: `hold_native` forgetting the first holder of a namespace the
// link-time inventory does not list, so two hand-written types both boot at
// one namespace in one engine.
#[test]
fn a_second_type_at_a_held_namespace_fails_the_build() {
    let (registry, mailer) = bare_substrate();
    let err = Builder::<TestChassis>::new(registry, mailer)
        .with_actor::<Cap<false>>(())
        .with_actor::<Cap<true>>(())
        .build_passive()
        .expect_err("the second type's boot is refused its namespace");

    let BootError::Other(source) = &err else {
        panic!("expected the hold refusal, got {err:?}")
    };
    assert!(
        matches!(source.downcast_ref::<NativeHoldRefusal>(), Some(NativeHoldRefusal::HeldByOther { .. })),
        "expected HeldByOther, got {err:?}"
    );
}
