//! Identity resolution before construction: a top-level birth stays the root
//! of its own lineage, a birth under a parent folds onto it, the built
//! actor's binding retains that logical parent (ADR-0099 §3 / ADR-0165), and
//! a composed name outside the ADR-0166 grammar is refused at staging.

use aether_data::{ActorPath, ActorPathError, MAX_SCOPE_PATH_DEPTH, ScopePathError};

use crate::actor::native::identity::ActorRuntimeIdentity;
use crate::actor::native::spawn::{SpawnError, Subname};
use crate::mail::MailboxId;

use super::support::{ActivationConfig, ActivationProbe, activation_fixture};

#[test]
fn spawned_binding_retains_the_logical_parent_mailbox() {
    let (spawner, _registry, _mailer, _pool) = activation_fixture();
    let parent_mailbox = MailboxId(0x4b01);
    let parent_name = ActorPath::new("test.parent:root").expect("fixture is an actor path");
    let parent = ActorRuntimeIdentity::new(parent_mailbox, None, parent_mailbox.0, parent_name);
    let root =
        spawner.prepare_identity::<ActivationProbe>(Subname::Named("root"), None).expect("prepare root identity");
    assert_eq!(root.parent, None);

    let identity = spawner
        .prepare_identity::<ActivationProbe>(Subname::Named("child"), Some(&parent))
        .expect("prepare child identity");
    assert_eq!(identity.parent, Some(parent_mailbox));
    let (events, _event_rx) = crossbeam_channel::unbounded();
    let staged =
        spawner.build::<ActivationProbe>(identity, ActivationConfig::new(events), (), Vec::new()).expect("build child");

    assert_eq!(staged.transport.parent_mailbox(), Some(parent_mailbox));
}

/// Catches composing the child's name without proving it: the birth would
/// stage cleanly and come back from the registry owner as `SubnameInUse`,
/// a name conflict that never happened, instead of the lineage fault.
#[test]
fn a_lineage_past_the_depth_cap_is_refused_at_staging() {
    let (spawner, _registry, _mailer, _pool) = activation_fixture();
    let parent_mailbox = MailboxId(0x4b02);
    let parent_name =
        ActorPath::new(&["test.parent"; MAX_SCOPE_PATH_DEPTH].join("/")).expect("a parent at the depth cap is a path");
    let parent = ActorRuntimeIdentity::new(parent_mailbox, None, parent_mailbox.0, parent_name);

    let refused = spawner.prepare_identity::<ActivationProbe>(Subname::Named("deep"), Some(&parent));

    assert!(
        matches!(
            refused,
            Err(SpawnError::PathInvalid(ActorPathError::Scope(ScopePathError::TooDeep {
                limit: MAX_SCOPE_PATH_DEPTH
            })))
        ),
        "a child one step past the depth cap is refused as a path fault: {:?}",
        refused.err(),
    );
}
