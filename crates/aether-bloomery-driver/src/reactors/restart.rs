//! Restart: watermark replay, serial warm, and the rejection batch (ADR-0226 decision 9).

use aether_bloomery_kinds::{
    ActivationRejected, Detail, Digest, DriverRecord, Head, JournalEntry, OpaqueBytes, ReadArtifact,
    ReadArtifactResult, Warm, WarmEntries,
};
use aether_bloomery_view::HeadActivation;
use aether_data::Kind;

use crate::bundles::InstanceState;
use crate::core::{
    ArtifactRead, ArtifactTicket, BundleRole, Command, LoadOutcome, LoadTicket, PendingWrite, PlannedRecord,
    ProgramCore, WarmTicket,
};
use crate::reactors::{RestartPhase, RestartWork, RoutingRead, WarmContext};

impl ProgramCore {
    /// Begin restart replay once the view has synced and program recovery is complete.
    pub(crate) fn begin_restart(&mut self, out: &mut Vec<Command>) {
        let reaction = self.journal.requests().reaction_watermark().0;
        let activation = self.journal.activations().watermark().0;
        let watermark = reaction.max(activation);
        if watermark == 0 {
            self.routing.started = true;
            return;
        }
        self.routing.restart = Some(RestartWork {
            watermark,
            phase: RestartPhase::Folding,
            to_warm: Vec::new(),
            warm_index: 0,
            warming: None,
            failures: Vec::new(),
        });
        self.emit_routing_read(0, RoutingRead::RestartFold { after: 0, watermark }, out);
    }

    /// Drive restart replay toward live routing.
    pub(crate) fn drive_restart(&mut self, out: &mut Vec<Command>) {
        loop {
            let Some(restart) = self.routing.restart.as_ref() else {
                return;
            };
            let progressed = match restart.phase {
                RestartPhase::Folding => self.drive_restart_folding(out),
                RestartPhase::Warming => self.drive_restart_warming(out),
            };
            if self.aborted || !progressed {
                return;
            }
        }
    }

    /// Fold one step toward `W`, returning whether synchronous work remains.
    fn drive_restart_folding(&mut self, out: &mut Vec<Command>) -> bool {
        let Some(restart) = self.routing.restart.as_ref() else {
            return false;
        };
        let watermark = restart.watermark;
        if self.routing.heads.cursor().0 < watermark {
            let after = self.routing.heads.cursor().0;
            self.emit_routing_read(after, RoutingRead::RestartFold { after, watermark }, out);
            return false;
        }
        if !self.ensure_set_cached(&self.routing.heads.clone(), out) {
            return false;
        }
        let selection = self.selection_at(&self.routing.heads.clone());
        let mut to_warm: Vec<(Digest, Vec<Head<OpaqueBytes>>)> = Vec::new();
        for (head, digest) in &selection {
            match self.journal.activations().get(head) {
                Some(HeadActivation::Live(activated)) if activated.bundle() == *digest => {
                    match to_warm.iter_mut().find(|(known, _)| known == digest) {
                        Some((_, heads)) => heads.push(head.clone()),
                        None => to_warm.push((*digest, vec![head.clone()])),
                    }
                }
                Some(HeadActivation::Live(_)) => {
                    self.abort(format!("restart selected {head:?} live under another digest"), out);
                    return false;
                }
                None => {
                    self.abort(format!("restart selected {head:?} with no activation"), out);
                    return false;
                }
                Some(HeadActivation::Owed { .. }) => {}
            }
        }
        if let Some(restart) = self.routing.restart.as_mut() {
            restart.phase = RestartPhase::Warming;
            restart.to_warm = to_warm;
        }
        true
    }

    /// Warm one restarting digest through `W`, returning whether more work remains.
    fn drive_restart_warming(&mut self, out: &mut Vec<Command>) -> bool {
        let Some(restart) = self.routing.restart.as_ref() else {
            return false;
        };
        if restart.warming.is_none() {
            return self.begin_restart_warm(out);
        }
        let Some(restart) = self.routing.restart.as_ref() else {
            return false;
        };
        let Some(warming) = restart.warming else {
            return false;
        };
        let Some(instance) = self.bundles.instance(&warming) else {
            self.abort(format!("restart warming unknown digest {warming}"), out);
            return false;
        };
        let InstanceState::Ready { .. } = &instance.state else {
            self.abort(format!("restart warming digest {warming} is not ready"), out);
            return false;
        };
        let Some(restart) = self.routing.restart.as_ref() else {
            return false;
        };
        let watermark = restart.watermark;
        let cursor = self.bundles.instance(&warming).map_or(0, |instance| instance.cursor);
        if cursor < watermark {
            self.emit_routing_read(cursor, RoutingRead::RestartWarm { after: cursor, digest: warming, watermark }, out);
            return false;
        }
        if let Some(restart) = self.routing.restart.as_mut() {
            restart.warming = None;
            restart.warm_index += 1;
        }
        true
    }

    /// Start warming the next restarting digest, returning whether more work remains.
    fn begin_restart_warm(&mut self, out: &mut Vec<Command>) -> bool {
        let Some(restart) = self.routing.restart.as_ref() else {
            return false;
        };
        if restart.warm_index >= restart.to_warm.len() {
            self.finish_restart(out);
            return false;
        }
        let (digest, _) = restart.to_warm[restart.warm_index].clone();
        if self.bundles.queue(&digest).is_some() {
            self.abort(format!("restart digest {digest} is claimed as a program bundle"), out);
            return false;
        }
        let Some(instance) = self.bundles.claim_reactor(digest) else {
            self.abort(format!("restart digest {digest} is claimed as a program bundle"), out);
            return false;
        };
        match &instance.state {
            InstanceState::Poisoned { reason } | InstanceState::Unavailable { reason } => {
                let reason = reason.as_str().to_string();
                if let Some(restart) = self.routing.restart.as_mut() {
                    let heads = restart.to_warm[restart.warm_index].1.clone();
                    restart.failures.push((digest, heads, reason));
                    restart.warm_index += 1;
                }
                true
            }
            InstanceState::Loading => {
                self.abort(format!("restart digest {digest} found a load already in flight"), out);
                false
            }
            InstanceState::Reading => {
                let ticket = self.mint(ArtifactTicket::mint);
                self.artifact_reads.insert(ticket, ArtifactRead::ReactorBundle(digest));
                if let Some(restart) = self.routing.restart.as_mut() {
                    restart.warming = Some(digest);
                }
                out.push(Command::ReadArtifact { ticket, request: ReadArtifact { digest } });
                false
            }
            InstanceState::Ready { .. } => self.begin_ready_restart_warm(digest, out),
        }
    }

    /// Advance one already-ready restarting digest, returning whether more work remains.
    fn begin_ready_restart_warm(&mut self, digest: Digest, out: &mut Vec<Command>) -> bool {
        let Some(restart) = self.routing.restart.as_ref() else {
            return false;
        };
        let watermark = restart.watermark;
        let cursor = self.bundles.instance(&digest).map_or(0, |instance| instance.cursor);
        if cursor >= watermark {
            if let Some(restart) = self.routing.restart.as_mut() {
                restart.warm_index += 1;
            }
            return true;
        }
        if let Some(restart) = self.routing.restart.as_mut() {
            restart.warming = Some(digest);
        }
        self.emit_routing_read(cursor, RoutingRead::RestartWarm { after: cursor, digest, watermark }, out);
        false
    }

    /// Continue one restart fold page.
    pub(crate) fn continue_restart_fold(&mut self, watermark: u64, entries: Vec<JournalEntry>, out: &mut Vec<Command>) {
        let Some(restart) = self.routing.restart.as_ref() else {
            self.abort("restart fold arrived with no restart in progress".to_string(), out);
            return;
        };
        if restart.watermark != watermark {
            self.abort("restart fold arrived for another watermark".to_string(), out);
            return;
        }
        for entry in entries.into_iter().filter(|entry| entry.seq <= watermark) {
            let seq = entry.seq;
            let folded = entry.to_entry();
            if let Err(error) = self.routing.heads.apply(&folded) {
                self.abort(format!("restart fold rejected entry {seq}: {error}"), out);
                return;
            }
        }
        self.drive_routing(out);
    }

    /// Continue one restart warm page.
    pub(crate) fn continue_restart_warm_page(
        &mut self,
        digest: Digest,
        watermark: u64,
        entries: Vec<JournalEntry>,
        out: &mut Vec<Command>,
    ) {
        let Some(restart) = self.routing.restart.as_ref() else {
            self.abort("restart warm page arrived with no restart in progress".to_string(), out);
            return;
        };
        if restart.watermark != watermark || restart.warming != Some(digest) {
            self.abort("restart warm page arrived for another digest".to_string(), out);
            return;
        }
        let trimmed: Vec<JournalEntry> = entries.into_iter().filter(|entry| entry.seq <= watermark).collect();
        if trimmed.is_empty() {
            self.abort(format!("restart warm page for {digest} through {watermark} is empty"), out);
            return;
        }
        let first = trimmed.first().expect("non-empty").seq;
        let last = trimmed.last().expect("non-empty").seq;
        let Ok(batch) = WarmEntries::new(trimmed) else {
            self.abort(format!("restart warm batch for {digest} is not dense from {first}"), out);
            return;
        };
        let Some(instance) = self.bundles.instance(&digest) else {
            self.abort(format!("restart warm page for unknown digest {digest}"), out);
            return;
        };
        let InstanceState::Ready { root } = instance.state else {
            self.abort(format!("restart warm page for unready digest {digest}"), out);
            return;
        };
        let ticket = self.mint(WarmTicket::mint);
        self.routing.warms.insert(
            ticket,
            WarmContext { digest, head: None, trigger: watermark, live_from: watermark + 1, first, last },
        );
        out.push(Command::Warm { ticket, root, request: Warm::new(batch) });
    }

    /// Continue one restart bundle artifact read.
    pub(crate) fn continue_restart_bundle_artifact(
        &mut self,
        digest: Digest,
        result: ReadArtifactResult,
        out: &mut Vec<Command>,
    ) {
        let Some(restart) = self.routing.restart.as_ref() else {
            self.abort(format!("restart artifact for {digest} arrived with no restart"), out);
            return;
        };
        if restart.warming != Some(digest) {
            self.abort(format!("restart artifact for {digest} arrived for another digest"), out);
            return;
        }
        match result {
            ReadArtifactResult::Found { kind, bytes, .. } => {
                if kind != OpaqueBytes::ID {
                    let reason = "bundle artifact has the wrong kind".to_string();
                    self.fail_restart_digest(digest, reason);
                    self.drive_routing(out);
                    return;
                }
                let ticket = self.mint(LoadTicket::mint);
                self.loads.insert(ticket, digest);
                if let Some(instance) = self.bundles.instance_mut(&digest) {
                    instance.state = InstanceState::Loading;
                }
                out.push(Command::Load { ticket, bundle: digest, role: BundleRole::Reactor, wasm: bytes });
            }
            ReadArtifactResult::Missing { .. } => {
                self.fail_restart_digest(digest, "bundle artifact is missing".to_string());
                self.drive_routing(out);
            }
            ReadArtifactResult::Err { message, .. } => {
                self.fail_restart_digest(digest, message);
                self.drive_routing(out);
            }
        }
    }

    /// Continue one restart load.
    pub(crate) fn continue_restart_loaded(&mut self, digest: Digest, outcome: LoadOutcome, out: &mut Vec<Command>) {
        let Some(restart) = self.routing.restart.as_ref() else {
            self.abort(format!("restart load for {digest} arrived with no restart"), out);
            return;
        };
        if restart.warming != Some(digest) {
            self.abort(format!("restart load for {digest} arrived for another digest"), out);
            return;
        }
        match outcome {
            LoadOutcome::Loaded { root } => {
                if let Some(instance) = self.bundles.instance_mut(&digest) {
                    instance.state = InstanceState::Ready { root };
                }
                self.drive_routing(out);
            }
            LoadOutcome::Failed { error } => {
                self.fail_restart_digest(digest, error);
                self.drive_routing(out);
            }
        }
    }

    /// Handle one restart `Warmed` reply.
    pub(crate) fn handle_restart_warmed(
        &mut self,
        context: &WarmContext,
        warmed: aether_bloomery_kinds::Warmed,
        out: &mut Vec<Command>,
    ) {
        use aether_bloomery_kinds::Warmed;
        let digest = context.digest;
        let watermark = context.trigger;
        let Some(restart) = self.routing.restart.as_ref() else {
            self.abort("restart warm arrived with no restart in progress".to_string(), out);
            return;
        };
        if restart.watermark != watermark || restart.warming != Some(digest) {
            self.abort("restart warm arrived for another digest".to_string(), out);
            return;
        }
        match warmed {
            Warmed::Folded { through } => {
                if through != context.last || context.last >= context.live_from {
                    self.abort(
                        format!(
                            "restart warmed through {through} does not match batch {}..={} for live_from {}",
                            context.first, context.last, context.live_from
                        ),
                        out,
                    );
                    return;
                }
                let expected_first = self.bundles.instance(&digest).map(|instance| instance.cursor + 1);
                if expected_first != Some(context.first) {
                    self.abort(
                        format!(
                            "restart warmed batch starts at {} but instance owes {}",
                            context.first,
                            expected_first.unwrap_or(0)
                        ),
                        out,
                    );
                    return;
                }
                if let Some(instance) = self.bundles.instance_mut(&digest) {
                    instance.cursor = through;
                }
                self.drive_routing(out);
            }
            Warmed::Poisoned { reason, .. } => {
                if let Some(instance) = self.bundles.instance_mut(&digest) {
                    instance.state = InstanceState::Poisoned { reason: reason.clone() };
                }
                self.fail_restart_digest(digest, reason.as_str().to_string());
                self.drive_routing(out);
            }
            Warmed::OutOfSequence { first, expected } => {
                let reason = format!("warm out-of-sequence: first {first}, expected {expected}");
                if let Some(instance) = self.bundles.instance_mut(&digest) {
                    instance.state = InstanceState::Unavailable { reason: Detail::new(&reason) };
                }
                self.fail_restart_digest(digest, reason);
                self.drive_routing(out);
            }
        }
    }

    /// Record one restart digest failure and advance past it.
    fn fail_restart_digest(&mut self, digest: Digest, reason: String) {
        if let Some(restart) = self.routing.restart.as_mut() {
            if restart.warming == Some(digest) {
                restart.warming = None;
            }
            let heads = restart
                .to_warm
                .iter()
                .find(|(known, _)| *known == digest)
                .map(|(_, heads)| heads.clone())
                .unwrap_or_default();
            restart.failures.push((digest, heads, reason));
            restart.warm_index += 1;
        }
    }

    /// Queue the restart rejection batch, or finish an unfailed restart.
    fn finish_restart(&mut self, out: &mut Vec<Command>) {
        let Some(restart) = self.routing.restart.take() else {
            return;
        };
        if restart.failures.is_empty() {
            self.routing.started = true;
            return;
        }
        let mut rejected: Vec<(Head<OpaqueBytes>, Digest, String)> = Vec::new();
        for (digest, heads, reason) in &restart.failures {
            for head in heads {
                rejected.push((head.clone(), *digest, reason.clone()));
            }
        }
        rejected.sort_by(|left, right| left.0.cmp(&right.0));
        let mut plan: Vec<PlannedRecord> = Vec::with_capacity(rejected.len());
        for (head, bundle, reason) in rejected {
            let record = DriverRecord::ActivationRejected {
                cause: restart.watermark,
                record: ActivationRejected { head, bundle, reason: Detail::new(reason) },
            };
            plan.push(PlannedRecord::Ready(record));
        }
        self.journal.queue_back(PendingWrite::Routing { trigger: restart.watermark, plan, live: Vec::new() });
        self.pump(out);
    }
}
