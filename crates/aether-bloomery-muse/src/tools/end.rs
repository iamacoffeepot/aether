//! `muse.end`: end the session's run as done, blocked, or asking a question.

use aether_bloomery_kinds::{Mode, Refusal};
use aether_bloomery_program::{Env, Program, Sync, Tooled, program};
use aether_data::Kind;

use crate::input::{ToolCall, ToolInput, ToolOutput};
use crate::session::CallAnswer;
use crate::session::RestReason;

/// The user message the loop answers a reply without calls with.
pub const NUDGE_TEXT: &str =
    "A reply without a tool call does not end the session; continue the work or call `muse-end` to end your run.";

/// How the run ends, as the model writes it: one word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Storage)]
enum EndingTag {
    /// Every briefed change is in the tree.
    Done,
    /// The work cannot be finished.
    Blocked,
    /// The work cannot go on without the answer to one question.
    Asked,
}

/// What `muse.end` is called with: how the run ends, and its text.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.end.args")]
pub struct EndArgs {
    /// How the run ends: `Done`, `Blocked`, or `Asked`.
    ending: EndingTag,
    /// For `Done`, a summary of what changed. For `Blocked`, what stopped the work. For `Asked`, the one question
    /// that blocks the work.
    text: String,
}

impl EndArgs {
    /// End the run as `ending`.
    #[must_use]
    pub fn new(ending: Ending) -> Self {
        match ending {
            Ending::Done { summary } => Self { ending: EndingTag::Done, text: summary },
            Ending::Blocked { reason } => Self { ending: EndingTag::Blocked, text: reason },
            Ending::Asked { question } => Self { ending: EndingTag::Asked, text: question },
        }
    }

    /// The recorded ending holding the written text.
    #[must_use]
    pub fn into_ending(self) -> Ending {
        match self.ending {
            EndingTag::Done => Ending::Done { summary: self.text },
            EndingTag::Blocked => Ending::Blocked { reason: self.text },
            EndingTag::Asked => Ending::Asked { question: self.text },
        }
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
/// work cannot go on without the answer to one question. The call takes
/// `ending`, one of `Done`, `Blocked`, or `Asked`, and `text`, a plain string.
#[program]
impl Program for End {
    const NAME: &'static str = "muse.end";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "End the session's run as done, blocked, or asking a question.";
    type Input = Tooled<EndArgs>;
    type Result = Ending;

    fn run(input: Self::Input, env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        Ok(env.injected(input.args())?.into_ending())
    }
}

/// The position of the first decoded `muse.end` call in `calls`, if any.
pub fn end_position(calls: &[ToolCall]) -> Option<usize> {
    calls
        .iter()
        .position(|call| matches!(call.input(), ToolInput::Decoded { program, .. } if program.as_str() == End::NAME))
}

/// Whether the turn ended its run: the first decoded `muse.end` call in
/// `calls` is answered in `outputs` by an ending.
pub fn ends_run(calls: &[ToolCall], outputs: &[CallAnswer]) -> bool {
    let Some(answer) = end_position(calls).and_then(|position| outputs.get(position)) else {
        return false;
    };
    matches!(answer.output(), ToolOutput::Result { result, .. } if result.kind() == Ending::ID)
}

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::ProgramName;
    use aether_bloomery_program::{Program, ToolSchema, function_name, tool_definition};
    use aether_data::Storage;

    use super::{End, EndArgs, Ending};
    use crate::arguments::decode;

    #[test]
    fn the_program_resolves_to_muse_end() {
        // Tripwire: the `muse open` preface names `muse-end`, so a rename has to change both.
        let program = ProgramName::new(End::NAME).expect("program name");
        assert_eq!(function_name(&program).expect("function name"), "muse-end");
        let definition = tool_definition::<End>().expect("renders");
        assert_eq!(definition["name"], "muse-end");
    }

    #[test]
    fn each_flat_end_call_decodes_to_the_ending_of_its_tag_with_its_text() {
        // Catches a tag mapped to another variant (a `Blocked` reason recorded as a `Done` summary would run the
        // gate and rest the session completed) and a text altered on the way.
        for (tag, ending) in [
            ("Done", Ending::Done { summary: "say \"hi\"\nbye".to_owned() }),
            ("Blocked", Ending::Blocked { reason: "say \"hi\"\nbye".to_owned() }),
            ("Asked", Ending::Asked { question: "say \"hi\"\nbye".to_owned() }),
        ] {
            let arguments = serde_json::json!({ "ending": tag, "text": ending.text() }).to_string();
            let bytes = decode(&arguments, &ToolSchema::of::<EndArgs>()).expect("the call decodes");
            let args = EndArgs::decode_storage(&bytes).expect("the bytes decode").value;
            assert_eq!(args.into_ending(), ending, "{tag}");
        }
    }
}
