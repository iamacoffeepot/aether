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
//! [`OfferedTool`], beside the program's input and result schemas
//! (`ToolSchema::of`); nothing defaults to every declared program. A reply
//! that asks for calls is [`TurnOutcome::Called`], each [`ToolCall`] naming
//! an offered program, citing its arguments verbatim, and citing a
//! [`ToolInput`]: the arguments decoded against the offered input schema and
//! stored under the input's kind, or the text of the refused decode. The
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

pub use input::{
    CallId, CallIdError, Endpoint, EndpointError, ModelName, ModelNameError, OfferedTool, OfferedTools,
    OfferedToolsError, OutputBudget, OutputBudgetError, ReasoningEffort, Role, ToolCall, ToolCalls, ToolCallsError,
    ToolInput, ToolOutput, TurnInput, TurnItem, TurnItems, TurnItemsError,
};
pub use program::MuseTurn;
pub use result::{HttpStatus, HttpStatusError, TurnOutcome, TurnResult, TurnUsage};

aether_actor::export!(public = [MuseTurn], generators = [aether_bloomery_program::bundle]);
