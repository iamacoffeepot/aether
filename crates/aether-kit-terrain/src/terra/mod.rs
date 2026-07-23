//! Correlated one-flight terra actor.
//!
//! Terra owns only an ordered selection and semantic command state. It
//! talks to a separately loaded [`crate::mark::MarkBook`] by configured
//! mailbox, preflights every batch completely, and never mutates a world view.

mod kinds;
pub use kinds::*;
mod selection;

use alloc::{string::String, vec, vec::Vec};
use core::mem::take;

use aether_actor::{
    ActorInitError, Manual, OutboundReply, ReplyHandle, RequestId, WasmActor, WasmCtx, WasmInitCtx, actor,
};
use aether_data::MailboxId;
use serde::{Deserialize, Serialize};

use crate::mark::{
    Mark, MarkCreate, MarkCreateResult, MarkDelete, MarkDeleteResult, MarkGet, MarkGetResult, MarkRef, MarkUpdate,
    MarkUpdateResult,
};

use self::selection::{
    BatchProgress, PlannedDelete, PlannedUpdate, Selection, plan_delete, plan_move, plan_relabel, validate_batch_marks,
    validate_mark,
};

/// Selection-and-command facade over a standalone terrain mark book.
///
/// # Agent
/// Load `aether_kit_terrain@aether.kit.terra` with a
/// [`TerraConfig`] containing the mailbox id returned when the mark
/// book was loaded. Send at most one mutation command at a time; queries stay
/// available while that command is in flight.
pub struct TerraEditor {
    config: TerraConfig,
    selection: Selection,
    pending: Option<PendingCommand>,
    pending_request: Option<RequestId>,
}

enum PendingCommand {
    SetSelection {
        reply: Option<ReplyHandle>,
        requested: Vec<MarkRef>,
        index: usize,
    },
    ToggleSelection {
        reply: Option<ReplyHandle>,
        requested: MarkRef,
    },
    Create {
        reply: Option<ReplyHandle>,
        request: MarkCreate,
    },
    Batch {
        reply: Option<ReplyHandle>,
        operation: BatchOperation,
        requested: Vec<MarkRef>,
        marks: Vec<Mark>,
        plan: Option<BatchPlan>,
        index: usize,
        progress: BatchProgress,
    },
}

enum BatchOperation {
    Move(WorldDelta),
    Relabel(String),
    Delete,
}

enum BatchPlan {
    Updates(Vec<PlannedUpdate>),
    Deletes(Vec<PlannedDelete>),
}

#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
enum MarkRequestStage {
    ValidateSelection,
    Create,
    Preflight,
    Update,
    Delete,
}

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[kind(name = "aether.kit.terra.mark_request_context")]
struct MarkRequestContext {
    stage: MarkRequestStage,
    index: u32,
}

enum PendingMarkRequest {
    Get(MarkGet),
    Create(MarkCreate),
    Update(MarkUpdate),
    Delete(MarkDelete),
}

struct OutboundMarkRequest {
    payload: PendingMarkRequest,
    context: MarkRequestContext,
}

struct ReplyEnvelope {
    request: RequestId,
    context: MarkRequestContext,
}

impl PendingCommand {
    fn reply(&self) -> Option<ReplyHandle> {
        match self {
            Self::SetSelection { reply, .. }
            | Self::ToggleSelection { reply, .. }
            | Self::Create { reply, .. }
            | Self::Batch { reply, .. } => *reply,
        }
    }

    fn expected_context(&self) -> MarkRequestContext {
        match self {
            Self::SetSelection { index, .. } => {
                MarkRequestContext { stage: MarkRequestStage::ValidateSelection, index: index_u32(*index) }
            }
            Self::ToggleSelection { .. } => MarkRequestContext { stage: MarkRequestStage::ValidateSelection, index: 0 },
            Self::Create { .. } => MarkRequestContext { stage: MarkRequestStage::Create, index: 0 },
            Self::Batch { plan: Some(BatchPlan::Updates(_)), index, .. } => {
                MarkRequestContext { stage: MarkRequestStage::Update, index: index_u32(*index) }
            }
            Self::Batch { plan: Some(BatchPlan::Deletes(_)), index, .. } => {
                MarkRequestContext { stage: MarkRequestStage::Delete, index: index_u32(*index) }
            }
            Self::Batch { marks, plan, .. } => {
                debug_assert!(plan.is_none());
                MarkRequestContext { stage: MarkRequestStage::Preflight, index: index_u32(marks.len()) }
            }
        }
    }

    fn next_request(&self) -> OutboundMarkRequest {
        let context = self.expected_context();
        let payload = match self {
            Self::SetSelection { requested, index, .. } => {
                PendingMarkRequest::Get(MarkGet { id: requested[*index].id })
            }
            Self::ToggleSelection { requested, .. } => PendingMarkRequest::Get(MarkGet { id: requested.id }),
            Self::Create { request, .. } => PendingMarkRequest::Create(request.clone()),
            Self::Batch { plan: Some(BatchPlan::Updates(updates)), index, .. } => {
                let update = &updates[*index];
                PendingMarkRequest::Update(MarkUpdate {
                    id: update.requested.id,
                    geometry: update.geometry.clone(),
                    label: update.label.clone(),
                })
            }
            Self::Batch { plan: Some(BatchPlan::Deletes(deletes)), index, .. } => {
                PendingMarkRequest::Delete(MarkDelete { id: deletes[*index].requested.id })
            }
            Self::Batch { requested, marks, plan, .. } => {
                debug_assert!(plan.is_none());
                PendingMarkRequest::Get(MarkGet { id: requested[marks.len()].id })
            }
        };
        OutboundMarkRequest { payload, context }
    }
}

fn index_u32(index: usize) -> u32 {
    u32::try_from(index).expect("terra selection cannot exceed u32::MAX entries")
}

impl TerraEditor {
    fn busy(&self) -> bool {
        self.pending.is_some()
    }

    fn query_result(&self) -> TerraQueryResult {
        TerraQueryResult { selection: self.selection.snapshot(), busy: self.busy() }
    }

    fn rejection(&self, error: TerraError) -> TerraCommandResult {
        TerraCommandResult::Rejected { selection: self.selection.snapshot(), error }
    }

    fn applied_without_mark_mutation(&self) -> TerraCommandResult {
        TerraCommandResult::Applied { selection: self.selection.snapshot(), changed: Vec::new(), deleted: Vec::new() }
    }

    fn require_idle(&self) -> Result<(), TerraError> {
        if self.busy() {
            Err(TerraError::Busy)
        } else {
            Ok(())
        }
    }

    fn require_mark_book(&self) -> Result<(), TerraError> {
        if self.config.mark_book_mailbox == MailboxId::NONE {
            Err(TerraError::MarkBookNotConfigured)
        } else {
            Ok(())
        }
    }

    fn mark_book_command_ready(&self, ctx: &mut WasmCtx<'_, Manual>) -> bool {
        match self.require_idle().and_then(|()| self.require_mark_book()) {
            Ok(()) => true,
            Err(error) => {
                ctx.reply(&self.rejection(error));
                false
            }
        }
    }

    fn begin(&mut self, ctx: &mut WasmCtx<'_, Manual>, pending: PendingCommand) {
        self.pending = Some(pending);
        self.issue_next(ctx);
    }

    fn issue_next(&mut self, ctx: &mut WasmCtx<'_, Manual>) {
        let request = self.pending.as_ref().expect("pending command exists before issuing mark request").next_request();
        let mailbox = self.config.mark_book_mailbox;
        let request_id = match request.payload {
            PendingMarkRequest::Get(payload) => ctx.send_to_with_context(mailbox, &payload, &request.context),
            PendingMarkRequest::Create(payload) => ctx.send_to_with_context(mailbox, &payload, &request.context),
            PendingMarkRequest::Update(payload) => ctx.send_to_with_context(mailbox, &payload, &request.context),
            PendingMarkRequest::Delete(payload) => ctx.send_to_with_context(mailbox, &payload, &request.context),
        };
        self.pending_request = Some(request_id);
    }

    fn accepts_envelope(&self, envelope: &ReplyEnvelope) -> bool {
        self.pending_request == Some(envelope.request)
            && self.pending.as_ref().is_some_and(|pending| pending.expected_context() == envelope.context)
    }

    fn reply_source_allowed(source: Option<MailboxId>) -> bool {
        // Peer-component replies deliberately carry SourceAddr::None while
        // echoing the request correlation. A present component source means
        // this is an ordinary send, not the reply issued through MarkBook's
        // one-shot ReplyHandle. The configured mailbox is enforced as the
        // request destination; reply identity is then proven by the stored
        // RequestId plus the request registry's typed, one-shot context.
        source.is_none()
    }

    fn take_reply_envelope(&self, ctx: &mut WasmCtx<'_, Manual>) -> Option<ReplyEnvelope> {
        let source = ctx.source_mailbox();
        if !Self::reply_source_allowed(source) {
            tracing::warn!(
                target: "aether_kit_terrain",
                observed = source.map_or(MailboxId::NONE.0, |mailbox| mailbox.0),
                "terra ignored correlated mail carrying an ordinary component source",
            );
            return None;
        }
        let Some(request) = ctx.in_reply_to() else {
            tracing::warn!(target: "aether_kit_terrain", "terra ignored uncorrelated mark reply");
            return None;
        };
        if self.pending_request != Some(request) {
            tracing::warn!(
                target: "aether_kit_terrain",
                observed = request.0,
                "terra ignored unmatched or duplicate mark reply",
            );
            return None;
        }
        let Some(context) = ctx.take_context::<MarkRequestContext>() else {
            tracing::warn!(
                target: "aether_kit_terrain",
                request = request.0,
                "terra ignored mark reply with missing typed context",
            );
            return None;
        };
        let envelope = ReplyEnvelope { request, context };
        if !self.accepts_envelope(&envelope) {
            tracing::warn!(
                target: "aether_kit_terrain",
                request = request.0,
                stage = ?context.stage,
                index = context.index,
                "terra ignored wrong-stage mark reply",
            );
            return None;
        }
        Some(envelope)
    }

    fn finish(&mut self, ctx: &mut WasmCtx<'_, Manual>, result: &TerraCommandResult) {
        let reply = self.pending.as_ref().and_then(PendingCommand::reply);
        if let Some(reply) = reply {
            ctx.reply_to(reply, result);
        }
        self.pending = None;
        self.pending_request = None;
    }

    fn finish_error(&mut self, ctx: &mut WasmCtx<'_, Manual>, error: TerraError) {
        let result = match self.pending.as_mut() {
            Some(PendingCommand::Batch { progress, .. }) => take(progress).finish(&self.selection, Some(error)),
            _ => self.rejection(error),
        };
        self.finish(ctx, &result);
    }

    fn start_batch(&mut self, ctx: &mut WasmCtx<'_, Manual>, operation: BatchOperation) {
        if !self.mark_book_command_ready(ctx) {
            return;
        }
        if self.selection.is_empty() {
            ctx.reply(&self.rejection(TerraError::EmptySelection));
            return;
        }
        let requested = self.selection.snapshot();
        self.begin(
            ctx,
            PendingCommand::Batch {
                reply: ctx.reply_target(),
                operation,
                requested,
                marks: Vec::new(),
                plan: None,
                index: 0,
                progress: BatchProgress::default(),
            },
        );
    }

    fn complete_preflight(&mut self, ctx: &mut WasmCtx<'_, Manual>) {
        let plan = {
            let Some(PendingCommand::Batch { operation, marks, .. }) = self.pending.as_ref() else {
                self.finish_error(
                    ctx,
                    TerraError::MarkProtocol { reason: String::from("preflight completed outside a batch") },
                );
                return;
            };
            if let Err(error) = validate_batch_marks(marks) {
                Err(error)
            } else {
                match operation {
                    BatchOperation::Move(delta) => plan_move(marks, *delta).map(BatchPlan::Updates),
                    BatchOperation::Relabel(label) => plan_relabel(marks, label).map(BatchPlan::Updates),
                    BatchOperation::Delete => Ok(BatchPlan::Deletes(plan_delete(marks))),
                }
            }
        };
        let plan = match plan {
            Ok(plan) => plan,
            Err(error) => {
                self.finish_error(ctx, error);
                return;
            }
        };
        if let Some(PendingCommand::Batch { plan: pending_plan, index, .. }) = self.pending.as_mut() {
            *pending_plan = Some(plan);
            *index = 0;
        }
        self.issue_next(ctx);
    }

    fn apply_update_result(&mut self, requested: MarkRef, result: MarkUpdateResult) -> Option<TerraError> {
        match result {
            MarkUpdateResult::Updated { reference: observed } => {
                let expected = MarkRef {
                    id: requested.id,
                    revision: requested.revision.checked_add(1).expect("revision exhaustion rejected during preflight"),
                };
                if let Some(PendingCommand::Batch { progress, .. }) = self.pending.as_mut() {
                    progress.record_update(&mut self.selection, observed);
                }
                (observed != expected).then_some(TerraError::RevisionRace { expected, observed })
            }
            MarkUpdateResult::NotFound { .. } => Some(TerraError::MarkMissing { requested }),
            MarkUpdateResult::Rejected { error } => {
                Some(TerraError::MarkMutationRejected { requested: Some(requested), error })
            }
        }
    }

    fn apply_create_result(&mut self, result: MarkCreateResult) -> TerraCommandResult {
        match result {
            MarkCreateResult::Created { reference } => {
                self.selection.replace(vec![reference]);
                TerraCommandResult::Applied {
                    selection: self.selection.snapshot(),
                    changed: vec![reference],
                    deleted: Vec::new(),
                }
            }
            MarkCreateResult::Rejected { error } => {
                self.rejection(TerraError::MarkMutationRejected { requested: None, error })
            }
        }
    }

    fn apply_delete_result(&mut self, requested: MarkRef, result: &MarkDeleteResult) -> Option<TerraError> {
        match result {
            MarkDeleteResult::Deleted { reference: observed } => {
                let observed = *observed;
                if let Some(PendingCommand::Batch { progress, .. }) = self.pending.as_mut() {
                    progress.record_delete(&mut self.selection, observed);
                }
                (observed != requested).then_some(TerraError::RevisionRace { expected: requested, observed })
            }
            MarkDeleteResult::NotFound { .. } => Some(TerraError::MarkMissing { requested }),
        }
    }

    fn advance_batch_commit(&mut self, ctx: &mut WasmCtx<'_, Manual>, complete: bool) {
        if !complete {
            self.issue_next(ctx);
            return;
        }
        let result = match self.pending.as_mut() {
            Some(PendingCommand::Batch { progress, .. }) => take(progress).finish(&self.selection, None),
            _ => unreachable!("batch remains pending"),
        };
        self.finish(ctx, &result);
    }
}

#[actor]
impl WasmActor for TerraEditor {
    type Config = TerraConfig;
    const NAMESPACE: &'static str = "aether.kit.terra";

    fn init(config: TerraConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self { config, selection: Selection::default(), pending: None, pending_request: None })
    }

    #[handler::manual]
    fn on_set_selection(&mut self, ctx: &mut WasmCtx<'_, Manual>, command: SetTerraSelection) {
        if let Err(error) = self.require_idle() {
            ctx.reply(&self.rejection(error));
            return;
        }
        if let Err(error) = Selection::validate_unique(&command.references) {
            ctx.reply(&self.rejection(error));
            return;
        }
        if command.references.is_empty() {
            self.selection.clear();
            ctx.reply(&self.applied_without_mark_mutation());
            return;
        }
        if let Err(error) = self.require_mark_book() {
            ctx.reply(&self.rejection(error));
            return;
        }
        self.begin(
            ctx,
            PendingCommand::SetSelection { reply: ctx.reply_target(), requested: command.references, index: 0 },
        );
    }

    #[handler::manual]
    fn on_toggle_selection(&mut self, ctx: &mut WasmCtx<'_, Manual>, command: ToggleTerraSelection) {
        if !self.mark_book_command_ready(ctx) {
            return;
        }
        self.begin(ctx, PendingCommand::ToggleSelection { reply: ctx.reply_target(), requested: command.reference });
    }

    #[handler::manual]
    fn on_clear_selection(&mut self, ctx: &mut WasmCtx<'_, Manual>, _command: ClearTerraSelection) {
        if let Err(error) = self.require_idle() {
            ctx.reply(&self.rejection(error));
            return;
        }
        self.selection.clear();
        ctx.reply(&self.applied_without_mark_mutation());
    }

    #[handler::manual]
    fn on_create_mark(&mut self, ctx: &mut WasmCtx<'_, Manual>, command: CreateTerraMark) {
        if !self.mark_book_command_ready(ctx) {
            return;
        }
        self.begin(
            ctx,
            PendingCommand::Create {
                reply: ctx.reply_target(),
                request: MarkCreate { geometry: command.geometry, label: command.label },
            },
        );
    }

    #[handler::manual]
    fn on_move_selection(&mut self, ctx: &mut WasmCtx<'_, Manual>, command: MoveTerraSelection) {
        self.start_batch(ctx, BatchOperation::Move(command.delta));
    }

    #[handler::manual]
    fn on_relabel_selection(&mut self, ctx: &mut WasmCtx<'_, Manual>, command: RelabelTerraSelection) {
        self.start_batch(ctx, BatchOperation::Relabel(command.label));
    }

    #[handler::manual]
    fn on_delete_selection(&mut self, ctx: &mut WasmCtx<'_, Manual>, _command: DeleteTerraSelection) {
        self.start_batch(ctx, BatchOperation::Delete);
    }

    #[handler::manual]
    fn on_query(&mut self, ctx: &mut WasmCtx<'_, Manual>, _query: TerraQuery) {
        ctx.reply(&self.query_result());
    }

    #[handler::manual]
    fn on_mark_get_result(&mut self, ctx: &mut WasmCtx<'_, Manual>, result: MarkGetResult) {
        if self.take_reply_envelope(ctx).is_none() {
            return;
        }

        match self.pending.as_mut() {
            Some(PendingCommand::SetSelection { requested, index, .. }) => {
                let reference = requested[*index];
                if let Err(error) = validate_mark(reference, result.mark.as_ref()) {
                    self.finish_error(ctx, error);
                    return;
                }
                *index += 1;
                if *index == requested.len() {
                    self.selection.replace(requested.clone());
                    let reply = self.applied_without_mark_mutation();
                    self.finish(ctx, &reply);
                } else {
                    self.issue_next(ctx);
                }
            }
            Some(PendingCommand::ToggleSelection { requested, .. }) => {
                let requested = *requested;
                if let Err(error) = validate_mark(requested, result.mark.as_ref()) {
                    self.finish_error(ctx, error);
                    return;
                }
                self.selection.toggle(requested);
                let reply = self.applied_without_mark_mutation();
                self.finish(ctx, &reply);
            }
            Some(PendingCommand::Batch { requested, marks, .. }) => {
                let reference = requested[marks.len()];
                if let Err(error) = validate_mark(reference, result.mark.as_ref()) {
                    self.finish_error(ctx, error);
                    return;
                }
                marks.push(result.mark.expect("validated mark exists"));
                if marks.len() == requested.len() {
                    self.complete_preflight(ctx);
                } else {
                    self.issue_next(ctx);
                }
            }
            _ => self.finish_error(
                ctx,
                TerraError::MarkProtocol { reason: String::from("MarkGetResult arrived for a non-get stage") },
            ),
        }
    }

    #[handler::manual]
    fn on_mark_create_result(&mut self, ctx: &mut WasmCtx<'_, Manual>, result: MarkCreateResult) {
        if self.take_reply_envelope(ctx).is_none() {
            return;
        }
        if !matches!(self.pending, Some(PendingCommand::Create { .. })) {
            self.finish_error(
                ctx,
                TerraError::MarkProtocol { reason: String::from("MarkCreateResult arrived for a non-create stage") },
            );
            return;
        }
        let command_result = self.apply_create_result(result);
        self.finish(ctx, &command_result);
    }

    #[handler::manual]
    fn on_mark_update_result(&mut self, ctx: &mut WasmCtx<'_, Manual>, result: MarkUpdateResult) {
        if self.take_reply_envelope(ctx).is_none() {
            return;
        }
        let requested = if let Some(PendingCommand::Batch { plan: Some(BatchPlan::Updates(updates)), index, .. }) =
            self.pending.as_ref()
        {
            updates[*index].requested
        } else {
            self.finish_error(
                ctx,
                TerraError::MarkProtocol { reason: String::from("MarkUpdateResult arrived for a non-update stage") },
            );
            return;
        };
        if let Some(error) = self.apply_update_result(requested, result) {
            self.finish_error(ctx, error);
            return;
        }
        let complete = match self.pending.as_mut() {
            Some(PendingCommand::Batch { plan: Some(BatchPlan::Updates(updates)), index, .. }) => {
                *index += 1;
                *index == updates.len()
            }
            _ => unreachable!("update stage checked above"),
        };
        self.advance_batch_commit(ctx, complete);
    }

    #[handler::manual]
    #[allow(clippy::needless_pass_by_value)] // handler ABI owns decoded mail
    fn on_mark_delete_result(&mut self, ctx: &mut WasmCtx<'_, Manual>, result: MarkDeleteResult) {
        if self.take_reply_envelope(ctx).is_none() {
            return;
        }
        let requested = if let Some(PendingCommand::Batch { plan: Some(BatchPlan::Deletes(deletes)), index, .. }) =
            self.pending.as_ref()
        {
            deletes[*index].requested
        } else {
            self.finish_error(
                ctx,
                TerraError::MarkProtocol { reason: String::from("MarkDeleteResult arrived for a non-delete stage") },
            );
            return;
        };
        if let Some(error) = self.apply_delete_result(requested, &result) {
            self.finish_error(ctx, error);
            return;
        }
        let complete = match self.pending.as_mut() {
            Some(PendingCommand::Batch { plan: Some(BatchPlan::Deletes(deletes)), index, .. }) => {
                *index += 1;
                *index == deletes.len()
            }
            _ => unreachable!("delete stage checked above"),
        };
        self.advance_batch_commit(ctx, complete);
    }
}

#[cfg(test)]
mod tests {
    use serde::de::value::{Error as ValueError, U32Deserializer};

    use super::*;
    use crate::mark::{MarkId, MarkMutationError};

    fn reference(id: u32, revision: u32) -> MarkRef {
        MarkRef { id: MarkId::new(id), revision }
    }

    fn editor() -> TerraEditor {
        TerraEditor {
            config: TerraConfig { mark_book_mailbox: MailboxId(44) },
            selection: Selection::default(),
            pending: None,
            pending_request: None,
        }
    }

    fn reply_handle(raw: u32) -> ReplyHandle {
        ReplyHandle::deserialize(U32Deserializer::<ValueError>::new(raw)).expect("reply handle")
    }

    #[test]
    fn query_is_available_and_reports_busy_from_pending_state() {
        let mut editor = editor();
        editor.selection.replace(vec![reference(2, 1)]);
        assert_eq!(editor.query_result(), TerraQueryResult { selection: vec![reference(2, 1)], busy: false });
        editor.pending = Some(PendingCommand::ToggleSelection { reply: None, requested: reference(3, 1) });
        assert!(editor.busy());
        assert_eq!(editor.require_idle(), Err(TerraError::Busy));
        assert!(editor.query_result().busy);
    }

    #[test]
    fn mismatched_and_stale_reply_envelopes_do_not_advance_or_drop_reply() {
        let mut editor = editor();
        let retained = reply_handle(91);
        editor.pending = Some(PendingCommand::ToggleSelection { reply: Some(retained), requested: reference(3, 1) });
        editor.pending_request = Some(RequestId(12));
        let expected = MarkRequestContext { stage: MarkRequestStage::ValidateSelection, index: 0 };
        let stale_request = ReplyEnvelope { request: RequestId(11), context: expected };
        let wrong_stage = ReplyEnvelope {
            request: RequestId(12),
            context: MarkRequestContext { stage: MarkRequestStage::Update, index: 0 },
        };
        assert!(TerraEditor::reply_source_allowed(None));
        assert!(!TerraEditor::reply_source_allowed(Some(MailboxId(44))));
        assert!(!editor.accepts_envelope(&stale_request));
        assert!(!editor.accepts_envelope(&wrong_stage));
        assert_eq!(
            editor.pending.as_ref().and_then(PendingCommand::reply),
            Some(retained),
            "ignored replies retain the original deferred reply handle"
        );
        assert_eq!(editor.pending_request, Some(RequestId(12)));
    }

    #[test]
    fn create_result_replaces_selection_or_preserves_it_on_rejection() {
        let previous = reference(8, 2);
        let created = reference(9, 1);
        let mut editor = editor();
        editor.selection.replace(vec![previous]);
        assert_eq!(
            editor.apply_create_result(MarkCreateResult::Created { reference: created }),
            TerraCommandResult::Applied { selection: vec![created], changed: vec![created], deleted: Vec::new() }
        );

        let error = MarkMutationError::IdExhausted;
        assert_eq!(
            editor.apply_create_result(MarkCreateResult::Rejected { error: error.clone() }),
            TerraCommandResult::Rejected {
                selection: vec![created],
                error: TerraError::MarkMutationRejected { requested: None, error },
            }
        );
        assert_eq!(editor.selection.snapshot(), vec![created]);
    }

    #[test]
    fn update_revision_race_records_landed_change_before_partial_error() {
        let old = reference(1, 4);
        let observed = reference(1, 7);
        let mut editor = editor();
        editor.selection.replace(vec![old]);
        editor.pending = Some(PendingCommand::Batch {
            reply: None,
            operation: BatchOperation::Relabel(String::from("new")),
            requested: vec![old],
            marks: Vec::new(),
            plan: Some(BatchPlan::Updates(vec![PlannedUpdate {
                requested: old,
                geometry: None,
                label: Some(String::from("new")),
            }])),
            index: 0,
            progress: BatchProgress::default(),
        });
        assert_eq!(
            editor.apply_update_result(old, MarkUpdateResult::Updated { reference: observed }),
            Some(TerraError::RevisionRace { expected: reference(1, 5), observed })
        );
        let result = match editor.pending.as_mut() {
            Some(PendingCommand::Batch { progress, .. }) => take(progress)
                .finish(&editor.selection, Some(TerraError::RevisionRace { expected: reference(1, 5), observed })),
            _ => panic!("batch pending"),
        };
        assert_eq!(
            result,
            TerraCommandResult::PartiallyApplied {
                selection: vec![observed],
                changed: vec![observed],
                deleted: Vec::new(),
                error: TerraError::RevisionRace { expected: reference(1, 5), observed },
            }
        );
    }

    #[test]
    fn delete_revision_race_removes_landed_delete_without_rollback() {
        let expected = reference(1, 4);
        let other = reference(2, 1);
        let observed = reference(1, 5);
        let mut editor = editor();
        editor.selection.replace(vec![expected, other]);
        editor.pending = Some(PendingCommand::Batch {
            reply: None,
            operation: BatchOperation::Delete,
            requested: vec![expected, other],
            marks: Vec::new(),
            plan: Some(BatchPlan::Deletes(vec![PlannedDelete { requested: expected }])),
            index: 0,
            progress: BatchProgress::default(),
        });
        assert_eq!(
            editor.apply_delete_result(expected, &MarkDeleteResult::Deleted { reference: observed }),
            Some(TerraError::RevisionRace { expected, observed })
        );
        assert_eq!(editor.selection.snapshot(), vec![other]);
    }

    #[test]
    fn rejected_update_records_no_progress_so_first_failure_stops_cleanly() {
        let selected = reference(1, 2);
        let mut editor = editor();
        editor.selection.replace(vec![selected]);
        editor.pending = Some(PendingCommand::Batch {
            reply: None,
            operation: BatchOperation::Move(WorldDelta { x_octimeters: 1, z_octimeters: 0 }),
            requested: vec![selected],
            marks: Vec::new(),
            plan: Some(BatchPlan::Updates(vec![PlannedUpdate { requested: selected, geometry: None, label: None }])),
            index: 0,
            progress: BatchProgress::default(),
        });
        let error = MarkMutationError::EmptyUpdate;
        assert_eq!(
            editor.apply_update_result(selected, MarkUpdateResult::Rejected { error: error.clone() }),
            Some(TerraError::MarkMutationRejected { requested: Some(selected), error: error.clone() })
        );
        let result = match editor.pending.as_mut() {
            Some(PendingCommand::Batch { progress, .. }) => take(progress)
                .finish(&editor.selection, Some(TerraError::MarkMutationRejected { requested: Some(selected), error })),
            _ => panic!("batch pending"),
        };
        assert!(matches!(result, TerraCommandResult::Rejected { .. }));
        assert_eq!(editor.selection.snapshot(), vec![selected]);
    }
}
