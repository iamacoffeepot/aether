//! `muse.session.record`: a rested turn written down as the [`Session`] it
//! leaves.

use aether_bloomery_kinds::{Detail, Mode, Ref, Refusal, Tree, Utf8Text};
use aether_bloomery_program::{Env, Program, Sync, program};

use crate::input::{CallId, Role, ToolCall, ToolOutput, TurnInput, TurnItem};
use crate::result::{TurnOutcome, TurnResult};
use crate::session::replay::replay;
use crate::session::state::{RestReason, Session, SessionItems};

/// One call's output, answering the call with the same id.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
pub struct CallAnswer {
    /// The id of the call this answers.
    call_id: CallId,
    /// The program's result, or the reason its arguments did not decode.
    output: ToolOutput,
}

impl CallAnswer {
    /// `output` answering the call `call_id`.
    #[must_use]
    pub const fn new(call_id: CallId, output: ToolOutput) -> Self {
        Self { call_id, output }
    }

    /// The id of the call this answers.
    #[must_use]
    pub const fn call_id(&self) -> &CallId {
        &self.call_id
    }

    /// The answer as the conversation item that replays it.
    #[must_use]
    pub fn item(&self) -> TurnItem {
        TurnItem::CallOutput { call_id: self.call_id.clone(), output: self.output.clone() }
    }
}

/// A rested turn to record: the turn, its result, the outputs of every call it
/// asked for when it rests at the turn limit, and the tree its tools left.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.session.record.input")]
pub struct RecordInput {
    /// The cited input of the turn the session rests after.
    turn: Ref<TurnInput>,
    /// The cited result of that turn.
    result: Ref<TurnResult>,
    /// For a turn that asked for calls, one output for each call in the order
    /// asked. Empty for a turn that answered, stopped early, or refused.
    outputs: Vec<CallAnswer>,
    /// The tree the session's tools left. `None` until tools edit files.
    tree: Option<Ref<Tree>>,
}

impl RecordInput {
    /// Record the rest after the turn `turn` that answered `result`, with the
    /// `outputs` of any calls it asked for.
    #[must_use]
    pub const fn new(
        turn: Ref<TurnInput>,
        result: Ref<TurnResult>,
        outputs: Vec<CallAnswer>,
        tree: Option<Ref<Tree>>,
    ) -> Self {
        Self { turn, result, outputs, tree }
    }
}

/// The `muse.session.record` program.
pub struct SessionRecord;

/// Records a rested session: the turn's conversation followed by its reply, or
/// by its calls and their outputs when it rests at the turn limit.
///
/// A turn that answered, stopped early, or refused adds one assistant message
/// with its text or refusal. A turn that asked for calls adds its text when not
/// empty, every call, and the outputs, which must answer the calls exactly and
/// in order, and rests at the turn limit. Anything else refuses.
#[program]
impl Program for SessionRecord {
    const NAME: &'static str = "muse.session.record";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Record a rested Muse session from its last turn.";
    type Input = RecordInput;
    type Result = Session;

    fn run(input: Self::Input, env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        let turn = env.injected(input.turn)?;
        let result = env.injected(input.result)?;
        let (reply, rested) = rest(result.outcome(), &input.outputs)?;

        let items = turn.items().iter().cloned().chain(reply).collect();
        let items = SessionItems::new(items).map_err(|error| refused(&format!("the session's items: {error}")))?;
        Ok(Session::new(turn.settings(), items, rested, input.tree))
    }
}

/// What `outcome` adds to the conversation and why it rests the session.
fn rest(outcome: &TurnOutcome, outputs: &[CallAnswer]) -> Result<(Vec<TurnItem>, RestReason), Refusal> {
    let said = |text: &Ref<Utf8Text>| vec![TurnItem::message(Role::Assistant, *text)];
    match (outcome, outputs.is_empty()) {
        (TurnOutcome::Completed { text, .. }, true) => Ok((said(text), RestReason::Completed)),
        (TurnOutcome::Incomplete { text, .. }, true) => Ok((said(text), RestReason::Incomplete)),
        (TurnOutcome::Declined { refusal, .. }, true) => Ok((said(refusal), RestReason::Declined)),
        (TurnOutcome::Called { calls, text, .. }, _) if answers(calls.as_slice(), outputs) => {
            Ok((replay(*text, calls.as_slice(), outputs), RestReason::TurnLimit))
        }
        (TurnOutcome::Called { .. }, _) => Err(refused("the outputs do not answer the turn's calls in order")),
        _ => Err(refused("the turn's outcome does not rest a session with these outputs")),
    }
}

/// Whether `outputs` answer `calls` one to one, in order.
fn answers(calls: &[ToolCall], outputs: &[CallAnswer]) -> bool {
    calls.len() == outputs.len() && calls.iter().zip(outputs).all(|(call, output)| call.call_id() == output.call_id())
}

fn refused(reason: &str) -> Refusal {
    Refusal::Refused { reason: Detail::new(reason) }
}

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{Ref, Refusal};

    use super::{CallAnswer, RecordInput, SessionRecord};
    use crate::input::tests::call;
    use crate::input::{CallId, OfferedTools, Role, ToolCalls, ToolOutput, TurnItem, TurnItems};
    use crate::result::{HttpStatus, TurnOutcome, TurnResult, TurnUsage};
    use crate::session::fixture::{run, settings, stored};
    use crate::session::state::RestReason;

    fn answer(id: &str) -> CallAnswer {
        CallAnswer::new(CallId::new(id).expect("call id"), ToolOutput::Refused(Ref::of_text(id)))
    }

    #[test]
    fn a_rest_records_the_reply_or_at_the_limit_every_call_and_its_output() {
        // Catches a rest recording the wrong reply or reason, a limit rest whose items differ from what the next turn
        // would have sent, and outputs that do not answer the calls being recorded anyway.
        let user = TurnItem::message(Role::User, Ref::of_text("hi"));
        let turn = settings(OfferedTools::default()).with_items(TurnItems::new(vec![user.clone()]).expect("items"));
        let usage = TurnUsage::new(0, 0, 0, 0);
        let result = |outcome| TurnResult::new(HttpStatus::new(200).expect("status"), Ref::of_bytes(b"{}"), outcome);
        let record = |outcome, outputs| {
            let result = result(outcome);
            let input = RecordInput::new(
                Ref::of_encoded(&turn).expect("turn"),
                Ref::of_encoded(&result).expect("result"),
                outputs,
                None,
            );
            run::<SessionRecord>(&input, vec![stored(&turn), stored(&result)])
        };

        let completed = record(TurnOutcome::Completed { text: Ref::of_text("done"), usage }, Vec::new())
            .expect("a completed turn records");
        assert_eq!(completed.items(), [user.clone(), TurnItem::message(Role::Assistant, Ref::of_text("done"))]);
        assert_eq!(completed.rested(), RestReason::Completed);
        assert_eq!(*completed.settings(), turn.settings());

        let calls = || ToolCalls::new(vec![call("a", "muse.echo"), call("b", "muse.echo")]).expect("calls");
        let called = || TurnOutcome::Called { calls: calls(), text: Ref::of_text("Checking."), usage };
        let limited = record(called(), vec![answer("a"), answer("b")]).expect("an answered turn records");
        assert_eq!(
            limited.items(),
            [
                user,
                TurnItem::message(Role::Assistant, Ref::of_text("Checking.")),
                TurnItem::Call(calls().as_slice()[0].clone()),
                TurnItem::Call(calls().as_slice()[1].clone()),
                answer("a").item(),
                answer("b").item(),
            ]
        );
        assert_eq!(limited.rested(), RestReason::TurnLimit);

        for outputs in [vec![answer("a")], vec![answer("b"), answer("a")]] {
            assert!(matches!(record(called(), outputs), Err(Refusal::Refused { .. })), "outputs must answer in order");
        }
    }
}
