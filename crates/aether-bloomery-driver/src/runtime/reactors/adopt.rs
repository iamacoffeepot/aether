//! Adoption: a root the engine already held live resumes from the cursor it reports (ADR-0226 decision 9).
//!
//! A load that finds the unit's bundle root already live stands nothing up,
//! so the root has folded whatever an earlier driver delivered to it. When
//! the bundle declares reactors, the core asks the root for its status before
//! routing reaches it: the digest's instance waits on its cursor, routing
//! counts the query as outstanding work, and the status sets the cursor or
//! poisons the instance. Restart warming then starts from that cursor, and
//! live routing delivers from the seq after it.

use aether_bloomery_kinds::{Detail, Digest, Status};

use super::instance::{Health, Instance};
use crate::runtime::bundles::{DeclaredRoles, LoadState};
use crate::runtime::core::{Command, ProgramCore, StatusTicket};

impl ProgramCore {
    /// Ask an adopted root for its cursor when its bundle declares reactors.
    /// A program-only root owes no cursor, so nothing waits on it.
    pub(crate) fn adopt_reactor(&mut self, bundle: Digest, out: &mut Vec<Command>) {
        let declares =
            self.bundles.state(&bundle).and_then(LoadState::roles).is_some_and(DeclaredRoles::declares_reactors);
        if !declares {
            return;
        }
        self.routing.instances.insert(bundle, Instance::adopting());
        let ticket = self.mint(StatusTicket::mint);
        self.routing.adoptions.insert(ticket, bundle);
        out.push(Command::QueryStatus { ticket, bundle });
    }

    /// Resume an adopted instance from its reported cursor, or poison it
    /// when the root reports poisoned.
    pub(crate) fn continue_adoption(&mut self, bundle: Digest, status: &Status) {
        let Some(instance) = self.routing.instances.get_mut(&bundle) else {
            return;
        };
        if status.poisoned() {
            instance.health = Health::Poisoned(Detail::new("adopted reactor reports poisoned"));
        } else {
            instance.cursor = status.cursor();
            instance.health = Health::Live;
        }
    }
}
