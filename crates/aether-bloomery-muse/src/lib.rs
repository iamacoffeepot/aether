//! The `muse.turn` bloomery program: one stateless responses-API turn per
//! run (ADR-0234).
//!
//! A turn's input ([`TurnInput`], `muse.turn.input`) names the endpoint, the
//! model, the programs offered as tools, the whole conversation as a flat
//! list of cited items, the output budget, and the reasoning effort. The
//! program reads everything those cite, sends one `Fetch` with `store: false`, the
//! offered tool definitions, and the full conversation, and records the reply
//! as a [`TurnResult`] (`muse.turn.result`): the HTTP status, the raw body
//! (always kept), and a [`TurnOutcome`] with every usage count the vendor
//! reported. Any reply is a recorded result. A fetch that timed out or failed
//! at the connection is a recorded result with no reply whose outcome reads as
//! `Transient`, so the session resends the turn; only a failure with no reply
//! that a resend cannot clear (an allowlist denial, disabled egress, an invalid
//! URL, a body too large, a closed capability) refuses, which the driver
//! records as a fault. The fetch waits 180 s, or 600 s at High reasoning.
//!
//! A tool is a program to run. The caller renders each offered program with
//! `aether_bloomery_program::tool_definition` and cites the definition in an
//! [`OfferedTool`], beside the schemas of the program's arguments (the `A` of
//! its `Tooled<A, B>` input) and of its result (`ToolSchema::of`), and the
//! bound value `B` the loop binds into every call, which the model never
//! sees; nothing defaults to every declared program. A reply that asks for calls is [`TurnOutcome::Called`], each [`ToolCall`] citing
//! its arguments verbatim and holding a [`ToolInput`]: a call whose name is an
//! offered program's function name and whose arguments decode against the
//! offered arguments schema carries the program and the arguments stored
//! under their kind; any other call is refused under the name the model wrote,
//! with the text of the refusal (`no such tool: <name>`, or the refused
//! decode), so the model sees its mistake on the next turn. The
//! program never runs a call. A later turn replays the call as
//! [`TurnItem::Call`] and its [`ToolOutput`] as [`TurnItem::CallOutput`]:
//! a stored result, which the program renders to JSON with the result schema
//! the output cites, or the refusal text, sent as stored.
//!
//! The program links no tool's types; everything it decodes or renders with
//! is cited. The rendering is deterministic, so the request a turn sends is a
//! function of its cited input alone. A decoded input carries no citations,
//! since a schema does not mark a reference field, and a tool input's
//! `#[storage(validate)]` invariant is not checked by the decode: a tool
//! meets a violating input in its own run.
//!
//! The program sets no credential header, and no credential enters a kind,
//! the journal, the recorded closure, the bundle, or mail the program builds.
//! A credential comes from the engine secrets mechanism (ADR-0235), never from
//! the program: the operator binds the endpoint's host with the http
//! capability's `--http-secrets` knob beside `--http-allowlist` and
//! `--secrets-dir`, and `aether.http` attaches the bound header to requests for
//! exactly that host, over HTTPS only.
//!
//! A session loops over turns and the calls they ask for as a bloomery
//! reactor, `MuseSession`, exported from this same bundle.
//! `muse.session.open` opens one on a tree with the session instructions,
//! optionally with seeded `tree.read` calls the loop runs before the first
//! turn, and `muse.session.continue` resumes one; the loop runs each decoded
//! call through a bound tool one at a time, in the bundle its offer names,
//! over the session's current tree and the bound value its offer carries,
//! sends the next turn with the calls and their outputs appended, and at each
//! rest writes the conversation and
//! the latest tree down as a [`Session`] (`muse.session`) through
//! `muse.session.record`, moving the session's head to it. The bound tools are
//! [`TreeEdit`] (`tree.edit`) and [`TreeWrite`] (`tree.write`), which return an `Edited`
//! tree the loop carries to the next call; [`TreeList`] (`tree.list`),
//! [`TreeRead`] (`tree.read`), and [`TreeGrep`] (`tree.grep`), which return
//! the text they read as [`Viewed`] and leave the tree as it was; [`End`]
//! (`muse.end`), which ends the run as done, blocked, or asking a question;
//! and the fixture [`Echo`] (`muse.echo`). A session opened with
//! [`offered_with_proofs`] also offers `proof.clippy` and `proof.test` from
//! the `workspace-programs` bundle, bound to its environment, vendor tree,
//! cargo config, and test env: the first formats the tree and checks it with
//! clippy, the second formats the tree and runs its workspace tests with the
//! session's test env, and each one's `Edited` tree the loop carries on like
//! an edit's.
//! Only a `muse.end` call ends a run:
//! a reply without a call is nudged back for another turn. Each open and
//! continue states its own [`TurnLimit`]; a session that reaches it rests with
//! [`RestReason::TurnLimit`]. Each turn carries an [`InputLimit`]; a called
//! turn that reached it rests with [`RestReason::ContextFull`] instead of
//! sending its next turn, once its calls ran, and a reply without a call past
//! the input limit rests with [`RestReason::ContextFull`], or with
//! [`RestReason::TurnLimit`] at the turn limit.
//! A turn the vendor refuses as transient is sent again, byte-identical,
//! after a wait on the driver's clock (ADR-0245), a few times at most.
//! A session that fails (a faulted run, a failed rule, a turn the vendor
//! ended, or a request the loop cannot build) rests with
//! [`RestReason::Failed`] naming the [`Failure`], so every activation ends
//! with one head move, and a continue picks it up like any other rest.

/// Implement [`aether_data::Invariant`], `Display`, and `Error` for error
/// enums that carry a `const fn reason(self) -> &'static str`.
macro_rules! invariant_errors {
    ($($error:ty),+ $(,)?) => {$(
        impl ::aether_data::Invariant for $error {
            fn reason(&self) -> &'static str {
                Self::reason(*self)
            }
        }

        impl ::core::fmt::Display for $error {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.write_str(Self::reason(*self))
            }
        }

        impl ::core::error::Error for $error {}
    )+};
}

mod arguments;
mod input;
mod program;
mod render;
mod request;
mod response;
mod result;
mod session;
mod tools;

pub use input::{
    CallId, CallIdError, Endpoint, EndpointError, FunctionName, FunctionNameError, InputLimit, InputLimitError,
    ModelName, ModelNameError, OfferedTool, OfferedTools, OfferedToolsError, OutputBudget, OutputBudgetError,
    ReasoningEffort, Role, ToolCall, ToolCalls, ToolCallsError, ToolInput, ToolOutput, TurnInput, TurnItem, TurnItems,
    TurnItemsError,
};
pub use program::MuseTurn;
pub use result::{HttpStatus, HttpStatusError, TurnOutcome, TurnResult, TurnUsage};
pub use session::{
    Answered, CallAnswer, ContinueInput, Exhausted, ExhaustedInput, Exhaustion, Failure, MAX_TOOL_RETRIES, MUSE,
    MuseSession, OpenInput, Opened, RecordInput, RestReason, Session, SessionContinue, SessionExhausted, SessionItems,
    SessionItemsError, SessionKey, SessionOpen, SessionRecord, TurnEnd, TurnLimit, TurnLimitError, TurnSettings,
};
pub use tools::{
    Echo, EchoArgs, EchoResult, EditArgs, End, EndArgs, Ending, GrepArgs, ListArgs, MAX_TEXT_BYTES, NUDGE_TEXT,
    ReadArgs, TreeEdit, TreeGrep, TreeList, TreeRead, TreeWrite, VIEW_MAX_BYTES, Viewed, WriteArgs, offered,
    offered_with_proofs,
};

aether_actor::export!(
    public = [
        MuseTurn,
        SessionOpen,
        SessionContinue,
        SessionRecord,
        SessionExhausted,
        Echo,
        End,
        TreeEdit,
        TreeWrite,
        TreeList,
        TreeRead,
        TreeGrep,
        MuseSession,
    ],
    generators = [aether_bloomery_program::bundle],
);
