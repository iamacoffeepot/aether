//! `muse.echo`: a value-only fixture tool that repeats its text back.

use aether_bloomery_kinds::{Mode, Refusal};
use aether_bloomery_program::{Env, Program, Sync, Tooled, program};

/// What `muse.echo` is asked to repeat.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.echo.args")]
pub struct EchoArgs {
    /// The text to repeat back unchanged.
    text: String,
}

impl EchoArgs {
    /// Ask for `text` back.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

/// What `muse.echo` repeated.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.echo.result")]
pub struct EchoResult {
    /// The text it was asked to repeat, unchanged.
    text: String,
}

impl EchoResult {
    /// The repeated text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// The `muse.echo` program.
pub struct Echo;

/// Repeats the given text back unchanged.
#[program]
impl Program for Echo {
    const NAME: &'static str = "muse.echo";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Repeat the given text back unchanged.";
    type Input = Tooled<EchoArgs>;
    type Result = EchoResult;

    fn run(input: Self::Input, env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        Ok(EchoResult { text: env.injected(input.args())?.text })
    }
}
