//! `muse.session.exhausted`: the answer to a tool call whose every attempt ran
//! out of the executor's time or memory (ADR-0234 decision 10).
//!
//! An exhausted run says nothing about the model's code, and the executor
//! grows its estimate for that run after each exhaustion (ADR-0237 decision
//! 9), so the loop requests the same run again, up to [`MAX_TOOL_RETRIES`]
//! times. After that it answers the call with the text this program stages,
//! and the session goes on.

use aether_bloomery_kinds::{FaultReason, Mode, ProgramName, Ref, Refusal, Utf8Text};
use aether_bloomery_program::{Env, Program, Sync, program};

/// The most times the loop requests a tool run again after it ran out of
/// time or memory: three attempts in all.
pub const MAX_TOOL_RETRIES: u32 = 2;

/// The executor allotment a tool run ran out of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Storage)]
pub enum Exhaustion {
    /// The run's time allotment ran out.
    Time,
    /// The run's memory allotment ran out.
    Memory,
}

impl Exhaustion {
    /// The allotment `reason` says a run ran out of, or `None` for a fault
    /// that is not an exhaustion.
    #[must_use]
    pub const fn of(reason: &FaultReason) -> Option<Self> {
        match reason {
            FaultReason::TimedOut => Some(Self::Time),
            FaultReason::ResourceExhausted => Some(Self::Memory),
            _ => None,
        }
    }

    const fn noun(self) -> &'static str {
        match self {
            Self::Time => "time",
            Self::Memory => "memory",
        }
    }
}

/// A tool whose every attempt ran out of an allotment: the program, the
/// allotment the last attempt ran out of, and how many attempts ran.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.session.exhausted.input")]
pub struct ExhaustedInput {
    /// The tool's program.
    program: ProgramName,
    /// The allotment the last attempt ran out of.
    resource: Exhaustion,
    /// How many attempts ran.
    attempts: u32,
}

impl ExhaustedInput {
    /// `program` ran out of `resource` on the last of `attempts` attempts.
    #[must_use]
    pub const fn new(program: ProgramName, resource: Exhaustion, attempts: u32) -> Self {
        Self { program, resource, attempts }
    }
}

/// The staged refusal that answers an exhausted tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.session.exhausted")]
pub struct Exhausted {
    /// The cited text the call is answered with.
    refusal: Ref<Utf8Text>,
}

impl Exhausted {
    /// The cited text the call is answered with.
    #[must_use]
    pub const fn refusal(&self) -> Ref<Utf8Text> {
        self.refusal
    }
}

/// The `muse.session.exhausted` program.
pub struct SessionExhausted;

/// Stages the text that answers a tool call whose every attempt ran out of
/// time or memory: the tool, the allotment, and the attempts, as in
/// "`proof.clippy` ran out of time after 3 attempts".
#[program]
impl Program for SessionExhausted {
    const NAME: &'static str = "muse.session.exhausted";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Stage the answer to a tool call that ran out of time or memory on every attempt.";
    type Input = ExhaustedInput;
    type Result = Exhausted;

    fn run(input: Self::Input, env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        let ExhaustedInput { program, resource, attempts } = input;
        let text = format!("`{}` ran out of {} after {attempts} attempts", program.as_str(), resource.noun());
        Ok(Exhausted { refusal: env.stage_text(&text) })
    }
}

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{ProgramName, Ref};

    use super::{ExhaustedInput, Exhaustion, SessionExhausted};
    use crate::session::fixture::run;

    #[test]
    fn the_answer_names_the_tool_the_allotment_and_the_attempts() {
        // Catches an answer that names the wrong allotment, tool, or count, which the model reads to decide whether
        // to shrink its work or end the run blocked.
        let program = ProgramName::new("proof.clippy").expect("program");
        for (resource, text) in [
            (Exhaustion::Time, "`proof.clippy` ran out of time after 3 attempts"),
            (Exhaustion::Memory, "`proof.clippy` ran out of memory after 3 attempts"),
        ] {
            let exhausted = run::<SessionExhausted>(&ExhaustedInput::new(program.clone(), resource, 3), Vec::new())
                .expect("an exhausted call is answered");
            assert_eq!(exhausted.refusal(), Ref::of_text(text));
        }
    }
}
