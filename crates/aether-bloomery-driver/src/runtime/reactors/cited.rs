//! Cited artifacts: what a routing page's entries cite directly, read before the page routes (ADR-0226).
//!
//! Every page reaches routing through
//! [`continue_routing_events`](ProgramCore::continue_routing_events) — steady,
//! warm, catch-up, and restart warm alike — so a `Warm` and a live `Event`
//! carrying the same entry hand a root the same artifacts. Hits come from the
//! driver's artifact cache; misses are read with one `ReadArtifacts`, which
//! answers a prefix by contract, so the core asks again for the rest. A failed
//! read is re-issued up to [`ARTIFACT_READ_RETRIES`] times, then aborts; a
//! missing artifact means the journal broke its append guarantee, and aborts.
//! Neither poisons a view: the page never reaches a root.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use aether_bloomery_kinds::{ArtifactDigests, JournalEntry, ReadArtifacts, ReadArtifactsResult};

use crate::runtime::core::{ArtifactsTicket, Command, ProgramCore};
use crate::runtime::reactors::{CitedEntry, PendingCited, RoutingRead};

/// Failed `ReadArtifacts` answers the core re-issues for one request before it aborts.
const ARTIFACT_READ_RETRIES: u32 = 3;

impl ProgramCore {
    /// Feed one batched artifact reply. Unknown tickets return no commands.
    pub fn on_artifacts(&mut self, ticket: ArtifactsTicket, result: ReadArtifactsResult) -> Vec<Command> {
        let mut out = Vec::new();
        if self.aborted {
            return out;
        }
        let Some((_, mut pending)) = self.routing.cited.take_if(|(held, _)| *held == ticket) else {
            return out;
        };
        match result {
            ReadArtifactsResult::Found { artifacts } => {
                if artifacts.is_empty() {
                    self.abort("journal answered a cited-artifact read with no artifacts".to_string(), &mut out);
                    return out;
                }
                for artifact in artifacts {
                    let claimed = artifact.claimed().unverified();
                    if pending.missing.pop_front() != Some(claimed) {
                        self.abort(format!("journal answered cited artifact {claimed} out of request order"), &mut out);
                        return out;
                    }
                    self.artifacts.insert(claimed, artifact.clone());
                    pending.found.insert(claimed, artifact);
                }
                pending.retries = 0;
                self.continue_cited(pending, &mut out);
            }
            ReadArtifactsResult::Missing { digest } => {
                self.abort(format!("journal is missing artifact {digest}, which a routed entry cites"), &mut out);
                return out;
            }
            ReadArtifactsResult::Err { message } => {
                if pending.retries >= ARTIFACT_READ_RETRIES {
                    let reason = format!("cited artifact read failed after {} retries: {message}", pending.retries);
                    self.abort(reason, &mut out);
                    return out;
                }
                pending.retries += 1;
                self.request_cited(pending, &mut out);
            }
        }
        self.drive_routing(&mut out);
        out
    }

    /// Gather what `entries` cite for `purpose`: cached artifacts at once,
    /// and one read for the rest before the page routes.
    pub(crate) fn fetch_cited(&mut self, purpose: RoutingRead, entries: Vec<JournalEntry>, out: &mut Vec<Command>) {
        let mut found = BTreeMap::new();
        let mut missing = VecDeque::new();
        let mut seen = BTreeSet::new();
        for digest in entries.iter().flat_map(|entry| entry.cites.iter().copied()) {
            if !seen.insert(digest) {
                continue;
            }
            let Some(artifact) = self.artifacts.get(digest) else {
                missing.push_back(digest);
                continue;
            };
            found.insert(digest, artifact.clone());
        }
        self.continue_cited(PendingCited { purpose, entries, found, missing, retries: 0 }, out);
    }

    /// Route the page once nothing it cites is missing, or read the next
    /// missing prefix.
    fn continue_cited(&mut self, pending: PendingCited, out: &mut Vec<Command>) {
        if pending.missing.is_empty() {
            self.route_cited(pending, out);
        } else {
            self.request_cited(pending, out);
        }
    }

    /// Park `pending` behind one `ReadArtifacts` for its missing digests, in
    /// citation order, under the core's byte budget.
    fn request_cited(&mut self, pending: PendingCited, out: &mut Vec<Command>) {
        let batch = pending.missing.iter().take(ReadArtifacts::MAX_ARTIFACTS).copied().collect();
        let digests = match ArtifactDigests::new(batch) {
            Ok(digests) => digests,
            Err(error) => {
                self.abort(format!("cited artifact read refused its own digest list: {error}"), out);
                return;
            }
        };
        let ticket = self.mint(ArtifactsTicket::mint);
        out.push(Command::ReadArtifacts { ticket, request: ReadArtifacts { digests, limit_bytes: self.limit } });
        self.routing.cited = Some((ticket, pending));
    }

    /// Pair each entry with one artifact per distinct digest it cites, in
    /// citation order, and route the page.
    fn route_cited(&mut self, pending: PendingCited, out: &mut Vec<Command>) {
        let PendingCited { purpose, entries, found, .. } = pending;
        let mut page = Vec::with_capacity(entries.len());
        for entry in entries {
            let mut seen = BTreeSet::new();
            let mut artifacts = Vec::with_capacity(entry.cites.len());
            for digest in entry.cites.iter().filter(|digest| seen.insert(**digest)) {
                let Some(artifact) = found.get(digest) else {
                    self.abort(format!("entry {} cites {digest}, which routing never read", entry.seq), out);
                    return;
                };
                artifacts.push(artifact.clone());
            }
            page.push(CitedEntry { entry, artifacts });
        }
        match purpose {
            RoutingRead::Steady => self.routing.page = page.into(),
            RoutingRead::Warm => self.continue_warm_page(page, out),
            RoutingRead::CatchUp => self.continue_catch_up_page(page, out),
            RoutingRead::RestartWarm => self.continue_restart_warm_page(page, out),
        }
    }
}
