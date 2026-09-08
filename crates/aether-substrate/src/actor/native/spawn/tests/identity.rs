//! Identity resolution before construction: a top-level birth stays the root
//! of its own lineage, a birth under a parent folds onto it, and the built
//! actor's binding retains that logical parent (ADR-0099 §3 / ADR-0165).

use std::sync::Arc;

use crate::actor::native::identity::ActorRuntimeIdentity;
use crate::actor::native::spawn::Subname;
use crate::mail::MailboxId;

use super::support::{ActivationConfig, ActivationProbe, activation_fixture};

#[test]
fn spawned_binding_retains_the_logical_parent_mailbox() {
    let (spawner, _registry, _mailer, _pool) = activation_fixture();
    let parent_mailbox = MailboxId(0x4b01);
    let parent =
        ActorRuntimeIdentity::new(parent_mailbox, MailboxId::NONE, parent_mailbox.0, Arc::from("test.parent:root"));
    let root =
        spawner.prepare_identity::<ActivationProbe>(Subname::Named("root"), None).expect("prepare root identity");
    assert_eq!(root.parent, MailboxId::NONE);

    let identity = spawner
        .prepare_identity::<ActivationProbe>(Subname::Named("child"), Some(&parent))
        .expect("prepare child identity");
    assert_eq!(identity.parent, parent_mailbox);
    let (events, _event_rx) = crossbeam_channel::unbounded();
    let staged =
        spawner.build::<ActivationProbe>(identity, ActivationConfig::new(events), (), Vec::new()).expect("build child");

    assert_eq!(staged.transport.parent_mailbox(), parent_mailbox);
}
