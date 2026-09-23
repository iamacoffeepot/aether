//! The reactor role's one decision point over the shared load lifecycle.
//!
//! Activation and restart share [`ProgramCore::claim_reactor`]: the first
//! attempt and every wake-up run the same code, so a load failure, an
//! undeclared role, and a past-start root all refuse through one path.

use aether_bloomery_kinds::{Detail, Digest};

use super::instance::{Health, Instance};
use crate::bundles::{DeclaredRoles, LoadState};
use crate::core::{Command, ProgramCore};

/// How the reactor role may use one digest right now.
pub enum Claim {
    /// Refuse the waiter with the reason.
    Refuse(Detail),
    /// A shared read or load is in flight; the waiter waits.
    Pending,
    /// The digest's live instance may route.
    Ready {
        /// The instance's cursor.
        cursor: u64,
    },
}

impl ProgramCore {
    /// Decide how the reactor role may use `digest` right now.
    ///
    /// An unseen digest starts its one shared read; a digest whose read or
    /// load is in flight waits, whichever role issued it; a digest that
    /// declares no reactors is refused before any load; and a ready digest
    /// routes only through a live instance.
    pub(crate) fn claim_reactor(&mut self, digest: Digest, out: &mut Vec<Command>) -> Claim {
        let declares =
            self.bundles.state(&digest).and_then(LoadState::roles).is_some_and(DeclaredRoles::declares_reactors);
        match self.bundles.state(&digest) {
            None => {
                self.issue_read(digest, out);
                Claim::Pending
            }
            Some(LoadState::Unavailable(reason)) => Claim::Refuse(reason.clone()),
            Some(LoadState::Declared { .. } | LoadState::Loading { .. } | LoadState::Ready { .. }) if !declares => {
                Claim::Refuse(Detail::new("bundle declares no reactors"))
            }
            Some(LoadState::Declared { .. }) => {
                self.issue_load(digest, out);
                Claim::Pending
            }
            Some(LoadState::Reading | LoadState::Loading { .. }) => Claim::Pending,
            Some(LoadState::Ready { .. }) => {
                let instance = self.routing.instances.entry(digest).or_insert_with(Instance::new);
                match &instance.health {
                    Health::Live => Claim::Ready { cursor: instance.cursor },
                    Health::Poisoned(reason) | Health::Untrusted(reason) => Claim::Refuse(reason.clone()),
                }
            }
        }
    }

    /// Whether the digest's root is loaded and its instance is live.
    pub(crate) fn live_ready(&self, digest: Digest) -> bool {
        self.routing.instances.get(&digest).is_some_and(Instance::is_live) && self.bundles.ready(&digest)
    }

    /// Mark `digest` failed with `health` and reject what waited on it: the
    /// activation in progress while routing, or the restart's warming digest
    /// before it.
    pub(crate) fn fail_reactor(&mut self, digest: Digest, health: fn(Detail) -> Health, reason: Detail) {
        if let Some(instance) = self.routing.instances.get_mut(&digest) {
            instance.health = health(reason.clone());
        }
        self.reject_waiter(reason);
    }

    /// Reject what waits on a failed digest.
    fn reject_waiter(&mut self, reason: Detail) {
        if self.routing.started {
            self.reject_activation(reason);
        } else {
            self.fail_restart_digest(&reason);
        }
    }
}
