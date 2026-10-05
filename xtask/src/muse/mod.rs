//! `cargo xtask muse <verb>`: drive Muse sessions on a Bloomery engine the
//! hub runs, one short command per step, so long waits and file bytes stay
//! out of the caller's context.
//!
//! Every verb dials the engine's RPC port and addresses one unit's journal
//! owner and the bundle driver over the shared [`crate::bloomery`] client.
//!
//! - [`activation`] reads from the journal whether the muse reactor is live,
//!   so `bind` and `open` report a reactor that failed to activate.
//! - [`bind`] binds the muse bundle and the reactor set that runs its session
//!   loop, once per engine.
//! - [`open`] stages a tree, instructions, a brief, and seeded reads, and
//!   opens a session.
//! - [`continue_`] resumes a rested session with a message, or resends it.
//! - [`wait`] follows one session to its next rest and prints the rest, the
//!   tree it moved from and to, and the usage its turns reported.
//! - [`export`] writes the difference between two trees into a directory.
//!
//! `open` and `continue` call their program under a key derived from the
//! input's digest, so a retry after a lost reply replays the recorded outcome
//! instead of opening or continuing twice. Two opens with the same tree,
//! instructions, brief, seeds, and settings therefore name the same session.

mod activation;
mod bind;
mod continue_;
mod export;
mod open;
mod wait;

#[cfg(test)]
mod tests;

use aether_bloomery_kinds::{Call, CallOutcome, NativeOrigin, ProgramName, UnitKey};
use aether_bloomery_muse::{
    Endpoint, InputLimit, MUSE, ModelName, OfferedTools, OutputBudget, ReasoningEffort, TurnLimit, TurnSettings,
};
use aether_bloomery_program::Program;
use aether_data::Ref;
use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand, ValueEnum};

use crate::bloomery::{self, Engine};

/// The origin every call these verbs make names; the driver keys replays by
/// it and the call key together.
const ORIGIN: &str = "xtask.muse";

/// Arguments for `cargo xtask muse`.
#[derive(Args, Debug)]
pub struct MuseArgs {
    #[command(subcommand)]
    verb: Verb,
}

#[derive(Subcommand, Debug)]
enum Verb {
    /// Bind the muse bundle and a reactor set holding it, unless both heads
    /// already name them. Prints `bound` or `unchanged` with both digests, and
    /// fails with the driver's recorded reason when the reactor is not live.
    Bind(bind::BindArgs),
    /// Open a session on a commit's tree, or a stored tree, with instructions,
    /// a brief, and seeded reads. Prints `tree=`, `session=`, and `after=`.
    /// Refuses when the muse reactor is not live.
    Open(open::OpenArgs),
    /// Continue a rested session with a message, or resend its conversation
    /// as it stands. Prints `after=`.
    Continue(continue_::ContinueArgs),
    /// Follow a session from `--after` to its next rest and print it.
    Wait(wait::WaitArgs),
    /// Write the difference between two trees into a directory and print
    /// `A|M|D <path>` for each change, in path order.
    Export(export::ExportArgs),
}

/// Run one muse verb.
///
/// # Errors
/// The verb failed; its error says where.
pub fn run(args: &MuseArgs) -> Result<()> {
    match &args.verb {
        Verb::Bind(args) => bind::run(args),
        Verb::Open(args) => open::run(args),
        Verb::Continue(args) => continue_::run(args),
        Verb::Wait(args) => wait::run(args),
        Verb::Export(args) => export::run(args),
    }
}

/// The engine every verb dials.
#[derive(Args, Debug)]
struct EngineArgs {
    /// The Bloomery engine's RPC port on 127.0.0.1.
    #[arg(long)]
    rpc_port: u16,
    /// The unit key of the journal the sessions live in: the engine's journal
    /// owner for it answers at `aether.bloomery.journal:<key>`.
    #[arg(long)]
    unit: String,
}

impl EngineArgs {
    /// Install the frame cap and dial the engine.
    fn connect(&self) -> Result<Engine> {
        let unit = UnitKey::new(&self.unit).with_context(|| format!("--unit {:?} is not a unit key", self.unit))?;
        bloomery::install_frame_cap()?;
        Engine::connect(self.rpc_port, &unit, "xtask muse")
    }
}

/// What every turn of an opened session sends besides its conversation.
#[derive(Args, Debug)]
struct SettingsArgs {
    /// The responses endpoint each turn posts to, `https://` or `http://`.
    #[arg(long)]
    endpoint: String,
    /// The model that answers.
    #[arg(long)]
    model: String,
    /// How much the model reasons before it answers.
    #[arg(long, value_enum)]
    effort: Effort,
    /// The most output tokens, reasoning included, one turn may produce.
    #[arg(long)]
    max_output_tokens: u32,
    /// The most input tokens a turn may be billed for before the session
    /// rests `ContextFull`.
    #[arg(long)]
    input_limit: u64,
}

impl SettingsArgs {
    /// The settings, offering `tools`.
    fn settings(&self, tools: OfferedTools) -> Result<TurnSettings> {
        Ok(TurnSettings::new(
            Endpoint::new(self.endpoint.as_str()).map_err(|error| anyhow!("--endpoint: {error}"))?,
            ModelName::new(self.model.as_str()).map_err(|error| anyhow!("--model: {error}"))?,
            tools,
            budget(self.max_output_tokens)?,
            self.effort.into(),
            input_limit(self.input_limit)?,
        ))
    }
}

/// `--effort`: how much the model reasons before it answers.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum Effort {
    Low,
    Medium,
    High,
    #[value(name = "xhigh")]
    XHigh,
    Max,
}

impl From<Effort> for ReasoningEffort {
    fn from(effort: Effort) -> Self {
        match effort {
            Effort::Low => Self::Low,
            Effort::Medium => Self::Medium,
            Effort::High => Self::High,
            Effort::XHigh => Self::XHigh,
            Effort::Max => Self::Max,
        }
    }
}

/// `--max-output-tokens` as a budget.
fn budget(tokens: u32) -> Result<OutputBudget> {
    OutputBudget::new(tokens).map_err(|error| anyhow!("--max-output-tokens: {error}"))
}

/// `--input-limit` as a limit: a value of 0 is refused.
fn input_limit(tokens: u64) -> Result<InputLimit> {
    InputLimit::new(tokens).map_err(|error| anyhow!("--input-limit: {error}"))
}

/// `--max-turns` as a limit.
fn turn_limit(turns: u32) -> Result<TurnLimit> {
    TurnLimit::new(turns).map_err(|error| anyhow!("--max-turns: {error}"))
}

/// Ask the driver to run `P` from the bundle [`MUSE`] resolves to over the
/// stored `input`, and return the seq of the run's recorded transition.
///
/// The call key is the first eight bytes of the input's digest, so a retried
/// call with the same input replays the outcome the first one recorded.
///
/// # Errors
/// The driver refused the call, the run faulted, or the transport failed.
fn call<P: Program>(engine: &mut Engine, input: Ref<P::Input>) -> Result<u64> {
    let digest = input.digest();
    let (key, _) = digest.as_bytes().split_first_chunk::<8>().context("a digest holds 32 bytes")?;
    let call = Call {
        program: MUSE,
        name: ProgramName::new(P::NAME).map_err(|error| anyhow!("program name {:?}: {error}", P::NAME))?,
        input: digest,
        origin: NativeOrigin::new(ORIGIN).map_err(|error| anyhow!("origin {ORIGIN:?}: {error}"))?,
        key: u64::from_le_bytes(*key),
    };

    match engine.call_program(&call)? {
        CallOutcome::Transition { seq, .. } => Ok(seq),
        CallOutcome::Fault { seq, fault, .. } => bail!("{} faulted at seq {seq}: {:?}", P::NAME, fault.reason),
        CallOutcome::Refused { reason, .. } => bail!("the driver refused {}: {reason:?}", P::NAME),
    }
}
