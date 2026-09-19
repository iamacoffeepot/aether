//! Activation: loads, `Warm` paging, reuse, rejections, and owed catch-up (ADR-0226 decision 5).

use aether_bloomery_kinds::{
    Activated, ActivationRejected, Detail, Digest, DriverRecord, Event, Head, JournalEntry, OpaqueBytes, ReactorIntent,
    ReactorName, ReadArtifact, ReadArtifactResult, Seq, Warm, WarmEntries, Warmed,
};
use aether_bloomery_view::{HeadActivation, Heads};
use aether_data::Kind;

use crate::bundles::InstanceState;
use crate::core::{
    ArtifactRead, ArtifactTicket, BundleRole, Command, EvaluateTicket, LoadOutcome, LoadTicket, PlannedRecord,
    ProgramCore, WarmTicket,
};
use crate::reactors::{
    ActivationPhase, ActivationWork, EvaluateContext, PlanOrder, RoutingRead, SeqPhase, WarmContext,
};

impl ProgramCore {
    /// Feed one warmup reply. Unknown tickets return no commands.
    pub fn on_warmed(&mut self, ticket: WarmTicket, warmed: Warmed) -> Vec<Command> {
        let mut out = Vec::new();
        if self.aborted {
            return out;
        }
        let Some(context) = self.routing.warms.remove(&ticket) else {
            return out;
        };
        if context.head.is_none() {
            self.handle_restart_warmed(&context, warmed, &mut out);
        } else {
            self.handle_activation_warmed(context, warmed, &mut out);
        }
        out
    }

    /// Continue one reactor bundle artifact read.
    pub(crate) fn continue_reactor_artifact(
        &mut self,
        digest: Digest,
        result: ReadArtifactResult,
        out: &mut Vec<Command>,
    ) {
        if !self.routing.started {
            self.continue_restart_bundle_artifact(digest, result, out);
            return;
        }
        let Some(work) = self.routing.current.as_ref() else {
            self.abort(format!("reactor artifact for {digest} arrived with no seq in progress"), out);
            return;
        };
        let Some(activation) = work.activation.as_ref() else {
            self.abort(format!("reactor artifact for {digest} arrived with no activation"), out);
            return;
        };
        if activation.bundle != digest {
            self.abort(format!("reactor artifact for {digest} arrived for another activation"), out);
            return;
        }
        match result {
            ReadArtifactResult::Found { kind, bytes, .. } => {
                if kind != OpaqueBytes::ID {
                    let reason = Detail::new("bundle artifact has the wrong kind");
                    self.fail_activation_with_unavailable(digest, reason, out);
                    if !self.aborted {
                        self.drive_routing(out);
                    }
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
                let reason = Detail::new("bundle artifact is missing");
                self.fail_activation_with_unavailable(digest, reason, out);
                if !self.aborted {
                    self.drive_routing(out);
                }
            }
            ReadArtifactResult::Err { message, .. } => {
                let reason = Detail::new(message);
                self.fail_activation_with_unavailable(digest, reason, out);
                if !self.aborted {
                    self.drive_routing(out);
                }
            }
        }
    }

    /// Continue one reactor load.
    pub(crate) fn continue_reactor_loaded(&mut self, digest: Digest, outcome: LoadOutcome, out: &mut Vec<Command>) {
        if !self.routing.started {
            self.continue_restart_loaded(digest, outcome, out);
            return;
        }
        let Some(work) = self.routing.current.as_ref() else {
            self.abort(format!("reactor load for {digest} arrived with no seq in progress"), out);
            return;
        };
        let Some(activation) = work.activation.as_ref() else {
            self.abort(format!("reactor load for {digest} arrived with no activation"), out);
            return;
        };
        if activation.bundle != digest {
            self.abort(format!("reactor load for {digest} arrived for another activation"), out);
            return;
        }
        match outcome {
            LoadOutcome::Loaded { root } => {
                if let Some(instance) = self.bundles.instance_mut(&digest) {
                    instance.state = InstanceState::Ready { root };
                }
                if let Some(work) = self.routing.current.as_mut()
                    && let Some(activation) = work.activation.as_mut()
                {
                    activation.phase = ActivationPhase::Warming;
                }
                self.drive_routing(out);
            }
            LoadOutcome::Failed { error } => {
                let reason = Detail::new(error);
                self.fail_activation_with_unavailable(digest, reason, out);
                if !self.aborted {
                    self.drive_routing(out);
                }
            }
        }
    }

    /// Continue one activation warm page.
    pub(crate) fn continue_warm_page(
        &mut self,
        digest: Digest,
        head: Head<OpaqueBytes>,
        trigger: u64,
        live_from: u64,
        entries: Vec<JournalEntry>,
        out: &mut Vec<Command>,
    ) {
        let Some(work) = self.routing.current.as_ref() else {
            self.abort("warm page arrived with no seq in progress".to_string(), out);
            return;
        };
        let Some(activation) = work.activation.as_ref() else {
            self.abort("warm page arrived with no activation".to_string(), out);
            return;
        };
        if activation.bundle != digest || activation.head != head || activation.trigger != trigger {
            self.abort("warm page arrived for another activation".to_string(), out);
            return;
        }
        let trimmed: Vec<JournalEntry> = entries.into_iter().filter(|entry| entry.seq < live_from).collect();
        if trimmed.is_empty() {
            self.abort(format!("warm page for {digest} through {live_from} is empty"), out);
            return;
        }
        let first = trimmed.first().expect("non-empty").seq;
        let last = trimmed.last().expect("non-empty").seq;
        let Ok(batch) = WarmEntries::new(trimmed) else {
            self.abort(format!("warm batch for {digest} is not dense from {first}"), out);
            return;
        };
        let Some(instance) = self.bundles.instance(&digest) else {
            self.abort(format!("warm page for unknown digest {digest}"), out);
            return;
        };
        let InstanceState::Ready { root } = instance.state else {
            self.abort(format!("warm page for unready digest {digest}"), out);
            return;
        };
        let ticket = self.mint(WarmTicket::mint);
        self.routing.warms.insert(ticket, WarmContext { digest, head: Some(head), trigger, live_from, first, last });
        out.push(Command::Warm { ticket, root, request: Warm::new(batch) });
    }

    /// Continue one owed catch-up page.
    pub(crate) fn continue_catchup_page(
        &mut self,
        digest: Digest,
        head: &Head<OpaqueBytes>,
        trigger: u64,
        live_from: u64,
        entries: Vec<JournalEntry>,
        out: &mut Vec<Command>,
    ) {
        let Some(work) = self.routing.current.as_ref() else {
            self.abort("catch-up page arrived with no seq in progress".to_string(), out);
            return;
        };
        let Some(activation) = work.activation.as_ref() else {
            self.abort("catch-up page arrived with no activation".to_string(), out);
            return;
        };
        if activation.bundle != digest || activation.head != *head || activation.trigger != trigger {
            self.abort("catch-up page arrived for another activation".to_string(), out);
            return;
        }
        let trimmed: Vec<JournalEntry> = entries.into_iter().filter(|entry| entry.seq <= trigger).collect();
        if let Some(work) = self.routing.current.as_mut()
            && let Some(activation) = work.activation.as_mut()
        {
            activation.catchup_page = trimmed;
            activation.catchup_index = 0;
        }
        let _ = live_from;
        self.drive_catchup(out);
        if !self.aborted {
            self.drive_routing(out);
        }
    }

    /// Begin serial activation once live evaluation finished.
    pub(crate) fn begin_activations(&mut self, out: &mut Vec<Command>) {
        let Some(work) = self.routing.current.as_ref() else {
            return;
        };
        if work.phase != SeqPhase::Evaluating {
            return;
        }
        if work.curr.is_none() {
            if !self.ensure_set_cached(&self.routing.heads.clone(), out) {
                return;
            }
            let curr = self.selection_at(&self.routing.heads.clone());
            let prev = self.routing.current.as_ref().expect("seq work").prev.clone();
            let to_activate = Self::changed_heads(&prev, &curr);
            if let Some(work) = self.routing.current.as_mut() {
                work.curr = Some(curr);
                work.to_activate = to_activate;
                work.phase = SeqPhase::Activating;
            }
        } else if let Some(work) = self.routing.current.as_mut() {
            work.phase = SeqPhase::Activating;
        }
        self.drive_activations(out);
    }

    /// Drive the in-progress activation toward its terminal record.
    pub(crate) fn drive_activations(&mut self, out: &mut Vec<Command>) {
        loop {
            let Some(work) = self.routing.current.as_ref() else {
                return;
            };
            if work.phase != SeqPhase::Activating {
                return;
            }
            if let Some(activation) = work.activation.as_ref() {
                match activation.phase {
                    ActivationPhase::Loading | ActivationPhase::CatchingUp => return,
                    ActivationPhase::Warming => {
                        if !self.drive_warming(out) {
                            return;
                        }
                    }
                }
                if self.aborted {
                    return;
                }
                continue;
            }
            let Some(work) = self.routing.current.as_ref() else {
                return;
            };
            if work.activate_index >= work.to_activate.len() {
                if let Some(work) = self.routing.current.as_mut() {
                    work.phase = SeqPhase::Planning;
                }
                self.drive_planning(out);
                return;
            }
            let (head, bundle) = work.to_activate[work.activate_index].clone();
            let head_index = work.activate_index;
            let trigger = work.n;
            self.start_activation(head, bundle, trigger, head_index, out);
            if self.aborted {
                return;
            }
            if self.routing.read_ticket.is_some()
                || !self.routing.warms.is_empty()
                || !self.routing.evaluates.is_empty()
                || self
                    .artifact_reads
                    .values()
                    .any(|read| matches!(read, ArtifactRead::ReactorBundle(_) | ArtifactRead::ReactorSet(_)))
                || self.loads.values().any(|loaded| self.bundles.instance(loaded).is_some())
            {
                return;
            }
        }
    }

    /// Handle one catch-up `Evaluated` reply for the current activation.
    pub(crate) fn handle_catchup_evaluated(
        &mut self,
        context: EvaluateContext,
        evaluated: aether_bloomery_kinds::Evaluated,
        out: &mut Vec<Command>,
    ) {
        use aether_bloomery_kinds::Evaluated;
        let EvaluateContext { digest, trigger, cause, head } = context;
        let Some(head) = head else {
            self.abort("catch-up reply arrived without a head".to_string(), out);
            return;
        };
        let head_index = match self.catchup_head_index(digest, &head, trigger, cause) {
            Ok(index) => index,
            Err(reason) => {
                self.abort(reason, out);
                return;
            }
        };
        let reply_seq = match &evaluated {
            Evaluated::Completed { seq, .. }
            | Evaluated::OutOfSequence { seq, .. }
            | Evaluated::Poisoned { seq, .. }
            | Evaluated::Failed { seq, .. } => *seq,
        };
        if reply_seq != cause {
            let reason = Detail::new(format!("catch-up evaluated seq {reply_seq} does not match {cause}"));
            self.fail_catchup_with_unavailable(reason, out);
            if !self.aborted {
                self.drive_routing(out);
            }
            return;
        }
        match evaluated {
            Evaluated::Completed { intents, .. } => {
                self.catchup_completed(digest, cause, head_index, intents, out);
            }
            Evaluated::Failed { reactor, reason, .. } => {
                self.catchup_failed(digest, cause, head_index, reactor, reason, out);
            }
            Evaluated::Poisoned { reason, .. } => {
                self.fail_catchup_with_poisoned(reason, out);
                if !self.aborted {
                    self.drive_routing(out);
                }
            }
            Evaluated::OutOfSequence { .. } => {
                let reason = Detail::new(format!("catch-up out-of-sequence at {cause}"));
                self.fail_catchup_with_unavailable(reason, out);
                if !self.aborted {
                    self.drive_routing(out);
                }
            }
        }
    }

    /// Validate one catch-up reply against the current activation, returning its head index.
    fn catchup_head_index(
        &self,
        digest: Digest,
        head: &Head<OpaqueBytes>,
        trigger: u64,
        cause: u64,
    ) -> Result<usize, String> {
        let Some(work) = self.routing.current.as_ref() else {
            return Err("catch-up reply arrived with no seq in progress".to_string());
        };
        let Some(activation) = work.activation.as_ref() else {
            return Err("catch-up reply arrived with no activation".to_string());
        };
        if activation.bundle != digest || activation.head != *head || activation.trigger != trigger {
            return Err("catch-up reply arrived for another activation".to_string());
        }
        if activation.next_k != cause {
            return Err(format!("catch-up reply for {cause} arrived while owing {}", activation.next_k));
        }
        Ok(activation.head_index)
    }

    /// Map one catch-up `Completed` reply, advance its cursor, and finish or continue.
    fn catchup_completed(
        &mut self,
        digest: Digest,
        cause: u64,
        head_index: usize,
        intents: Vec<ReactorIntent>,
        out: &mut Vec<Command>,
    ) {
        let scratch = self
            .routing
            .current
            .as_ref()
            .and_then(|work| work.activation.as_ref())
            .map(|activation| activation.scratch.clone());
        let Some(scratch) = scratch else {
            self.abort("catch-up completed with no activation".to_string(), out);
            return;
        };
        self.map_catchup_completed(digest, cause, head_index, intents, &scratch, out);
        if self.aborted {
            return;
        }
        if let Some(instance) = self.bundles.instance_mut(&digest) {
            instance.cursor = cause;
        }
        if self.advance_catchup(cause) {
            self.finish_activation_success(out);
            if !self.aborted {
                self.drive_routing(out);
            }
            return;
        }
        self.drive_catchup(out);
    }

    /// Map one catch-up `Failed` reply, advance its cursor, and finish or continue.
    fn catchup_failed(
        &mut self,
        digest: Digest,
        cause: u64,
        head_index: usize,
        reactor: ReactorName,
        reason: Detail,
        out: &mut Vec<Command>,
    ) {
        self.map_catchup_failed(digest, cause, head_index, reactor, reason);
        if let Some(instance) = self.bundles.instance_mut(&digest) {
            instance.cursor = cause;
        }
        if self.advance_catchup(cause) {
            self.finish_activation_success(out);
            if !self.aborted {
                self.drive_routing(out);
            }
            return;
        }
        self.drive_catchup(out);
    }

    /// Bump the catch-up cursor past `cause`, returning whether the interval finished.
    fn advance_catchup(&mut self, cause: u64) -> bool {
        self.routing.current.as_mut().and_then(|work| work.activation.as_mut()).is_some_and(|activation| {
            activation.next_k = cause + 1;
            activation.next_k > activation.trigger
        })
    }

    /// Start activating one head, warming or rejecting without a second load.
    fn start_activation(
        &mut self,
        head: Head<OpaqueBytes>,
        bundle: Digest,
        trigger: u64,
        head_index: usize,
        out: &mut Vec<Command>,
    ) {
        let live_from = match self.journal.activations().get(&head) {
            Some(HeadActivation::Owed { from }) => from.0,
            _ => trigger + 1,
        };
        if self.bundles.queue(&bundle).is_some() {
            let reason = Detail::new("digest is loaded as a program bundle");
            self.insert_activation_rejected(head, bundle, trigger, head_index, reason);
            if let Some(work) = self.routing.current.as_mut() {
                work.activate_index += 1;
            }
            return;
        }
        let Some(instance) = self.bundles.claim_reactor(bundle) else {
            let reason = Detail::new("digest is loaded as a program bundle");
            self.insert_activation_rejected(head, bundle, trigger, head_index, reason);
            if let Some(work) = self.routing.current.as_mut() {
                work.activate_index += 1;
            }
            return;
        };
        match &instance.state {
            InstanceState::Poisoned { reason } | InstanceState::Unavailable { reason } => {
                let reason = reason.clone();
                self.insert_activation_rejected(head, bundle, trigger, head_index, reason);
                if let Some(work) = self.routing.current.as_mut() {
                    work.activate_index += 1;
                }
                return;
            }
            InstanceState::Loading => {
                self.abort(format!("activation for {bundle} found a load already in flight"), out);
                return;
            }
            InstanceState::Reading => {
                let ticket = self.mint(ArtifactTicket::mint);
                self.artifact_reads.insert(ticket, ArtifactRead::ReactorBundle(bundle));
                if let Some(work) = self.routing.current.as_mut() {
                    work.activation = Some(ActivationWork {
                        head,
                        bundle,
                        live_from,
                        trigger,
                        head_index,
                        phase: ActivationPhase::Loading,
                        scratch: Heads::new(),
                        next_k: live_from,
                        catchup_page: Vec::new(),
                        catchup_index: 0,
                    });
                }
                out.push(Command::ReadArtifact { ticket, request: ReadArtifact { digest: bundle } });
                return;
            }
            InstanceState::Ready { .. } => {}
        }
        let cursor = self.bundles.instance(&bundle).map_or(0, |instance| instance.cursor);
        if cursor > live_from - 1 {
            let reason = Detail::new("shared instance is past the owed start");
            self.insert_activation_rejected(head, bundle, trigger, head_index, reason);
            if let Some(work) = self.routing.current.as_mut() {
                work.activate_index += 1;
            }
            return;
        }
        if let Some(work) = self.routing.current.as_mut() {
            work.activation = Some(ActivationWork {
                head,
                bundle,
                live_from,
                trigger,
                head_index,
                phase: ActivationPhase::Warming,
                scratch: Heads::new(),
                next_k: live_from,
                catchup_page: Vec::new(),
                catchup_index: 0,
            });
        }
    }

    /// Drive one warming step: next page read, catch-up, or success. Returns
    /// `false` when awaiting a read or `Warm` reply.
    fn drive_warming(&mut self, out: &mut Vec<Command>) -> bool {
        let Some(work) = self.routing.current.as_ref() else {
            return true;
        };
        let Some(activation) = work.activation.as_ref() else {
            return true;
        };
        if activation.phase != ActivationPhase::Warming {
            return true;
        }
        let digest = activation.bundle;
        let head = activation.head.clone();
        let trigger = activation.trigger;
        let live_from = activation.live_from;
        let cursor = self.bundles.instance(&digest).map_or(0, |instance| instance.cursor);
        if cursor >= live_from - 1 {
            if live_from > trigger {
                self.finish_activation_success(out);
                return !self.aborted;
            }
            if let Some(work) = self.routing.current.as_mut()
                && let Some(activation) = work.activation.as_mut()
            {
                activation.phase = ActivationPhase::CatchingUp;
                activation.scratch = Heads::new();
                activation.next_k = live_from;
                activation.catchup_page = Vec::new();
                activation.catchup_index = 0;
            }
            self.emit_routing_read(0, RoutingRead::CatchUp { after: 0, digest, head, trigger, live_from }, out);
            return false;
        }
        self.emit_routing_read(cursor, RoutingRead::Warm { after: cursor, digest, head, trigger, live_from }, out);
        false
    }

    /// Fold buffered catch-up entries and deliver the next owed `Event`.
    fn drive_catchup(&mut self, out: &mut Vec<Command>) {
        loop {
            let Some(work) = self.routing.current.as_ref() else {
                return;
            };
            let Some(activation) = work.activation.as_ref() else {
                return;
            };
            if activation.phase != ActivationPhase::CatchingUp {
                return;
            }
            let digest = activation.bundle;
            let head = activation.head.clone();
            let trigger = activation.trigger;
            let live_from = activation.live_from;
            let next_k = activation.next_k;
            if next_k > trigger {
                self.finish_activation_success(out);
                return;
            }
            let (page_len, page_index, scratch_cursor) =
                (activation.catchup_page.len(), activation.catchup_index, activation.scratch.cursor().0);
            if page_index < page_len {
                let entry = activation.catchup_page[page_index].clone();
                let folded = entry.to_entry();
                let apply = self
                    .routing
                    .current
                    .as_mut()
                    .and_then(|work| work.activation.as_mut())
                    .map(|activation| activation.scratch.apply(&folded));
                match apply {
                    Some(Ok(())) => {}
                    Some(Err(error)) => {
                        self.abort(format!("catch-up scratch rejected entry {}: {error}", entry.seq), out);
                        return;
                    }
                    None => return,
                }
                if let Some(work) = self.routing.current.as_mut()
                    && let Some(activation) = work.activation.as_mut()
                {
                    activation.catchup_index += 1;
                }
                if entry.seq == next_k {
                    let Some(instance) = self.bundles.instance(&digest) else {
                        self.abort(format!("catch-up for unknown digest {digest}"), out);
                        return;
                    };
                    let InstanceState::Ready { root } = instance.state else {
                        self.abort(format!("catch-up for unready digest {digest}"), out);
                        return;
                    };
                    let ticket = self.mint(EvaluateTicket::mint);
                    self.routing
                        .evaluates
                        .insert(ticket, EvaluateContext { digest, trigger, cause: next_k, head: Some(head) });
                    out.push(Command::Evaluate { ticket, root, request: Event::new(entry) });
                    return;
                }
                continue;
            }
            let _ = (live_from, scratch_cursor);
            self.emit_routing_read(
                scratch_cursor,
                RoutingRead::CatchUp { after: scratch_cursor, digest, head, trigger, live_from },
                out,
            );
            return;
        }
    }

    /// Handle one activation `Warmed` reply.
    fn handle_activation_warmed(&mut self, context: WarmContext, warmed: Warmed, out: &mut Vec<Command>) {
        let WarmContext { digest, head, trigger, live_from, first, last } = context;
        let Some(head) = head else {
            self.abort("activation warm arrived without a head".to_string(), out);
            return;
        };
        let expected = self.routing.current.as_ref().and_then(|work| {
            work.activation.as_ref().map(|activation| {
                (activation.bundle, activation.head.clone(), activation.trigger, activation.head_index)
            })
        });
        let Some((bundle, current_head, current_trigger, head_index)) = expected else {
            self.abort("warm arrived with no activation".to_string(), out);
            return;
        };
        if bundle != digest || current_head != head || current_trigger != trigger {
            self.abort("warm arrived for another activation".to_string(), out);
            return;
        }
        match warmed {
            Warmed::Folded { through } => {
                if through != last || last >= live_from {
                    self.abort(
                        format!(
                            "warmed through {through} does not match batch {first}..={last} for live_from {live_from}"
                        ),
                        out,
                    );
                    return;
                }
                let expected_first = self.bundles.instance(&digest).map(|instance| instance.cursor + 1);
                if expected_first != Some(first) {
                    self.abort(
                        format!("warmed batch starts at {first} but instance owes {}", expected_first.unwrap_or(0)),
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
                self.drop_catchup_for_current();
                self.insert_activation_rejected(head, digest, trigger, head_index, reason);
                self.finish_activation();
                self.drive_routing(out);
            }
            Warmed::OutOfSequence { first, expected } => {
                let reason = Detail::new(format!("warm out-of-sequence: first {first}, expected {expected}"));
                if let Some(instance) = self.bundles.instance_mut(&digest) {
                    instance.state = InstanceState::Unavailable { reason: reason.clone() };
                }
                self.drop_catchup_for_current();
                self.insert_activation_rejected(head, digest, trigger, head_index, reason);
                self.finish_activation();
                self.drive_routing(out);
            }
        }
    }

    /// Record one activation rejection for the current head and finish it.
    fn fail_activation_with_unavailable(&mut self, digest: Digest, reason: Detail, out: &mut Vec<Command>) {
        if let Some(instance) = self.bundles.instance_mut(&digest) {
            instance.state = InstanceState::Unavailable { reason: reason.clone() };
        }
        let current = self.routing.current.as_ref().and_then(|work| {
            work.activation
                .as_ref()
                .map(|activation| (activation.head.clone(), activation.trigger, activation.head_index))
        });
        let Some((head, trigger, head_index)) = current else {
            self.abort("activation failure arrived with no activation".to_string(), out);
            return;
        };
        self.insert_activation_rejected(head, digest, trigger, head_index, reason);
        self.finish_activation();
    }

    /// Fail the current catch-up as poisoned, dropping its records.
    fn fail_catchup_with_poisoned(&mut self, reason: Detail, out: &mut Vec<Command>) {
        let current = self.routing.current.as_ref().and_then(|work| {
            work.activation.as_ref().map(|activation| {
                (activation.bundle, activation.head.clone(), activation.trigger, activation.head_index)
            })
        });
        let Some((digest, head, trigger, head_index)) = current else {
            self.abort("catch-up poison arrived with no activation".to_string(), out);
            return;
        };
        if let Some(instance) = self.bundles.instance_mut(&digest) {
            instance.state = InstanceState::Poisoned { reason: reason.clone() };
        }
        self.drop_catchup_for_current();
        self.insert_activation_rejected(head, digest, trigger, head_index, reason);
        self.finish_activation();
    }

    /// Fail the current catch-up as out-of-sequence, dropping its records.
    fn fail_catchup_with_unavailable(&mut self, reason: Detail, out: &mut Vec<Command>) {
        let current = self.routing.current.as_ref().and_then(|work| {
            work.activation.as_ref().map(|activation| {
                (activation.bundle, activation.head.clone(), activation.trigger, activation.head_index)
            })
        });
        let Some((digest, head, trigger, head_index)) = current else {
            self.abort("catch-up failure arrived with no activation".to_string(), out);
            return;
        };
        if let Some(instance) = self.bundles.instance_mut(&digest) {
            instance.state = InstanceState::Unavailable { reason: reason.clone() };
        }
        self.drop_catchup_for_current();
        self.insert_activation_rejected(head, digest, trigger, head_index, reason);
        self.finish_activation();
    }

    /// Record the current head's successful activation and finish it.
    fn finish_activation_success(&mut self, out: &mut Vec<Command>) {
        let current = self.routing.current.as_ref().and_then(|work| {
            work.activation.as_ref().map(|activation| {
                (
                    activation.head.clone(),
                    activation.bundle,
                    activation.live_from,
                    activation.trigger,
                    activation.head_index,
                )
            })
        });
        let Some((head, bundle, live_from, trigger, head_index)) = current else {
            self.abort("activation success arrived with no activation".to_string(), out);
            return;
        };
        let Ok(activated) = Activated::new(head, bundle, Seq(live_from)) else {
            self.abort(format!("activation live_from {live_from} is zero"), out);
            return;
        };
        let record = DriverRecord::Activated { cause: trigger, record: activated };
        if let Some(work) = self.routing.current.as_mut() {
            work.order.insert(
                PlanOrder::Activation { head_index, cause: u64::MAX, index: usize::MAX },
                PlannedRecord::Ready(record),
            );
        }
        self.finish_activation();
    }

    /// Insert one terminal `ActivationRejected` for `head`.
    fn insert_activation_rejected(
        &mut self,
        head: Head<OpaqueBytes>,
        bundle: Digest,
        trigger: u64,
        head_index: usize,
        reason: Detail,
    ) {
        let record =
            DriverRecord::ActivationRejected { cause: trigger, record: ActivationRejected { head, bundle, reason } };
        if let Some(work) = self.routing.current.as_mut() {
            work.order.insert(
                PlanOrder::Activation { head_index, cause: u64::MAX, index: usize::MAX },
                PlannedRecord::Ready(record),
            );
        }
    }

    /// Drop catch-up records and queued destinations for the current head.
    fn drop_catchup_for_current(&mut self) {
        let Some(work) = self.routing.current.as_mut() else {
            return;
        };
        let Some(activation) = work.activation.as_ref() else {
            return;
        };
        let head_index = activation.head_index;
        work.order.retain(|order, _| match order {
            PlanOrder::Activation { head_index: indexed, .. } => *indexed != head_index,
            PlanOrder::Live { .. } => true,
        });
        work.pending_setheads.retain(|pending| match &pending.order {
            PlanOrder::Activation { head_index: indexed, .. } => *indexed != head_index,
            PlanOrder::Live { .. } => true,
        });
    }

    /// Finish the current activation, advancing to the next head.
    fn finish_activation(&mut self) {
        if let Some(work) = self.routing.current.as_mut() {
            work.activation = None;
            work.activate_index += 1;
        }
    }
}
