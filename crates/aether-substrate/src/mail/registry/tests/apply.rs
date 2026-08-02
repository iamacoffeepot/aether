//! Tests for [`super::super::mailbox::apply`] — the staged fold every
//! writer funnels through, including the direct pre-seal write path.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::config::RegistryQueueCapacities;
use crate::mail::mailer::Mailer;
use crate::mail::registry::effect::{EffectBatch, RegistryEffect};
use crate::mail::registry::owner::RegistryOwnerLease;
use crate::mail::registry::{MailboxEntry, Registry, noop_handler};
use crate::scheduler::WakeSink;
use crate::testing::boot_authority as auth;

#[test]
#[allow(clippy::disallowed_methods, reason = "the test deliberately races the two writer entry points")]
fn direct_and_owner_paths_share_the_transitional_writer() {
    use std::sync::Barrier;

    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let owner = RegistryOwnerLease::attach(
        auth(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
    let completion = registry
        .submit(EffectBatch::new(vec![RegistryEffect::publish_named(
            "shared-writer".to_owned(),
            MailboxEntry::Inbox { handler: noop_handler(), seize: Arc::default() },
        )]))
        .expect("owner accepts effect");
    let barrier = Arc::new(Barrier::new(2));
    let direct_registry = Arc::clone(&registry);
    let direct_barrier = Arc::clone(&barrier);
    let direct = thread::spawn(move || {
        direct_barrier.wait();
        direct_registry.try_register_inbox(&auth(), "shared-writer", noop_handler())
    });
    barrier.wait();
    owner.run_once();

    let owner_result = completion.wait_timeout(Duration::from_millis(100)).expect("owner completes");
    let direct_result = direct.join().expect("direct writer does not panic");
    assert_ne!(owner_result.is_ok(), direct_result.is_ok(), "exactly one serialized writer claims the route");
    assert_eq!(registry.list_mailbox_descriptors().iter().filter(|entry| entry.name == "shared-writer").count(), 1);
}
