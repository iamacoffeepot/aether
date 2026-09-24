//! Restart: watermark replay, serial warm, and the rejection batch (ADR-0226 decision 9).
//!
//! The restart point `W` is the higher of the reaction and activation
//! watermarks. Routing takes its `Heads` at `W` from the journal view's head
//! history without delivering or reading the journal, then loads and warms
//! each digest a head selected at `W` is live under,
//! serially and through `W`. A digest that fails to load or warm rejects
//! the heads it serves in one batch caused by `W`; live routing starts at
//! `W + 1`.

use std::collections::VecDeque;

use aether_bloomery_kinds::{ActivationRejected, Detail, Digest, DriverRecord, Head, JournalEntry, OpaqueBytes, Seq};
use aether_bloomery_view::HeadActivation;

use crate::core::{Command, PendingWrite, PlannedRecord, ProgramCore};
use crate::reactors::claim::Claim;
use crate::reactors::{RestartPhase, RestartWork, RoutingRead};

impl ProgramCore {
    /// Begin restart replay once the view has synced and program recovery is complete.
    pub(crate) fn begin_restart(&mut self) {
        let reaction = self.journal.requests().reaction_watermark().0;
        let activation = self.journal.activations().watermark().0;
        let watermark = reaction.max(activation);
        if watermark == 0 {
            self.routing.started = true;
        } else {
            self.routing.restart = Some(RestartWork { watermark, phase: RestartPhase::Folding, failures: Vec::new() });
        }
    }

    /// Take one restart step: take routing `Heads` at `W`, then load and warm each live digest.
    pub(crate) fn drive_restart(&mut self, out: &mut Vec<Command>) {
        let Some(restart) = self.routing.restart.as_ref() else {
            return;
        };
        let watermark = restart.watermark;
        if matches!(restart.phase, RestartPhase::Folding) {
            self.drive_restart_folding(watermark, out);
        } else {
            self.drive_restart_warming(watermark, out);
        }
    }

    /// Set routing `Heads` to the journal view's `Heads` at `W`, then queue
    /// the digests heads selected at `W` are live under.
    ///
    /// A selected head with no activation, or live under another digest, is
    /// a history the driver could not have written, so the core aborts.
    fn drive_restart_folding(&mut self, watermark: u64, out: &mut Vec<Command>) {
        if self.routing.cursor() < watermark {
            let Some(heads) = self.journal.history().heads_at(Seq(watermark)) else {
                let cursor = self.journal.cursor();
                self.abort(format!("restart point {watermark} is past the journal-view cursor {cursor}"), out);
                return;
            };
            self.routing.heads = heads;
        }
        if !self.ensure_set_cached(out) {
            return;
        }
        let mut queued: VecDeque<(Digest, Vec<Head<OpaqueBytes>>)> = VecDeque::new();
        for (head, digest) in self.selection() {
            match self.journal.activations().get(&head) {
                Some(HeadActivation::Live(activated)) if activated.bundle() == digest => {
                    match queued.iter_mut().find(|(known, _)| *known == digest) {
                        Some((_, heads)) => heads.push(head),
                        None => queued.push_back((digest, vec![head])),
                    }
                }
                Some(HeadActivation::Owed { .. }) => {}
                Some(HeadActivation::Live(_)) => {
                    self.abort(format!("restart selected {head:?} live under another digest"), out);
                    return;
                }
                None => {
                    self.abort(format!("restart selected {head:?} with no activation"), out);
                    return;
                }
            }
        }
        if let Some(restart) = self.routing.restart.as_mut() {
            restart.phase = RestartPhase::Warming { queued, warming: None };
        }
    }

    /// Take one warming step: start the next digest, page its warm through `W`, or finish.
    fn drive_restart_warming(&mut self, watermark: u64, out: &mut Vec<Command>) {
        let Some(RestartWork { phase: RestartPhase::Warming { queued, warming }, .. }) = self.routing.restart.as_mut()
        else {
            return;
        };
        if warming.is_none() {
            let Some(next) = queued.pop_front() else {
                self.finish_restart(out);
                return;
            };
            *warming = Some(next);
        }
        let Some(digest) = warming.as_ref().map(|(digest, _)| *digest) else {
            return;
        };
        match self.claim_reactor(digest, out) {
            Claim::Refuse(reason) => self.fail_restart_digest(&reason),
            Claim::Pending => {}
            Claim::Ready { cursor } if cursor < watermark => {
                self.emit_routing_read(cursor, RoutingRead::RestartWarm, out);
            }
            Claim::Ready { .. } => {
                if let Some(RestartWork { phase: RestartPhase::Warming { warming, .. }, .. }) =
                    self.routing.restart.as_mut()
                {
                    *warming = None;
                }
            }
        }
    }

    /// Warm the restarting digest with one page, trimmed to `W`.
    pub(crate) fn continue_restart_warm_page(&mut self, entries: Vec<JournalEntry>, out: &mut Vec<Command>) {
        let Some(RestartWork { watermark, phase: RestartPhase::Warming { warming: Some((digest, _)), .. }, .. }) =
            self.routing.restart.as_ref()
        else {
            self.abort("restart warm page arrived with no digest warming".to_string(), out);
            return;
        };
        let (watermark, digest) = (*watermark, *digest);
        self.send_warm(digest, entries.into_iter().filter(|entry| entry.seq <= watermark).collect(), out);
    }

    /// Record the warming digest's failure against every head it serves and move on.
    pub(crate) fn fail_restart_digest(&mut self, reason: &Detail) {
        if let Some(RestartWork { phase: RestartPhase::Warming { warming, .. }, failures, .. }) =
            self.routing.restart.as_mut()
            && let Some((digest, heads)) = warming.take()
        {
            failures.extend(heads.into_iter().map(|head| (head, digest, reason.clone())));
        }
    }

    /// Queue the restart rejection batch, or start live routing when nothing failed.
    ///
    /// The batch's read-back starts live routing.
    fn finish_restart(&mut self, out: &mut Vec<Command>) {
        let Some(RestartWork { watermark, mut failures, .. }) = self.routing.restart.take() else {
            return;
        };
        if failures.is_empty() {
            self.routing.started = true;
            return;
        }
        failures.sort_by(|left, right| left.0.cmp(&right.0));
        let plan = failures
            .into_iter()
            .map(|(head, bundle, reason)| {
                let record = ActivationRejected { head, bundle, reason };
                PlannedRecord::Ready(DriverRecord::ActivationRejected { cause: watermark, record })
            })
            .collect();
        self.journal.queue_back(PendingWrite::Routing { trigger: watermark, plan });
        self.pump(out);
    }
}
