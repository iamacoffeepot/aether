//! Operator CLI for commission authoring (ADR-0199 slice 2).
//!
//! A sibling binary of `bloomery`, not a subcommand of it. Authoring verbs
//! talk to the coordinator's authenticated control API and never open
//! `SQLite`. `import` is the offline exception: it writes an explicit
//! snapshot into a journal file while commission creation and sealing are
//! quiesced. `approve` and `cancel` submit an already-produced
//! [`SignatureEnvelope`]; this crate does not hold private keys.

mod client;
mod crates;
pub(crate) mod import;
pub(crate) mod scope;

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use aether_bloomery::{Digest, Observation, Provenance, ScopeRevision, SignatureEnvelope, Statement};
use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

use client::ControlApi;
use scope::load_revision;

use crate::bloomery::load_policy;

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
    /// Open a pre-bloom scoping run on the named commission.
    ScopeRun {
        /// Workpiece id whose commission is scoped.
        id: String,
    },
    /// Parse a managed-heading scope file and write the canonical revision.
    Scope {
        /// Workpiece id whose commission receives the revision.
        id: String,
        /// Managed-heading markdown.
        #[arg(long)]
        file: PathBuf,
        /// Approval policy the declared-surface granularity lint reads. The
        /// seal door is the backstop; this stops an operator before they sign
        /// an approval bound to a digest that can never seal.
        #[arg(long = "approval-policy", default_value = "approval-policy.toml")]
        approval_policy: PathBuf,
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
    /// Submit a pre-signed reopen envelope and put a stranded commission back
    /// in the line.
    Reopen {
        /// Workpiece id to restore.
        id: String,
        /// Operator-facing reason printed after a successful reopen.
        #[arg(long)]
        reason: String,
        /// Already-produced ADR-0179 signature envelope.
        #[arg(long)]
        envelope: PathBuf,
    },
    /// Import an explicit snapshot of planned issues into a local journal.
    Import {
        /// JSON listing the named issues and their body files. Never a directory sweep.
        #[arg(long)]
        manifest: PathBuf,
        /// `SQLite` journal to write. This verb opens the file; the coordinator stays quiesced.
        #[arg(long)]
        store_path: PathBuf,
        /// Optional sealed-bloom reconstructions whose rows must match the pinned digests.
        #[arg(long)]
        sealed: Option<PathBuf>,
    },
}

/// Parse `args` (including argv0) and run the named verb. Returns the text
/// written to stdout on success.
pub fn run<I, T>(args: I) -> Result<String>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    dispatch(CommissionCli::try_parse_from(args).map_err(|error| anyhow!("{error}"))?)
}

/// Parse process argv and run. `--help` exits through clap before this returns.
pub fn main_output() -> Result<String> {
    dispatch(CommissionCli::parse())
}

fn dispatch(cli: CommissionCli) -> Result<String> {
    let api = ControlApi { port: cli.http_port, token: cli.token };
    match cli.command {
        Command::Create { id, intent_file } => create(&api, &id, &intent_file),
        Command::ScopeRun { id } => open_scope_run(&api, &id),
        Command::Scope { id, file, approval_policy } => write_scope(&api, &id, &file, &approval_policy),
        Command::Approve { id, scope, envelope } => approve(&api, &id, &scope, &envelope),
        Command::Show { id, json } => show(&api, &id, json),
        Command::List { status } => list(&api, status.as_deref()),
        Command::Cancel { id, reason, envelope } => cancel(&api, &id, &reason, &envelope),
        Command::Reopen { id, reason, envelope } => reopen(&api, &id, &reason, &envelope),
        Command::Import { manifest, store_path, sealed } => {
            import::import_paths(&manifest, &store_path, sealed.as_deref())
        }
    }
}

#[derive(Serialize)]
struct CreateBody<'a> {
    id: &'a str,
    intent: &'a Statement,
}

#[derive(Serialize)]
struct WriteRevisionBody<'a> {
    revision: &'a ScopeRevision,
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

#[derive(Serialize)]
struct ReopenBody {
    statement: Statement,
    reason: String,
}

#[derive(Deserialize)]
struct ViewMainline {
    mainline: String,
}

#[derive(Serialize)]
struct ScopeRunBody {
    base: Digest,
}

#[derive(Deserialize)]
struct ScopeRunOpened {
    id: String,
    ordinal: u64,
    sequence: u64,
    subject: String,
}

fn open_scope_run(api: &ControlApi, id: &str) -> Result<String> {
    let view: ViewMainline = api.get_json("/view")?;
    let base = digest_from_hex(&view.mainline)?;
    let opened: ScopeRunOpened =
        api.send_json("POST", &format!("/commissions/{id}/scope-runs"), &ScopeRunBody { base })?;
    Ok(format!("{} ordinal {} sequence {} subject {}\n", opened.id, opened.ordinal, opened.sequence, opened.subject))
}

fn create(api: &ControlApi, id: &str, intent_file: &Path) -> Result<String> {
    let intent = load_intent(intent_file)?;
    let created: CreatedView = api.send_json("POST", "/commissions", &CreateBody { id, intent: &intent })?;
    Ok(format!("created {} intent {}\n", created.id, created.intent))
}

fn write_scope(api: &ControlApi, id: &str, file: &Path, approval_policy: &Path) -> Result<String> {
    let predecessor = match current_revision(api, id)? {
        Some(hex) => Some(digest_from_hex(&hex)?),
        None => None,
    };
    let revision = load_revision(id, file, predecessor)?;
    lint_surface_granularity(&revision, approval_policy)?;
    let written: DigestView =
        api.send_json("POST", &format!("/commissions/{id}/revisions"), &WriteRevisionBody { revision: &revision })?;
    Ok(format!("{}\n", written.digest))
}

/// Refuse a declared surface that names a file the approval policy does not.
///
/// The seal door is the authority — it reads the *stored* revision, so a
/// declaration that cannot seal is one an operator would otherwise discover
/// only after signing an approval bound to its digest. This is where they are
/// stopped instead.
///
/// A policy that cannot be read or parsed warns and skips: this is an authoring
/// convenience with a backstop, and refusing every scope write on a missing
/// file would make the lint the thing that blocks work.
fn lint_surface_granularity(revision: &ScopeRevision, approval_policy: &Path) -> Result<()> {
    let policy = match load_policy(approval_policy) {
        Ok(policy) => policy,
        Err(error) => {
            #[allow(
                clippy::print_stderr,
                reason = "an advisory the operator reads; the command's own output is the String it returns on stdout"
            )]
            {
                eprintln!(
                    "warning: approval policy {} could not be read ({error}); skipping the declared-surface \
                     granularity lint. The seal door still enforces it.",
                    approval_policy.display()
                );
            }
            return Ok(());
        }
    };
    if let Some(glob) = policy.unnamed_file_entries(&revision.declared_surface).first() {
        bail!(
            "declared surface {glob:?} names one file and no approval-policy rule names that file; \
             widen it to a crate glob such as crates/<crate>/src/**"
        );
    }
    Ok(())
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
    let path = status.map_or_else(|| "/commissions".to_owned(), |status| format!("/commissions?status={status}"));
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

/// Put a landed commission whose member never resolved back in the line.
///
/// The envelope is a Reopen-door signature over the commission's intent digest,
/// which is why the intent is read first: the operator signs the commission
/// they are looking at. The coordinator refuses a workpiece some bloom actually
/// resolved, so a wrong id is answered rather than acted on.
fn reopen(api: &ControlApi, id: &str, reason: &str, envelope: &Path) -> Result<String> {
    let view: ShowView = api.get_json(&format!("/commissions/{id}"))?;
    let intent = digest_from_hex(&view.intent)?;
    let statement = signed_statement(envelope, intent.as_bytes())?;
    let body = ReopenBody { statement, reason: reason.to_owned() };
    let reopened: CancelledView = api.send_json("POST", &format!("/commissions/{id}/reopen"), &body)?;
    Ok(format!("{} {} ({reason})\n", reopened.id, reopened.status))
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
    let bytes = fs::read(path).map_err(|error| anyhow!("read {}: {error}", path.display()))?;
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
    let bytes = fs::read(path).map_err(|error| anyhow!("read {}: {error}", path.display()))?;
    let envelope: SignatureEnvelope = serde_json::from_slice(&bytes)
        .with_context(|| format!("envelope {} is not an ADR-0179 SignatureEnvelope", path.display()))?;
    Ok(Statement { words: words.to_vec(), provenance: Provenance::AuthorSignature(envelope), parents: Vec::new() })
}

fn digest_from_hex(hex: &str) -> Result<Digest> {
    Digest::from_hex(hex).ok_or_else(|| anyhow!("expected a 64-character hex digest, got {hex:?}"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{Command, CommissionCli, load_intent, signed_statement};
    use clap::Parser;

    #[test]
    fn the_cli_is_a_subcommand_family_on_its_own_binary() {
        let cli =
            CommissionCli::try_parse_from(["bloomery-commission", "list", "--http-port", "8910", "--token", "secret"])
                .unwrap_or_else(|error| panic!("list must parse: {error}"));
        assert_eq!(cli.http_port, 8910);
        assert_eq!(cli.token, "secret");
    }

    #[test]
    fn scope_run_is_a_verb_on_the_sibling_binary() {
        let cli = CommissionCli::try_parse_from(["bloomery-commission", "scope-run", "issue-1"])
            .unwrap_or_else(|error| panic!("scope-run must parse: {error}"));
        match cli.command {
            Command::ScopeRun { id } => assert_eq!(id, "issue-1"),
            other => panic!("expected scope-run, got {other:?}"),
        }
    }

    #[test]
    fn reopen_is_a_verb_on_the_sibling_binary() {
        let cli = CommissionCli::try_parse_from([
            "bloomery-commission",
            "reopen",
            "issue-1",
            "--reason",
            "withdrawn from a landed bloom",
            "--envelope",
            "envelope.json",
        ])
        .unwrap_or_else(|error| panic!("reopen must parse: {error}"));
        match cli.command {
            Command::Reopen { id, reason, .. } => {
                assert_eq!(id, "issue-1");
                assert_eq!(reason, "withdrawn from a landed bloom");
            }
            other => panic!("expected reopen, got {other:?}"),
        }
    }

    #[test]
    fn import_is_a_verb_on_the_sibling_binary() {
        let cli = CommissionCli::try_parse_from([
            "bloomery-commission",
            "import",
            "--manifest",
            "manifest.json",
            "--store-path",
            "journal.sqlite",
        ])
        .unwrap_or_else(|error| panic!("import must parse: {error}"));
        match cli.command {
            Command::Import { .. } => {}
            other => panic!("expected import, got {other:?}"),
        }
    }

    #[test]
    fn a_malformed_envelope_file_is_refused_before_http() {
        let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("temp dir: {error}"));
        let path = dir.path().join("envelope.json");
        fs::write(&path, "{not-an-envelope").unwrap_or_else(|error| panic!("write envelope fixture: {error}"));
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
        let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("temp dir: {error}"));
        let path = dir.path().join("intent.txt");
        fs::write(&path, "ship the CLI").unwrap_or_else(|error| panic!("write intent fixture: {error}"));
        let statement = load_intent(&path).unwrap_or_else(|error| panic!("text intent must load: {error}"));
        assert_eq!(statement.words, b"ship the CLI");
    }
}
