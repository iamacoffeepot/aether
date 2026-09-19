//! Membership: set reads, selection at a prefix, and served-head maps (ADR-0226 decision 5).

use std::collections::BTreeMap;

use aether_bloomery_kinds::{Digest, Head, OpaqueBytes, ReactorSet, ReadArtifact, ReadArtifactResult};
use aether_bloomery_view::Heads;
use aether_data::{Kind, Storage};

use crate::core::{ArtifactRead, ArtifactTicket, Command, ProgramCore};

impl ProgramCore {
    /// Continue one reactor-set artifact read, caching the decode by digest.
    pub(crate) fn continue_set_artifact(&mut self, digest: Digest, result: ReadArtifactResult, out: &mut Vec<Command>) {
        match result {
            ReadArtifactResult::Found { kind, bytes, .. } => {
                if kind == ReactorSet::ID {
                    match ReactorSet::decode_storage(&bytes) {
                        Ok(data) => {
                            self.routing.sets.insert(digest, Some(data.value));
                        }
                        Err(_) => {
                            self.routing.sets.insert(digest, None);
                        }
                    }
                } else {
                    self.routing.sets.insert(digest, None);
                }
                self.drive_routing(out);
            }
            ReadArtifactResult::Missing { .. } => {
                self.routing.sets.insert(digest, None);
                self.drive_routing(out);
            }
            ReadArtifactResult::Err { message, .. } => {
                self.abort(format!("reactor set read failed: {message}"), out);
            }
        }
    }

    /// Selection at one prefix: member head to bound digest, skipping unbound entries.
    pub(crate) fn selection_at(&self, heads: &Heads) -> BTreeMap<Head<OpaqueBytes>, Digest> {
        let mut selection = BTreeMap::new();
        let Some(set_ref) = heads.get(&ReactorSet::ROOT) else {
            return selection;
        };
        let Some(cached) = self.routing.sets.get(&set_ref.digest()) else {
            return selection;
        };
        let Some(set) = cached else {
            return selection;
        };
        for member in set.clusters() {
            if let Some(bound) = heads.get(member) {
                selection.insert(member.clone(), bound.digest());
            }
        }
        selection
    }

    /// Ensure the set bound at `heads` is cached, reading it when missing.
    ///
    /// Returns `true` when selection may proceed (unbound root or cached
    /// digest), `false` when a read was issued or is already outstanding and
    /// the caller must pause.
    pub(crate) fn ensure_set_cached(&mut self, heads: &Heads, out: &mut Vec<Command>) -> bool {
        let Some(set_ref) = heads.get(&ReactorSet::ROOT) else {
            return true;
        };
        let digest = set_ref.digest();
        if self.routing.sets.contains_key(&digest) {
            return true;
        }
        for read in self.artifact_reads.values() {
            if matches!(read, ArtifactRead::ReactorSet(pending) if *pending == digest) {
                return false;
            }
        }
        let ticket = self.mint(ArtifactTicket::mint);
        self.artifact_reads.insert(ticket, ArtifactRead::ReactorSet(digest));
        out.push(Command::ReadArtifact { ticket, request: ReadArtifact { digest } });
        false
    }

    /// Heads selected at `curr` whose digest differs from `prev` or is newly selected, in head order.
    pub(crate) fn changed_heads(
        prev: &BTreeMap<Head<OpaqueBytes>, Digest>,
        curr: &BTreeMap<Head<OpaqueBytes>, Digest>,
    ) -> Vec<(Head<OpaqueBytes>, Digest)> {
        let mut changed = Vec::new();
        for (head, digest) in curr {
            match prev.get(head) {
                Some(previous) if previous == digest => {}
                _ => changed.push((head.clone(), *digest)),
            }
        }
        changed
    }

    /// Live digests for `selection`: distinct digests whose activation is live
    /// with the selected digest, with served heads in head order.
    pub(crate) fn live_digests(
        &self,
        selection: &BTreeMap<Head<OpaqueBytes>, Digest>,
    ) -> BTreeMap<Digest, Vec<Head<OpaqueBytes>>> {
        let mut live: BTreeMap<Digest, Vec<Head<OpaqueBytes>>> = BTreeMap::new();
        for (head, digest) in selection {
            let is_live = match self.journal.activations().get(head) {
                Some(aether_bloomery_view::HeadActivation::Live(activated)) => activated.bundle() == *digest,
                _ => false,
            };
            if is_live {
                live.entry(*digest).or_default().push(head.clone());
            }
        }
        live
    }
}
