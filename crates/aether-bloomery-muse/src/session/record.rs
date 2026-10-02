//! `muse.session.record`: the turn a session ended on, written down as the
//! [`Session`] it leaves.

use aether_bloomery_kinds::{Detail, Mode, Ref, Refusal, Tree, Utf8Text};
use aether_bloomery_program::{Env, Program, Sync, program};

use crate::input::{CallId, Role, ToolCall, ToolOutput, TurnInput, TurnItem, TurnItems};
use crate::result::{TurnOutcome, TurnResult, TurnUsage};
use crate::session::replay::replay;
use crate::session::state::{Failure, RestReason, Session, SessionItems};
use crate::tools::{Ending, end_position};

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

    /// The answer's output.
    #[must_use]
    pub const fn output(&self) -> &ToolOutput {
        &self.output
    }

    /// The answer as the conversation item that replays it.
    #[must_use]
    pub fn item(&self) -> TurnItem {
        TurnItem::CallOutput { call_id: self.call_id.clone(), output: self.output.clone() }
    }
}

/// A turn's result and the outputs of the calls it asked for that have been
/// answered, in the order asked.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
pub struct Answered {
    /// The cited result of the turn.
    result: Ref<TurnResult>,
    /// For a turn that asked for calls, one output for each call answered, in
    /// the order asked. Empty for a turn that answered, stopped early, or
    /// refused.
    outputs: Vec<CallAnswer>,
}

impl Answered {
    /// The turn's `result` and the `outputs` of the calls answered so far.
    #[must_use]
    pub const fn new(result: Ref<TurnResult>, outputs: Vec<CallAnswer>) -> Self {
        Self { result, outputs }
    }
}

/// How the turn a session is recorded after ended the activation.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
pub enum TurnEnd {
    /// The turn rested the session: at the turn limit, or past the input
    /// limit, with every call's output.
    Rested(Answered),
    /// The session failed after the turn was sent. `answered` is that turn's
    /// result and the calls answered before the failure, when the turn asked
    /// for calls.
    Failed {
        /// Why the session failed.
        failure: Failure,
        /// The turn's result and its calls answered before the failure.
        answered: Option<Answered>,
    },
}

/// A session's end to record: the last turn it sent, how that turn ended the
/// activation, and the tree its tools left.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.session.record.input")]
pub struct RecordInput {
    /// The cited input of the last turn the session sent.
    turn: Ref<TurnInput>,
    /// How that turn ended the activation.
    end: TurnEnd,
    /// The tree the session's tools left: the latest tree of the session.
    tree: Ref<Tree>,
}

impl RecordInput {
    /// Record the rest after the turn `turn` that answered `result`, with the
    /// `outputs` of any calls it asked for and the `tree` its tools left.
    #[must_use]
    pub const fn rested(
        turn: Ref<TurnInput>,
        result: Ref<TurnResult>,
        outputs: Vec<CallAnswer>,
        tree: Ref<Tree>,
    ) -> Self {
        Self { turn, end: TurnEnd::Rested(Answered::new(result, outputs)), tree }
    }

    /// Record the session that failed with `failure` after it sent `turn`,
    /// with that turn's `answered` calls, when it asked for any, and the
    /// `tree` its tools left.
    #[must_use]
    pub const fn failed(turn: Ref<TurnInput>, failure: Failure, answered: Option<Answered>, tree: Ref<Tree>) -> Self {
        Self { turn, end: TurnEnd::Failed { failure, answered }, tree }
    }
}

/// The `muse.session.record` program.
pub struct SessionRecord;

/// Records a session at the end of an activation: the last turn's
/// conversation followed by its reply, or by its calls and their outputs.
///
/// A turn that ended the run through `muse.end` adds its text, every call,
/// and the outputs, which must answer the calls exactly and in order, and
/// rests with `Completed`, `Blocked`, or `Asked` by the end call's variant:
/// `Done`, `Blocked`, or `Asked`. An end call without a readable `Ending`
/// output refuses rather than resting with a guessed reason. A turn that answered,
/// stopped early, or refused adds one assistant message with its text or
/// refusal, except a turn that stopped early with no text, which adds nothing:
/// the session then ends on what that turn sent, so a continue can resend it
/// as it stands. A turn that asked for calls and reached a limit adds its
/// text when not empty, every call, and the outputs, which must answer the
/// calls exactly and in order, and rests at the turn limit, or with
/// `ContextFull` when it reached the session's input limit. A reply without
/// calls only rests at a limit, so its arm rests like the called arm: with
/// `ContextFull` past the turn's input limit, and with `TurnLimit` at the
/// turn limit.
///
/// A failed session records the last turn's conversation, followed, when that
/// turn asked for calls, by its text, the calls answered before the failure,
/// and their outputs; calls after those are left out, so no call is left
/// unanswered. When that would pass the item cap, the turn's conversation
/// alone is recorded. A turn failure must cite a result that ends a session:
/// rejected, unreadable, or transient. Anything else refuses.
#[program]
impl Program for SessionRecord {
    const NAME: &'static str = "muse.session.record";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Record a Muse session from the last turn of its activation.";
    type Input = RecordInput;
    type Result = Session;

    fn run(input: Self::Input, env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        let turn = env.injected(input.turn)?;
        let (items, rested) = match input.end {
            TurnEnd::Rested(answered) => {
                let result = env.injected(answered.result)?;
                let ended = ended(env, result.outcome(), &answered.outputs)?;
                let (reply, rested) = rest(&turn, result.outcome(), &answered.outputs, ended)?;
                (session_items(turn.items().iter().cloned().chain(reply).collect())?, rested)
            }
            TurnEnd::Failed { failure, answered } => {
                if let Failure::Turn { result } = &failure {
                    ends(env.injected(*result)?.outcome())?;
                }
                let reply = match answered {
                    Some(answered) => partial(env.injected(answered.result)?.outcome(), &answered.outputs)?,
                    None => Vec::new(),
                };
                let items: Vec<_> = turn.items().iter().cloned().chain(reply).collect();
                let items = if items.len() > TurnItems::MAX_ITEMS {
                    turn.items().to_vec()
                } else {
                    items
                };
                (session_items(items)?, RestReason::Failed(failure))
            }
        };
        Ok(Session::new(turn.settings(), items, rested, input.tree))
    }
}

/// `items` as a session's conversation, or the refusal naming the rule they
/// broke.
fn session_items(items: Vec<TurnItem>) -> Result<SessionItems, Refusal> {
    SessionItems::new(items).map_err(|error| refused(&format!("the session's items: {error}")))
}

/// The `Ending` of the turn's end call, read from the closure, when the
/// outcome's calls include a decoded `muse.end` call, with its text staged for
/// the closing message to cite. An end call without a readable result refuses
/// rather than resting with a guessed reason.
fn ended(env: &mut Env<Sync>, outcome: &TurnOutcome, outputs: &[CallAnswer]) -> Result<Option<Ending>, Refusal> {
    let TurnOutcome::Called { calls, .. } = outcome else {
        return Ok(None);
    };
    if !answers(calls.as_slice(), outputs) {
        return Ok(None);
    }
    let Some(position) = end_position(calls.as_slice()) else {
        return Ok(None);
    };
    let result = match outputs[position].output() {
        ToolOutput::Result { result, .. } => result.cast::<Ending>(),
        ToolOutput::Refused(_) => None,
    };
    let Some(result) = result else {
        return Err(refused("the end call has no readable end result"));
    };
    let ending = env.injected(result).map_err(|_| refused("the end call has no readable end result"))?;
    env.stage_text(ending.text());
    Ok(Some(ending))
}

/// What `outcome` adds to the conversation and why it rests the session.
fn rest(
    turn: &TurnInput,
    outcome: &TurnOutcome,
    outputs: &[CallAnswer],
    ended: Option<Ending>,
) -> Result<(Vec<TurnItem>, RestReason), Refusal> {
    let said = |text: &Ref<Utf8Text>| vec![TurnItem::message(Role::Assistant, *text)];
    match (outcome, outputs.is_empty()) {
        (TurnOutcome::Completed { text, usage, .. }, true) => Ok((said(text), limited(turn, usage))),
        (TurnOutcome::Incomplete { text, .. }, true) if *text == Ref::of_text("") => {
            Ok((Vec::new(), RestReason::Incomplete))
        }
        (TurnOutcome::Incomplete { text, .. }, true) => Ok((said(text), RestReason::Incomplete)),
        (TurnOutcome::Declined { refusal, .. }, true) => Ok((said(refusal), RestReason::Declined)),
        (TurnOutcome::Called { calls, text, usage, .. }, _) if answers(calls.as_slice(), outputs) => {
            let reply = replay(*text, calls.as_slice(), outputs);
            match ended {
                Some(ending) => {
                    let rested = ending.rest_reason();
                    let closing = TurnItem::message(Role::Assistant, Ref::of_text(ending.text()));
                    Ok((reply.into_iter().chain([closing]).collect(), rested))
                }
                None => Ok((reply, limited(turn, usage))),
            }
        }
        (TurnOutcome::Called { .. }, _) => Err(refused("the outputs do not answer the turn's calls in order")),
        _ => Err(refused("the turn's outcome does not rest a session with these outputs")),
    }
}

/// Why a turn that goes on but reached a limit rests: with `ContextFull` past
/// the cited turn's input limit, and with `TurnLimit` at the turn limit.
fn limited(turn: &TurnInput, usage: &TurnUsage) -> RestReason {
    if turn.input_limit().reached(usage.input_tokens()) {
        RestReason::ContextFull
    } else {
        RestReason::TurnLimit
    }
}

/// What a called turn whose first `outputs.len()` calls were answered before
/// the session failed adds to the conversation: its text, those calls, and
/// their outputs.
fn partial(outcome: &TurnOutcome, outputs: &[CallAnswer]) -> Result<Vec<TurnItem>, Refusal> {
    let TurnOutcome::Called { calls, text, .. } = outcome else {
        return Err(refused("only a turn that asked for calls has answered calls"));
    };
    let calls = calls.as_slice().get(..outputs.len()).filter(|calls| answers(calls, outputs));
    let calls = calls.ok_or_else(|| refused("the outputs do not answer the turn's first calls in order"))?;
    Ok(replay(*text, calls, outputs))
}

/// Refuses an `outcome` that rests a session instead of ending it.
fn ends(outcome: &TurnOutcome) -> Result<(), Refusal> {
    match outcome {
        TurnOutcome::Rejected | TurnOutcome::Unreadable | TurnOutcome::Transient { .. } => Ok(()),
        _ => Err(refused("the turn's outcome rests a session instead of ending it")),
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
    use aether_bloomery_kinds::{Detail, ErasedRef, ProgramName, Ref, Refusal, Tree};
    use aether_bloomery_program::{Program, ToolSchema};
    use aether_data::Kind;

    use super::{Answered, CallAnswer, RecordInput, SessionRecord};
    use crate::input::tests::call;
    use crate::input::{CallId, OfferedTools, Role, ToolCall, ToolCalls, ToolOutput, TurnInput, TurnItem, TurnItems};
    use crate::result::{HttpStatus, TurnOutcome, TurnResult, TurnUsage};
    use crate::session::fixture::{run, settings, stored};
    use crate::session::state::{Failure, RestReason, Session};
    use crate::tools::{End, EndArgs, Ending};

    const USAGE: TurnUsage = TurnUsage::new(0, 0, 0, 0);

    fn answer(id: &str) -> CallAnswer {
        CallAnswer::new(CallId::new(id).expect("call id"), ToolOutput::Refused(Ref::of_text(id)))
    }

    /// `id` answered with `ending` as the call's stored output.
    fn ended_answer(id: &str, ending: &Ending) -> CallAnswer {
        let schema = ToolSchema::of::<Ending>();
        let digest = Ref::of_encoded(ending).expect("an ending encodes").digest();
        let output = ToolOutput::result(Ref::of_encoded(&schema).expect("a schema encodes"), &schema, digest);
        CallAnswer::new(CallId::new(id).expect("call id"), output)
    }

    fn user() -> TurnItem {
        TurnItem::message(Role::User, Ref::of_text("hi"))
    }

    fn turn(items: Vec<TurnItem>) -> TurnInput {
        settings(OfferedTools::default()).with_items(TurnItems::new(items).expect("items"))
    }

    fn result(outcome: TurnOutcome) -> TurnResult {
        TurnResult::received(HttpStatus::new(200).expect("status"), Ref::of_bytes(b"{}"), outcome)
    }

    fn calls() -> ToolCalls {
        ToolCalls::new(vec![call("a", "muse.echo"), call("b", "muse.echo")]).expect("calls")
    }

    /// A call list of one decoded `muse.end` call `id` over `ending`.
    fn end_calls(id: &str, ending: &Ending) -> ToolCalls {
        let args = Ref::of_encoded(&EndArgs::new(ending.clone())).expect("end arguments encode");
        let input = ErasedRef::new(EndArgs::ID, args.digest());
        let program = ProgramName::new(End::NAME).expect("program name");
        let call = ToolCall::decoded(CallId::new(id).expect("call id"), program, Ref::of_text("{}"), input);
        ToolCalls::new(vec![call]).expect("calls")
    }

    fn called() -> TurnOutcome {
        TurnOutcome::Called { calls: calls(), text: Ref::of_text("Checking."), usage: USAGE }
    }

    fn tree() -> Ref<Tree> {
        Ref::of_encoded(&Tree::empty()).expect("tree")
    }

    /// Run `input`'s record over its cited `turn` and `result`.
    fn record(turn: &TurnInput, result: &TurnResult, input: &RecordInput) -> Result<Session, Refusal> {
        run::<SessionRecord>(input, vec![stored(turn), stored(result)])
    }

    /// Run `input`'s record over its cited `turn`, `result`, and `endings`.
    fn record_with(
        turn: &TurnInput,
        result: &TurnResult,
        endings: &[Ending],
        input: &RecordInput,
    ) -> Result<Session, Refusal> {
        let mut closure = vec![stored(turn), stored(result)];
        closure.extend(endings.iter().map(stored));
        run::<SessionRecord>(input, closure)
    }

    #[test]
    fn a_rest_records_the_reply_or_at_the_limit_every_call_and_its_output() {
        // Catches a rest recording the wrong reply or reason, a limit rest whose items differ from what the next turn
        // would have sent, and outputs that do not answer the calls being recorded anyway.
        let turn = turn(vec![user()]);
        let rested = |outcome, outputs| {
            let result = result(outcome);
            let input = RecordInput::rested(
                Ref::of_encoded(&turn).expect("turn"),
                Ref::of_encoded(&result).expect("result"),
                outputs,
                tree(),
            );
            record(&turn, &result, &input)
        };

        let completed = rested(TurnOutcome::Completed { text: Ref::of_text("done"), usage: USAGE }, Vec::new())
            .expect("a completed turn records");
        assert_eq!(completed.items(), [user(), TurnItem::message(Role::Assistant, Ref::of_text("done"))]);
        assert_eq!(*completed.rested(), RestReason::TurnLimit);
        assert_eq!(*completed.settings(), turn.settings());

        let limited = rested(called(), vec![answer("a"), answer("b")]).expect("an answered turn records");
        assert_eq!(
            limited.items(),
            [
                user(),
                TurnItem::message(Role::Assistant, Ref::of_text("Checking.")),
                TurnItem::Call(calls().as_slice()[0].clone()),
                TurnItem::Call(calls().as_slice()[1].clone()),
                answer("a").item(),
                answer("b").item(),
            ]
        );
        assert_eq!(*limited.rested(), RestReason::TurnLimit);

        for outputs in [vec![answer("a")], vec![answer("b"), answer("a")]] {
            assert!(matches!(rested(called(), outputs), Err(Refusal::Refused { .. })), "outputs must answer in order");
        }
    }

    #[test]
    fn a_text_only_completed_turn_at_or_past_the_input_limit_rests_like_a_called_one() {
        // Catches a nudged-out session resting `Completed`: the loop only records a reply without calls at a limit, so
        // the recorder must label it the way the called arm does.
        let turn = turn(vec![user()]);
        let limit = turn.input_limit().get();
        assert!(limit > 0, "the fixture limit is non-zero");
        let rested = |input_tokens| {
            let usage = TurnUsage::new(input_tokens, 0, 0, 0);
            let result = result(TurnOutcome::Completed { text: Ref::of_text("done"), usage });
            let input = RecordInput::rested(
                Ref::of_encoded(&turn).expect("turn"),
                Ref::of_encoded(&result).expect("result"),
                Vec::new(),
                tree(),
            );
            record(&turn, &result, &input).expect("a completed turn at a limit records")
        };

        assert_eq!(*rested(limit).rested(), RestReason::ContextFull);
        assert_eq!(*rested(u64::MAX).rested(), RestReason::ContextFull);
        assert_eq!(*rested(limit - 1).rested(), RestReason::TurnLimit);
    }

    #[test]
    fn a_called_turn_at_the_input_limit_rests_full_with_every_call_and_its_output() {
        // Catches the recorder disagreeing with the loop's diversion: the loop rests `Rest` past the input limit, so
        // the record must label it `ContextFull` with the same items the next turn would have sent, one token below
        // it must stay `TurnLimit`, and a terminal rest past the limit must keep its own reason.
        let turn = turn(vec![user()]);
        let limit = turn.input_limit().get();
        assert!(limit > 0, "the fixture limit is non-zero");
        let rested = |outcome, outputs| {
            let result = result(outcome);
            let input = RecordInput::rested(
                Ref::of_encoded(&turn).expect("turn"),
                Ref::of_encoded(&result).expect("result"),
                outputs,
                tree(),
            );
            record(&turn, &result, &input)
        };
        let usage = |input_tokens| TurnUsage::new(input_tokens, 0, 0, 0);
        let called_at = |input_tokens| TurnOutcome::Called {
            calls: calls(),
            text: Ref::of_text("Checking."),
            usage: usage(input_tokens),
        };

        let full = rested(called_at(limit), vec![answer("a"), answer("b")]).expect("a full turn records");
        assert_eq!(
            full.items(),
            [
                user(),
                TurnItem::message(Role::Assistant, Ref::of_text("Checking.")),
                TurnItem::Call(calls().as_slice()[0].clone()),
                TurnItem::Call(calls().as_slice()[1].clone()),
                answer("a").item(),
                answer("b").item(),
            ]
        );
        assert_eq!(*full.rested(), RestReason::ContextFull);

        let below = rested(called_at(limit - 1), vec![answer("a"), answer("b")]).expect("a turn below records");
        assert_eq!(*below.rested(), RestReason::TurnLimit);
    }

    #[test]
    fn each_end_variant_records_its_calls_outputs_and_text_last_with_its_reason() {
        // Catches a variant mapped to the wrong reason or a dropped closing text.
        let turn = turn(vec![user()]);
        let rested = |ending: Ending| {
            let outcome = TurnOutcome::Called {
                calls: end_calls("end", &ending),
                text: Ref::of_text("Finishing up."),
                usage: USAGE,
            };
            let outcome = result(outcome);
            let input = RecordInput::rested(
                Ref::of_encoded(&turn).expect("turn"),
                Ref::of_encoded(&outcome).expect("result"),
                vec![ended_answer("end", &ending)],
                tree(),
            );
            record_with(&turn, &outcome, &[ending], &input).expect("an ended turn records")
        };

        let ending = Ending::Done { summary: "All briefed changes are in the tree.".into() };
        let done = rested(ending.clone());
        assert_eq!(
            done.items(),
            [
                user(),
                TurnItem::message(Role::Assistant, Ref::of_text("Finishing up.")),
                TurnItem::Call(end_calls("end", &ending).as_slice()[0].clone()),
                ended_answer("end", &ending).item(),
                TurnItem::message(Role::Assistant, Ref::of_text("All briefed changes are in the tree.")),
            ]
        );
        assert_eq!(*done.rested(), RestReason::Completed);

        let ending = Ending::Blocked { reason: "The store is unreachable.".into() };
        let blocked = rested(ending);
        assert_eq!(*blocked.rested(), RestReason::Blocked);
        assert_eq!(
            blocked.items().last(),
            Some(&TurnItem::message(Role::Assistant, Ref::of_text("The store is unreachable.")))
        );

        let ending = Ending::Asked { question: "Which file holds the brief?".into() };
        let asked = rested(ending);
        assert_eq!(*asked.rested(), RestReason::Asked);
        assert_eq!(
            asked.items().last(),
            Some(&TurnItem::message(Role::Assistant, Ref::of_text("Which file holds the brief?")))
        );
    }

    #[test]
    fn an_end_call_without_a_readable_result_refuses() {
        // Catches a rest that guesses a reason when the end call's output is missing, refused, or of another kind.
        let turn = turn(vec![user()]);
        let ending = Ending::Done { summary: "All briefed changes are in the tree.".into() };
        let outcome =
            TurnOutcome::Called { calls: end_calls("end", &ending), text: Ref::of_text("Finishing up."), usage: USAGE };
        let outcome = result(outcome);
        let rested = |outputs| {
            let input = RecordInput::rested(
                Ref::of_encoded(&turn).expect("turn"),
                Ref::of_encoded(&outcome).expect("result"),
                outputs,
                tree(),
            );
            record(&turn, &outcome, &input)
        };

        assert!(matches!(rested(vec![answer("end")]), Err(Refusal::Refused { .. })), "a refused end output refuses");
        assert!(matches!(rested(vec![answer("other")]), Err(Refusal::Refused { .. })), "a missing end output refuses");
    }

    #[test]
    fn an_incomplete_turn_with_no_text_records_only_what_it_sent() {
        // Catches an empty assistant message recorded after a turn that spent its budget before any output, which
        // would leave a session a continue cannot resend as the turn sent it, and a partial reply dropped with it.
        let turn = turn(vec![user()]);
        let rested = |text: &str| {
            let result = result(TurnOutcome::Incomplete {
                text: Ref::of_text(text),
                reason: Detail::new("max_output_tokens"),
                usage: USAGE,
            });
            let input = RecordInput::rested(
                Ref::of_encoded(&turn).expect("turn"),
                Ref::of_encoded(&result).expect("result"),
                Vec::new(),
                tree(),
            );
            record(&turn, &result, &input).expect("an incomplete turn records")
        };

        let empty = rested("");
        assert_eq!(empty.items(), turn.items());
        assert_eq!(*empty.rested(), RestReason::Incomplete);
        let partial = rested("Half");
        assert_eq!(partial.items(), [user(), TurnItem::message(Role::Assistant, Ref::of_text("Half"))]);
    }

    #[test]
    fn a_failed_rest_records_the_turn_and_its_answered_calls_or_the_turn_alone_past_the_cap() {
        // Catches a failed rest that leaves a call unanswered, replays calls after the answered prefix, accepts
        // outputs out of order, or refuses a conversation it could record without the partial turn.
        let failure = Failure::Unbuilt { reason: Detail::new("unbuilt") };
        let failed = |turn: &TurnInput, outputs| {
            let result = result(called());
            let answered = Some(Answered::new(Ref::of_encoded(&result).expect("result"), outputs));
            let input = RecordInput::failed(Ref::of_encoded(turn).expect("turn"), failure.clone(), answered, tree());
            record(turn, &result, &input)
        };

        let short = turn(vec![user()]);
        let session = failed(&short, vec![answer("a")]).expect("an answered prefix records");
        assert_eq!(
            session.items(),
            [
                user(),
                TurnItem::message(Role::Assistant, Ref::of_text("Checking.")),
                TurnItem::Call(calls().as_slice()[0].clone()),
                answer("a").item(),
            ]
        );
        assert_eq!(*session.rested(), RestReason::Failed(failure.clone()));
        assert!(matches!(failed(&short, vec![answer("b")]), Err(Refusal::Refused { .. })), "a prefix answers in order");

        let full = turn(vec![user(); TurnItems::MAX_ITEMS]);
        let session = failed(&full, vec![answer("a")]).expect("a full turn records alone");
        assert_eq!(session.items(), full.items(), "past the cap the turn's own items are kept");
    }

    #[test]
    fn a_turn_failure_must_cite_a_turn_that_ended_the_session() {
        // Catches a failed rest written over a turn that would have rested the session, so a completed session could
        // be recorded as failed.
        let turn = turn(vec![user()]);
        let failed = |outcome| {
            let result = result(outcome);
            let failure = Failure::Turn { result: Ref::of_encoded(&result).expect("result") };
            record(&turn, &result, &RecordInput::failed(Ref::of_encoded(&turn).expect("turn"), failure, None, tree()))
        };

        let rejected = failed(TurnOutcome::Rejected).expect("a rejected turn records");
        assert_eq!(rejected.items(), turn.items());
        assert!(matches!(rejected.rested(), RestReason::Failed(Failure::Turn { .. })));
        let completed = TurnOutcome::Completed { text: Ref::of_text("done"), usage: USAGE };
        assert!(matches!(failed(completed), Err(Refusal::Refused { .. })), "a resting outcome refuses");
    }
}
