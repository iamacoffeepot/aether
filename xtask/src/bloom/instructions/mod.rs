//! `cargo xtask bloom instructions` — assemble, validate, and record the
//! ADR-0214 model-process instruction bundle this repository's lanes run under.
//!
//! ADR-0214 §Migration wants the first real bundle imported from the existing
//! instruction files under an explicit operator authorization. This is the
//! import half, and only that half: it assembles the bundle from
//! [`bundle::imported`], refuses an incomplete one, and either prints the
//! `POST /configs` body or records it through that ordinary route. It never
//! authorizes anything.
//!
//! **Recording is not authorizing.** `POST /configs` stores content and hands
//! back its address; whether that address may serve as model-process policy is
//! the host's own answer, given by naming it in
//! `AETHER_BLOOMERY_AUTHORIZED_INSTRUCTIONS` and restarting the coordinator.
//! Knowing a digest, uploading it, or naming it in a request authorizes nothing
//! (ADR-0214 §"Model-process instructions are explicit configuration"), which is
//! why this command prints the address and stops.
//!
//! The address it prints is derived locally from the assembled value. When the
//! bundle is recorded, the coordinator's own address for the stored bytes is
//! compared against it and a mismatch is an error rather than a printed
//! surprise: the whole point of a content address is that both ends compute the
//! same one.

mod bundle;

pub(crate) use bundle::{RETROSPECT, RETROSPECT_FINDING_CONTRACT};

use std::fs;
use std::path::PathBuf;

use aether_bloomery::{ConfigKind, ModelProcessInstructions, ModelProcessInstructionsError};
use aether_data::Kind;
use anyhow::{Context, Result, bail};
use clap::Args;

use super::client::Client;
use super::dto::ConfigRequest;

/// Assemble the model-process instruction bundle (ADR-0214) from this
/// repository's own instruction sources and print its content address.
#[derive(Args, Debug)]
pub struct InstructionsArgs {
    /// Write the `POST /configs` body to this path.
    #[arg(long)]
    out: Option<PathBuf>,

    /// Record the bundle on the coordinator through `POST /configs`. Storing it
    /// does not authorize it: name the printed address in
    /// `AETHER_BLOOMERY_AUTHORIZED_INSTRUCTIONS` and restart the coordinator.
    #[arg(long)]
    record: bool,
}

/// Assemble, validate, and — as asked — write or record the bundle.
///
/// # Errors
/// The assembled bundle leaves a field empty, the body could not be written,
/// the coordinator refused the record, or its address for the stored bytes is
/// not the address of the value that was sent.
pub fn run(client: &Client<'_>, args: &InstructionsArgs) -> Result<String> {
    let bundle = bundle::imported();
    // Before anything is written or sent: an incomplete bundle is a bundle the
    // dispatch gate would refuse on every model lane, so printing its address
    // would hand the operator a digest to authorize that cannot dispatch.
    if let Err(ModelProcessInstructionsError::EmptyField(field)) = bundle.validate() {
        bail!("the assembled instruction bundle leaves `{field}` empty; it would refuse every model dispatch");
    }

    let address = bundle.address();
    let value = serde_json::to_value(&bundle).context("render the instruction bundle as JSON")?;
    let mut lines = vec![format!("{} {}", ModelProcessInstructions::NAME, address.to_hex())];

    if let Some(path) = &args.out {
        let body = serde_json::to_string_pretty(&ConfigRequest { kind: ModelProcessInstructions::NAME, value: &value })
            .context("render the POST /configs body")?;
        fs::write(path, body).with_context(|| format!("write {}", path.display()))?;
        lines.push(format!("wrote {}", path.display()));
    }

    if args.record {
        let stored = client.author_config(ModelProcessInstructions::NAME, &value)?;
        if stored.digest != address {
            bail!(
                "the coordinator addressed the stored bundle at {} rather than {}; the bytes it recorded are not the \
                 bytes that were assembled",
                stored.digest.to_hex(),
                address.to_hex(),
            );
        }
        lines.push(String::from("recorded as configuration on the coordinator"));
    }

    lines.push(
        "authorize it by naming that address in AETHER_BLOOMERY_AUTHORIZED_INSTRUCTIONS and restarting the \
         coordinator; seal blooms that pin it (ADR-0214)"
            .to_owned(),
    );
    Ok(lines.join("\n"))
}
