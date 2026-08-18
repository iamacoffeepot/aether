//! Operator CLI for commission authoring (ADR-0199 slice 2).
//!
//! A sibling binary of `bloomery`, not a subcommand of it. Every verb talks
//! to the coordinator's authenticated control API and never opens `SQLite`.
//! `approve` and `cancel` submit an already-produced [`SignatureEnvelope`];
//! this crate does not hold private keys.

mod client;
mod hex;
mod scope;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use aether_bloomery::{Digest, Observation, Provenance, SignatureEnvelope, Statement};
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

use client::ControlApi;
use scope::load_revision;

/// Default REST port when `--http-port` is omitted — the same default the
/// daemon binds when `AETHER_HTTP_PORT` is unset.
const DEFAULT_HTTP_PORT: u16 = 8910;

/// The `bloomery-commission` clap root.
#[derive(Parser, Debug)]
#[command(
    name = "bloomery-commission",
    about = "Author and query Bloomery commissions through the coordinator control API."
)]
struct CommissionCli {
    /// REST control-API port the coordinator bound.
    #[arg(long, default_value_t = DEFAULT_HTTP_PORT, global = true)]
    http_port: u16,

    /// Bearer token matching the coordinator's `AETHER_HTTP_CONTROL_TOKEN`.
    #[arg(long, default_value = "", global = true)]
    token: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Persist a new open commission from an intent file.
    Create {
        /// Workpiece id the commission is.
        #[arg(long)]
        id: String,
        /// File whose bytes become the intent statement.
        #[arg(long = "intent-file")]
        intent_file: PathBuf,
    },
    /// Parse a managed-heading scope file and write the canonical revision.
    Scope {
        /// Workpiece id whose commission receives the revision.
        id: String,
        /// Managed-heading markdown.
        #[arg(long)]
        file: PathBuf,
    },
    /// Submit a pre-signed approval envelope for a scope digest.
    Approve {
        /// Workpiece id whose commission is approved.
        id: String,
        /// Hex digest of the scope revision being approved.
        #[arg(long)]
        scope: String,
        /// Already-produced ADR-0179 signature envelope.
        #[arg(long)]
        envelope: PathBuf,
    },
    /// Show one commission.
    Show {
        /// Workpiece id to load.
        id: String,
        /// Print the control-API JSON body.
        #[arg(long)]
        json: bool,
    },
    /// List commissions, optionally filtered by status.
    List {
        /// Lifecycle filter (`open`, `cancelled`, `landed`).
        #[arg(long)]
        status: Option<String>,
    },
    /// Submit a pre-signed cancel envelope and close the commission.
    Cancel {
        /// Workpiece id to close.
        id: String,
        /// Operator-facing reason printed after a successful cancel.
        #[arg(long)]
        reason: String,
        /// Already-produced ADR-0179 signature envelope.
        #[arg(long)]
        envelope: PathBuf,
    },
}

/// Parse `args` (including argv0) and run the named verb. Returns the text
/// written to stdout on success.
pub fn run<I, T>(args: I) -> Result<String>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    dispatch(CommissionCli::try_parse_from(args).map_err(|error| anyhow::anyhow!("{error}"))?)
}

/// Parse process argv and run. `--help` exits through clap before this returns.
pub fn main_output() -> Result<String> {
    dispatch(CommissionCli::parse())
}

fn dispatch(cli: CommissionCli) -> Result<String> {
    let api = ControlApi { port: cli.http_port, token: cli.token };
    match cli.command {
        Command::Create { id, intent_file } => create(&api, &id, &intent_file),
        Command::Scope { id, file } => write_scope(&api, &id, &file),
        Command::Approve { id, scope, envelope } => approve(&api, &id, &scope, &envelope),
        Command::Show { id, json } => show(&api, &id, json),
        Command::List { status } => list(&api, status.as_deref()),
        Command::Cancel { id, reason, envelope } => cancel(&api, &id, &reason, &envelope),
    }
}

#[derive(Serialize)]
struct CreateBody<'a> {
    id: &'a str,
    intent: &'a Statement,
}

#[derive(Deserialize)]
struct CreatedView {
    id: String,
    intent: String,
}

#[derive(Deserialize)]
struct DigestView {
    digest: String,
}

#[derive(Deserialize)]
struct ShowView {
    id: String,
    intent: String,
    status: String,
    current_revision: Option<String>,
}

#[derive(Deserialize)]
struct ListView {
    commissions: Vec<ShowView>,
}

#[derive(Deserialize)]
struct CancelledView {
    id: String,
    status: String,
}

fn create(api: &ControlApi, id: &str, intent_file: &Path) -> Result<String> {
    let intent = load_intent(intent_file)?;
    let created: CreatedView = api.send_json("POST", "/commissions", &CreateBody { id, intent: &intent })?;
    Ok(format!("created {} intent {}\n", created.id, created.intent))
}

fn write_scope(api: &ControlApi, id: &str, file: &Path) -> Result<String> {
    let predecessor = match current_revision(api, id)? {
        Some(hex) => Some(digest_from_hex(&hex)?),
        None => None,
    };
    let revision = load_revision(id, file, predecessor)?;
    let written: DigestView = api.send_json("POST", &format!("/commissions/{id}/revisions"), &revision)?;
    Ok(format!("{}\n", written.digest))
}

fn approve(api: &ControlApi, id: &str, scope: &str, envelope: &Path) -> Result<String> {
    let digest = digest_from_hex(scope)?;
    let statement = signed_statement(envelope, digest.as_bytes())?;
    let written: DigestView = api.send_json("POST", &format!("/commissions/{id}/approvals"), &statement)?;
    Ok(format!("{}\n", written.digest))
}

fn show(api: &ControlApi, id: &str, json: bool) -> Result<String> {
    if json {
        let body: serde_json::Value = api.get_json(&format!("/commissions/{id}"))?;
        Ok(format!("{}\n", serde_json::to_string_pretty(&body).context("pretty-print commission JSON")?))
    } else {
        let view: ShowView = api.get_json(&format!("/commissions/{id}"))?;
        let revision = view.current_revision.as_deref().unwrap_or("-");
        Ok(format!("{} {} intent {} revision {}\n", view.id, view.status, view.intent, revision))
    }
}

fn list(api: &ControlApi, status: Option<&str>) -> Result<String> {
    let path = match status {
        Some(status) => format!("/commissions?status={status}"),
        None => "/commissions".to_owned(),
    };
    let view: ListView = api.get_json(&path)?;
    let mut out = String::new();
    for head in view.commissions {
        out.push_str(&head.id);
        out.push(' ');
        out.push_str(&head.status);
        out.push('\n');
    }
    Ok(out)
}

fn cancel(api: &ControlApi, id: &str, reason: &str, envelope: &Path) -> Result<String> {
    let view: ShowView = api.get_json(&format!("/commissions/{id}"))?;
    let intent = digest_from_hex(&view.intent)?;
    let statement = signed_statement(envelope, intent.as_bytes())?;
    let cancelled: CancelledView = api.send_json("POST", &format!("/commissions/{id}/cancel"), &statement)?;
    Ok(format!("{} {} ({reason})\n", cancelled.id, cancelled.status))
}

fn current_revision(api: &ControlApi, id: &str) -> Result<Option<String>> {
    match api.get_json::<ShowView>(&format!("/commissions/{id}")) {
        Ok(view) => Ok(view.current_revision),
        Err(error) if error.to_string().contains("404:") => {
            bail!("no commission named {id}")
        }
        Err(error) => Err(error),
    }
}

fn load_intent(path: &Path) -> Result<Statement> {
    let bytes = std::fs::read(path).map_err(|error| anyhow::anyhow!("read {}: {error}", path.display()))?;
    if bytes.first() == Some(&b'{') {
        return serde_json::from_slice(&bytes).with_context(|| format!("parse intent statement {}", path.display()));
    }
    Ok(Statement {
        words: bytes,
        provenance: Provenance::ObservationAttestation(Observation { source: path.display().to_string() }),
        parents: Vec::new(),
    })
}

fn signed_statement(path: &Path, words: &[u8]) -> Result<Statement> {
    let bytes = std::fs::read(path).map_err(|error| anyhow::anyhow!("read {}: {error}", path.display()))?;
    let envelope: SignatureEnvelope = serde_json::from_slice(&bytes)
        .with_context(|| format!("envelope {} is not an ADR-0179 SignatureEnvelope", path.display()))?;
    Ok(Statement { words: words.to_vec(), provenance: Provenance::AuthorSignature(envelope), parents: Vec::new() })
}

fn digest_from_hex(hex: &str) -> Result<Digest> {
    match hex::decode_digest(hex) {
        Some(bytes) => Ok(Digest::from_bytes(bytes)),
        None => bail!("expected a 64-character hex digest, got {hex:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{CommissionCli, load_intent, signed_statement};
    use clap::Parser;

    #[test]
    fn the_cli_is_a_subcommand_family_on_its_own_binary() {
        let cli = match CommissionCli::try_parse_from([
            "bloomery-commission",
            "list",
            "--http-port",
            "8910",
            "--token",
            "secret",
        ]) {
            Ok(cli) => cli,
            Err(error) => panic!("list must parse: {error}"),
        };
        assert_eq!(cli.http_port, 8910);
        assert_eq!(cli.token, "secret");
    }

    #[test]
    fn a_malformed_envelope_file_is_refused_before_http() {
        let dir = match tempfile::tempdir() {
            Ok(dir) => dir,
            Err(error) => panic!("temp dir: {error}"),
        };
        let path = dir.path().join("envelope.json");
        if let Err(error) = std::fs::write(&path, "{not-an-envelope") {
            panic!("write envelope fixture: {error}");
        }
        match signed_statement(&path, &[0; 32]) {
            Ok(_) => panic!("malformed envelope must not parse"),
            Err(error) => {
                let message = error.to_string();
                assert!(message.contains("SignatureEnvelope"), "operator-readable refusal, got {message}");
            }
        }
    }

    #[test]
    fn a_text_intent_file_preserves_its_bytes() {
        let dir = match tempfile::tempdir() {
            Ok(dir) => dir,
            Err(error) => panic!("temp dir: {error}"),
        };
        let path = dir.path().join("intent.txt");
        if let Err(error) = std::fs::write(&path, "ship the CLI") {
            panic!("write intent fixture: {error}");
        }
        let statement = match load_intent(&path) {
            Ok(statement) => statement,
            Err(error) => panic!("text intent must load: {error}"),
        };
        assert_eq!(statement.words, b"ship the CLI");
    }
}
