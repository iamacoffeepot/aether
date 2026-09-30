//! The programs a session binds as tools, and how the loop calls a program.
//!
//! The loop runs a call by the program name and decoded input the call
//! recorded, and folds any program's run as that call's output, so it links
//! no tool's types. [`offered`] is the set `muse.session.open` accepts: for
//! now only `Echo`, a value-only fixture that stands in until `tree.read`
//! replaces it.

use std::iter;

use aether_bloomery_kinds::{
    CallInput, CallProgram, EncodedArtifact, Head, Mode, OpaqueBytes, ProgramName, Ref, Refusal,
};
use aether_bloomery_program::{Env, Program, Sync, ToolSchema, program, tool_definition};
use aether_data::Schema;

use crate::input::{OfferedTool, OfferedTools};

/// The head every program the loop calls resolves through: the bundle this
/// crate builds.
pub const MUSE: Head<OpaqueBytes> = Head::new("muse");

/// What `muse.echo` is asked to repeat.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.echo.input")]
pub struct EchoInput {
    /// The text to repeat back unchanged.
    text: String,
}

impl EchoInput {
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
    type Input = EchoInput;
    type Result = EchoResult;

    fn run(input: Self::Input, _env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        Ok(EchoResult { text: input.text })
    }
}

/// Every bound tool offered as a session turn offers it, and the artifacts
/// those offers cite: each definition and each input and result schema, for a
/// caller that opens a session to stage.
///
/// # Panics
///
/// When a bound tool does not render as a tool or its schema does not encode,
/// which holds or fails the same way on every call.
#[must_use]
pub fn offered() -> (OfferedTools, Vec<EncodedArtifact>) {
    let (tool, artifacts) = bound::<Echo>();
    (OfferedTools::new(vec![tool]).expect("one bound tool keeps every tool list rule"), artifacts)
}

/// `P` as a bound tool, and the artifacts its offer cites.
fn bound<P: Program>() -> (OfferedTool, Vec<EncodedArtifact>)
where
    P::Input: Schema,
    P::Result: Schema,
{
    let definition = tool_definition::<P>().expect("a bound tool renders as a tool").to_string();
    let schemas = [ToolSchema::of::<P::Input>(), ToolSchema::of::<P::Result>()]
        .map(|schema| EncodedArtifact::new(&schema).expect("a tool schema encodes"));
    let [input, result] = schemas.each_ref().map(|schema| Ref::from_digest(schema.digest()));
    let tool = OfferedTool::new(program_name::<P>(), Ref::of_text(&definition), input, result);
    (tool, iter::once(EncodedArtifact::text(&definition)).chain(schemas).collect())
}

/// A call to the loop's own program `P` in the bundle [`MUSE`] resolves to,
/// over `input`.
pub fn call<P: Program>(input: CallInput) -> CallProgram {
    CallProgram { program: MUSE, name: program_name::<P>(), input }
}

fn program_name<P: Program>() -> ProgramName {
    ProgramName::new(P::NAME).expect("`#[program]` refuses an invalid NAME at compile time")
}
