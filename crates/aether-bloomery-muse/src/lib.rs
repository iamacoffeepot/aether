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
//! reported. Any reply is a recorded result; only no reply at all refuses,
//! which the driver records as a fault.
//!
//! A tool is a program to run. The caller renders each offered program with
//! `aether_bloomery_program::tool_definition` and cites the definition in an
//! [`OfferedTool`], beside the schemas of the program's arguments (the `A` of
//! its `Tooled<A>` input) and of its result (`ToolSchema::of`); nothing
//! defaults to every declared program. A reply
//! that asks for calls is [`TurnOutcome::Called`], each [`ToolCall`] citing
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
//! reactor, `MuseSession`, exported from this same bundle. `muse.session.open`
//! opens one on a tree and `muse.session.continue` resumes one; the loop runs
//! each decoded call through a bound tool one at a time, over the session's
//! current tree, sends the next turn with the calls and their outputs
//! appended, and at each rest writes the conversation and the latest tree
//! down as a [`Session`] (`muse.session`) through `muse.session.record`,
//! moving the session's head to it. The bound tools are [`TreeEdit`]
//! (`tree.edit`) and [`TreeWrite`] (`tree.write`), which return an `Edited`
//! tree the loop carries to the next call, and the fixture [`Echo`]
//! (`muse.echo`). Each open and
//! continue states its own [`TurnLimit`]; a session that reaches it rests with
//! [`RestReason::TurnLimit`].

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
    CallId, CallIdError, Endpoint, EndpointError, FunctionName, FunctionNameError, ModelName, ModelNameError,
    OfferedTool, OfferedTools, OfferedToolsError, OutputBudget, OutputBudgetError, ReasoningEffort, Role, ToolCall,
    ToolCalls, ToolCallsError, ToolInput, ToolOutput, TurnInput, TurnItem, TurnItems, TurnItemsError,
};
pub use program::MuseTurn;
pub use result::{HttpStatus, HttpStatusError, TurnOutcome, TurnResult, TurnUsage};
pub use session::{
    CallAnswer, ContinueInput, MUSE, MuseSession, OpenInput, RecordInput, RestReason, Session, SessionContinue,
    SessionItems, SessionItemsError, SessionKey, SessionOpen, SessionRecord, TurnLimit, TurnLimitError, TurnSettings,
};
pub use tools::{Echo, EchoArgs, EchoResult, EditArgs, MAX_TEXT_BYTES, TreeEdit, TreeWrite, WriteArgs, offered};

aether_actor::export!(
    public = [MuseTurn, SessionOpen, SessionContinue, SessionRecord, Echo, TreeEdit, TreeWrite, MuseSession],
    generators = [aether_bloomery_program::bundle],
);
