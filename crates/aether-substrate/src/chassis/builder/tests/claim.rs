//! Namespace claiming and registry-owner handoff at boot: duplicate claims
//! abort the build, the owner is retained for the chassis lifetime and applies
//! after the direct boot claims, and a failed `init` releases what it claimed.

use super::support::StubLog;
use crate::actor::native::Dispatch;
use crate::actor::native::ctx::NativeCtx;
use crate::chassis::builder::Builder;
use crate::mail::KindId;
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

/// Issue 607 Phase 7: a singleton whose `init` returns `Err`
/// releases its slot before `with_actor` propagates the error.
/// After the failed build, the chassis's `Registry` has no sink
/// at the cap's namespace and the `ActorRegistry`'s `name_owners`
/// no longer claims the namespace — so a fresh chassis can boot
/// a different cap with the same namespace string (or the same
/// cap with a different config) without colliding.
#[test]
fn failed_singleton_init_releases_namespace_and_sink() {
    struct FailingCap;
    impl Addressable for FailingCap {
        const NAMESPACE: &'static str = "test.phase7.failing_cap";
        type Resolver = aether_actor::One;
    }
    impl aether_actor::Root for FailingCap {}

    impl aether_actor::Lifecycle<Self> for FailingCap {
        type Config = ();
        type Params = ();
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init((): (), _params: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Err(BootError::Other(Box::new(io::Error::other("intentional init failure for Phase 7 cleanup test"))))
        }
    }
    impl NativeActor for FailingCap {
        type State = Self;
    }
    impl Dispatch<Self> for FailingCap {
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
    let err = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<FailingCap>(())
        .build_passive()
        .expect_err("init failure must propagate");
    // The error wraps init's std::io::Error message.
    assert!(format!("{err:?}").contains("intentional init failure"), "expected init error to propagate, got {err:?}");

    // Sink at the cap's namespace must be gone — Registry::lookup
    // returns None for absent entries.
    assert!(
        registry.lookup(FailingCap::NAMESPACE).is_none(),
        "sink at {} should be removed after failed init",
        FailingCap::NAMESPACE,
    );
}
