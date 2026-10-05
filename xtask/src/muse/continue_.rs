//! `muse continue`: resume a rested session from its latest record through
//! `muse.session.continue`.
//!
//! With no message the conversation is resent as it stands, which recovers a
//! session that rested failed, such as on a vendor 504, or incomplete with no
//! output.

use std::fs;
use std::path::PathBuf;

use aether_bloomery_kinds::{EncodedArtifact, RecordedHead};
use aether_bloomery_muse::{ContinueInput, SessionContinue, SessionKey};
use aether_data::Ref;
use anyhow::{Context, Result, anyhow};
use clap::Args;

use super::call::call;
use super::{EngineArgs, budget, turn_limit};
use crate::bloomery::latest_moves;

/// Arguments for `cargo xtask muse continue`.
#[derive(Args, Debug)]
pub(super) struct ContinueArgs {
    #[command(flatten)]
    engine: EngineArgs,
    /// The session to continue: the key `open` printed.
    #[arg(long)]
    session: u64,
    /// A file holding the next user message; without it the conversation is
    /// resent as it stands.
    #[arg(long)]
    message: Option<PathBuf>,
    /// The most turns the session may make before it rests again.
    #[arg(long)]
    max_turns: u32,
    /// Replaces the session's output budget from this turn on.
    #[arg(long)]
    max_output_tokens: Option<u32>,
}

/// Continue the session from its latest record and print `after=<seq>`, the
/// boundary `wait` reads from.
pub(super) fn run(args: &ContinueArgs) -> Result<()> {
    let message = args
        .message
        .as_ref()
        .map(|path| fs::read_to_string(path).with_context(|| format!("reading the message {}", path.display())))
        .transpose()?;
    let session = SessionKey::new(args.session);
    let mut engine = args.engine.connect()?;

    let from = latest_moves(&mut engine, &[RecordedHead::from(&session.head())])?
        .into_iter()
        .flatten()
        .next()
        .ok_or_else(|| anyhow!("session {} has no record yet: wait for it to rest first", args.session))?;

    let input = ContinueInput::new(
        session,
        Ref::from_digest(from),
        message.as_deref().map(Ref::of_text),
        args.max_output_tokens.map(budget).transpose()?,
        turn_limit(args.max_turns)?,
    );
    let staged = EncodedArtifact::new(&input)?;
    let input = Ref::from_digest(staged.digest());
    engine.stage_artifacts(message.as_deref().map(EncodedArtifact::text).into_iter().chain([staged]).collect())?;

    let run = call::<SessionContinue>(&mut engine, input)?;
    println!("after={}", run - 1);
    Ok(())
}
