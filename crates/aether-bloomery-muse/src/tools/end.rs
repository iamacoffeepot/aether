//! `muse.end`: end the session's run as done, blocked, or asking a question.

use aether_bloomery_kinds::{ErasedRef, Mode, Refusal};
use aether_bloomery_program::{Env, Program, Sync, Tooled, program};
use aether_data::Kind;

use crate::input::{ToolCall, ToolInput, ToolOutput};
use crate::session::CallAnswer;
use crate::session::RestReason;

/// The user message the loop answers a reply without calls with.
pub const NUDGE_TEXT: &str =
    "A reply without a tool call does not end the session; continue the work or call `muse-end` to end your run.";

/// What `muse.end` is called with: how the run ends.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.end.args")]
pub struct EndArgs {
    /// How the run ends: done, blocked, or asking a question.
    ending: Ending,
}

impl EndArgs {
    /// End the run as `ending`.
    #[must_use]
    pub const fn new(ending: Ending) -> Self {
        Self { ending }
    }
}

/// How the model ends its run, as its arguments' ending and as the recorded
/// result.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.end.ending")]
pub enum Ending {
    /// The work is done.
    Done {
        /// A summary of what changed.
        summary: String,
    },
    /// The work cannot be finished.
    Blocked {
        /// What stopped the work.
        reason: String,
    },
    /// The work needs an answer first.
    Asked {
        /// The one question that blocks the work.
        question: String,
    },
}

impl Ending {
    /// The summary, reason, or question the model wrote.
    #[must_use]
    pub fn text(&self) -> &str {
        match self {
            Self::Done { summary } => summary,
            Self::Blocked { reason } => reason,
            Self::Asked { question } => question,
        }
    }

    /// The reason a run that ends this way rests.
    #[must_use]
    pub const fn rest_reason(&self) -> RestReason {
        match self {
            Self::Done { .. } => RestReason::Completed,
            Self::Blocked { .. } => RestReason::Blocked,
            Self::Asked { .. } => RestReason::Asked,
        }
    }
}

/// The `muse.end` program.
pub struct End;

/// Ends the model's own run. Call it only to end the run: `Done` only when
/// every briefed change is in the tree, `Blocked` when the work cannot be
/// finished with the offered tools or as planned, and `Asked` only when the
/// work cannot go on without the answer to one question.
#[program]
impl Program for End {
    const NAME: &'static str = "muse.end";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "End the session's run as done, blocked, or asking a question.";
    type Input = Tooled<EndArgs>;
    type Result = Ending;

    fn run(input: Self::Input, env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        Ok(env.injected(input.args())?.ending)
    }
}

/// The position of the first decoded `muse.end` call in `calls`, if any.
pub fn end_position(calls: &[ToolCall]) -> Option<usize> {
    calls
        .iter()
        .position(|call| matches!(call.input(), ToolInput::Decoded { program, .. } if program.as_str() == End::NAME))
}

/// The cited `Ending` answering the first decoded `muse.end` call, when one
/// of `calls` is decoded and the output answering it is a stored result of
/// the ending's kind.
pub fn end_result(calls: &[ToolCall], outputs: &[CallAnswer]) -> Option<ErasedRef> {
    let output = outputs.get(end_position(calls)?)?.output();
    match output {
        ToolOutput::Result { result, .. } if result.kind() == Ending::ID => Some(*result),
        ToolOutput::Result { .. } | ToolOutput::Refused(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::ProgramName;
    use aether_bloomery_program::{Program, function_name, tool_definition};

    use super::End;

    #[test]
    fn the_program_resolves_to_muse_end() {
        // Tripwire: the `muse open` preface names `muse-end`, so a rename has to change both.
        let program = ProgramName::new(End::NAME).expect("program name");
        assert_eq!(function_name(&program).expect("function name"), "muse-end");
        let definition = tool_definition::<End>().expect("renders");
        assert_eq!(definition["name"], "muse-end");
    }
}
