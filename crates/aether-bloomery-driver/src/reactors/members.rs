//! Membership: set reads, selection at the routing prefix, and served-head maps (ADR-0226 decision 5).

use aether_bloomery_kinds::{Digest, Head, OpaqueBytes, ReactorSet, ReadArtifact, ReadArtifactResult};
use aether_bloomery_view::HeadActivation;
use aether_data::{Kind, Storage};

use crate::core::{ArtifactRead, ArtifactTicket, Command, ProgramCore};
use crate::reactors::{Selection, Served};

impl ProgramCore {
    /// Continue one reactor-set artifact read, caching the decode by digest.
    ///
    /// A missing, wrong-kind, or undecodable set caches as `None` and selects
    /// nothing; a read error aborts, because it says nothing about the bytes,
    /// and so do bytes that do not hash to `digest`.
    pub(crate) fn continue_set_artifact(&mut self, digest: Digest, result: ReadArtifactResult, out: &mut Vec<Command>) {
        let set = match result {
            ReadArtifactResult::Found { artifact } if artifact.kind() == ReactorSet::ID => {
                match artifact.load(digest) {
                    Ok(bytes) => ReactorSet::decode_storage(&bytes).ok().map(|data| data.value),
                    Err(mismatch) => {
                        self.abort(format!("reactor set read failed: {mismatch}"), out);
                        return;
                    }
                }
            }
            ReadArtifactResult::Found { .. } | ReadArtifactResult::Missing { .. } => None,
            ReadArtifactResult::Err { message, .. } => {
                self.abort(format!("reactor set read failed: {message}"), out);
                return;
            }
        };
        self.routing.sets.insert(digest, set);
        self.drive_routing(out);
    }

    /// Selection at the routing prefix: each member head bound there, with its digest.
    pub(crate) fn selection(&self) -> Selection {
        let heads = &self.routing.heads;
        let set = heads
            .get(&ReactorSet::ROOT)
            .and_then(|root| self.routing.sets.get(&root.digest()))
            .and_then(Option::as_ref);
        set.map(|set| {
            set.clusters().iter().filter_map(|member| Some((member.clone(), heads.get(member)?.digest()))).collect()
        })
        .unwrap_or_default()
    }

    /// Ensure the set bound at the routing prefix is cached, reading it when missing.
    ///
    /// Returns `true` when selection may proceed (unbound root or cached
    /// digest), `false` when the read was issued and routing waits on it.
    pub(crate) fn ensure_set_cached(&mut self, out: &mut Vec<Command>) -> bool {
        let Some(digest) = self.routing.heads.get(&ReactorSet::ROOT).map(|root| root.digest()) else {
            return true;
        };
        if self.routing.sets.contains_key(&digest) {
            return true;
        }
        let ticket = self.mint(ArtifactTicket::mint);
        self.artifact_reads.insert(ticket, ArtifactRead::ReactorSet(digest));
        out.push(Command::ReadArtifact { ticket, request: ReadArtifact { digest } });
        false
    }

    /// Heads selected in `curr` whose digest differs from `prev` or is newly selected, in head order.
    pub(crate) fn changed_heads(prev: &Selection, curr: &Selection) -> Vec<(Head<OpaqueBytes>, Digest)> {
        curr.iter()
            .filter(|&(head, digest)| prev.get(head) != Some(digest))
            .map(|(head, digest)| (head.clone(), *digest))
            .collect()
    }

    /// Live digests for `selection`: distinct digests whose head's activation
    /// is live under the selected digest, with served heads in head order.
    pub(crate) fn live_digests(&self, selection: &Selection) -> Served {
        let mut live = Served::new();
        for (head, digest) in selection {
            if matches!(self.journal.activations().get(head), Some(HeadActivation::Live(activated)) if activated.bundle() == *digest)
            {
                live.entry(*digest).or_default().push(head.clone());
            }
        }
        live
    }
}
