//! Operator CLI for commission authoring (ADR-0199 slice 2).
//!
//! A sibling binary of `bloomery`, not a subcommand of it. Authoring verbs
//! talk to the coordinator's authenticated control API and never open
//! `SQLite`. `import` is the offline exception: it writes an explicit
//! snapshot into a journal file while commission creation and sealing are
//! quiesced. The coordinator holds no private keys; this CLI signs on the
//! operator's host with the operator's seed, the same way `xtask bloom
//! cancel` / `reopen` / `amend` do. `approve` still accepts an already-produced
//! [`SignatureEnvelope`] when the operator minted one elsewhere.

mod client;
mod crates;
pub(crate) mod import;
pub(crate) mod scope;

use std::ffi::OsString;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use aether_bloomery::{
    CancelCommissionRequest, CommissionApprovalView, CommissionCancelledView, CommissionCreatedView,
    CommissionReopenedView, CommissionShowView, CommissionsView, CreateCommissionRequest, DEFAULT_HTTP_PORT, Digest,
    KeyId, Observation, OperatorKey, Provenance, ReopenCommissionRequest, RevisionEvidence, ScopeRevision,
    ScopeRevisionWrittenView, ScopeRunOpenedView, ScopeRunRequest, SignatureEnvelope, Statement, ViewDocument,
    WorkpieceId, WriteRevisionRequest, digest_of, signed_approval,
};
use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};

use client::ControlApi;
use scope::load_revision;

pub use scope::{parse_revision, task_text};

use crate::bloomery::load_policy;

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
    /// Submit an approval for a scope digest: either a pre-signed envelope or a
    /// seed-file signature minted through [`signed_approval`].
    Approve {
        /// Workpiece id whose commission is approved.
        id: String,
        /// Hex digest of the scope revision being approved.
        #[arg(long)]
        scope: String,
        /// Already-produced ADR-0179 signature envelope.
        #[arg(long)]
        envelope: Option<PathBuf>,
        /// Operator signing seed: 32 raw bytes or 64 hex characters, mode 0600.
        #[arg(long)]
        seed_file: Option<PathBuf>,
        /// The `KeyId` the coordinator's allowlist names for that seed.
        #[arg(long, default_value = "operator")]
        signer: String,
    },
    /// Create-or-rescope, write a scope revision, and approve it in one step.
    Author {
        /// Workpiece id the commission is.
        #[arg(long)]
        id: String,
        /// File whose bytes become the intent statement.
        #[arg(long = "intent-file")]
        intent_file: PathBuf,
        /// Managed-heading markdown for the scope revision.
        #[arg(long = "scope-file")]
        scope_file: PathBuf,
        /// Operator signing seed: 32 raw bytes or 64 hex characters, mode 0600.
        #[arg(long)]
        seed_file: Option<PathBuf>,
        /// The `KeyId` the coordinator's allowlist names for that seed.
        #[arg(long, default_value = "operator")]
        signer: String,
        /// Append `id=digest` to this file after a successful write.
        #[arg(long)]
        ledger: Option<PathBuf>,
        /// Approval policy the declared-surface granularity lint reads.
        #[arg(long = "approval-policy", default_value = "approval-policy.toml")]
        approval_policy: PathBuf,
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
        Command::Approve { id, scope, envelope, seed_file, signer } => {
            approve(&api, &id, &scope, envelope.as_deref(), seed_file.as_deref(), &signer)
        }
        Command::Author { id, intent_file, scope_file, seed_file, signer, ledger, approval_policy } => {
            create_or_rescope(&api, &id, &intent_file)?;
            let digest = write_verified_scope(&api, &id, &scope_file, &approval_policy)?;
            if let Some(seed_file) = seed_file.as_deref() {
                approve_with_seed(&api, &id, digest, seed_file, &signer)?;
            }
            record_authored(&id, digest, ledger.as_deref())
        }
        Command::Show { id, json } => show(&api, &id, json),
        Command::List { status } => list(&api, status.as_deref()),
        Command::Cancel { id, reason, envelope } => cancel(&api, &id, &reason, &envelope),
        Command::Reopen { id, reason, envelope } => reopen(&api, &id, &reason, &envelope),
        Command::Import { manifest, store_path, sealed } => {
            import::import_paths(&manifest, &store_path, sealed.as_deref())
        }
    }
}

fn open_scope_run(api: &ControlApi, id: &str) -> Result<String> {
    let view: ViewDocument = api.get_json("/view")?;
    let opened: ScopeRunOpenedView =
        api.send_json("POST", &format!("/commissions/{id}/scope-runs"), &ScopeRunRequest { base: view.mainline })?;
    Ok(format!("{} ordinal {} sequence {} subject {}\n", opened.id, opened.ordinal, opened.sequence, opened.subject))
}

fn create(api: &ControlApi, id: &str, intent_file: &Path) -> Result<String> {
    let intent = load_intent(intent_file)?;
    let created: CommissionCreatedView =
        api.send_json("POST", "/commissions", &CreateCommissionRequest { id: WorkpieceId(id.to_owned()), intent })?;
    Ok(format!("created {} intent {}\n", created.id, created.intent))
}

fn create_or_rescope(api: &ControlApi, id: &str, intent_file: &Path) -> Result<()> {
    match create(api, id, intent_file) {
        Ok(_) => Ok(()),
        Err(error) if is_duplicate_commission(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

fn is_duplicate_commission(error: &anyhow::Error) -> bool {
    let message = error.to_string();
    message.contains("409:") && message.contains("already exists")
}

fn write_scope(api: &ControlApi, id: &str, file: &Path, approval_policy: &Path) -> Result<String> {
    let written = post_revision(api, id, file, approval_policy)?;
    Ok(format!("{}\n", written.digest))
}

fn write_verified_scope(api: &ControlApi, id: &str, file: &Path, approval_policy: &Path) -> Result<Digest> {
    let revision = prepared_revision(api, id, file, approval_policy)?;
    let expected = digest_of(&revision);
    let written = post_revision_value(api, id, revision)?;
    refuse_unread_digest(written.digest, expected)
}

fn post_revision(api: &ControlApi, id: &str, file: &Path, approval_policy: &Path) -> Result<ScopeRevisionWrittenView> {
    post_revision_value(api, id, prepared_revision(api, id, file, approval_policy)?)
}

fn post_revision_value(api: &ControlApi, id: &str, revision: ScopeRevision) -> Result<ScopeRevisionWrittenView> {
    api.send_json(
        "POST",
        &format!("/commissions/{id}/revisions"),
        &WriteRevisionRequest { revision, evidence: RevisionEvidence::default() },
    )
}

fn prepared_revision(api: &ControlApi, id: &str, file: &Path, approval_policy: &Path) -> Result<ScopeRevision> {
    let predecessor = current_revision(api, id)?;
    let revision = load_revision(id, file, predecessor)?;
    lint_surface_granularity(&revision, approval_policy)?;
    Ok(revision)
}

/// Refuse a coordinator-stored digest that is not the address of the revision
/// just written, so an approval is never signed over unread bytes.
fn refuse_unread_digest(stored: Digest, expected: Digest) -> Result<Digest> {
    if stored != expected {
        bail!("the coordinator stored {stored} for a revision addressed {expected}");
    }
    Ok(stored)
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

fn approve(
    api: &ControlApi,
    id: &str,
    scope: &str,
    envelope: Option<&Path>,
    seed_file: Option<&Path>,
    signer: &str,
) -> Result<String> {
    let digest = digest_from_hex(scope)?;
    let statement = approval_statement(digest, envelope, seed_file, signer)?;
    let written: CommissionApprovalView = api.send_json("POST", &format!("/commissions/{id}/approvals"), &statement)?;
    Ok(format!("{}\n", written.digest))
}

fn approve_with_seed(api: &ControlApi, id: &str, digest: Digest, seed_file: &Path, signer: &str) -> Result<()> {
    let statement = approval_statement(digest, None, Some(seed_file), signer)?;
    let _: CommissionApprovalView = api.send_json("POST", &format!("/commissions/{id}/approvals"), &statement)?;
    Ok(())
}

fn approval_statement(
    digest: Digest,
    envelope: Option<&Path>,
    seed_file: Option<&Path>,
    signer: &str,
) -> Result<Statement> {
    match (envelope, seed_file) {
        (Some(_), Some(_)) => bail!("approve takes --envelope or --seed-file, not both"),
        (None, None) => bail!("approve requires --envelope or --seed-file"),
        (Some(path), None) => signed_statement(path, digest.as_bytes()),
        (None, Some(path)) => {
            let key = OperatorKey::load(KeyId(signer.to_owned()), path)?;
            Ok(signed_approval(key.signer.clone(), key.seed(), digest))
        }
    }
}

fn record_authored(id: &str, digest: Digest, ledger: Option<&Path>) -> Result<String> {
    let line = format!("{id}={}", digest.to_hex());
    if let Some(path) = ledger {
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut file| writeln!(file, "{line}"))
            .with_context(|| format!("append ledger {}", path.display()))?;
    }
    Ok(format!("{line}\n"))
}

fn show(api: &ControlApi, id: &str, json: bool) -> Result<String> {
    if json {
        let body: serde_json::Value = api.get_json(&format!("/commissions/{id}"))?;
        Ok(format!("{}\n", serde_json::to_string_pretty(&body).context("pretty-print commission JSON")?))
    } else {
        let view: CommissionShowView = api.get_json(&format!("/commissions/{id}"))?;
        let revision = view.current_revision.map_or_else(|| "-".to_owned(), |digest| digest.to_string());
        Ok(match view.current_unreadable {
            Some(reason) => {
                format!("{} {} intent {} revision {revision} unreadable: {reason}\n", view.id, view.status, view.intent)
            }
            None => format!("{} {} intent {} revision {revision}\n", view.id, view.status, view.intent),
        })
    }
}

fn list(api: &ControlApi, status: Option<&str>) -> Result<String> {
    let path = status.map_or_else(|| "/commissions".to_owned(), |status| format!("/commissions?status={status}"));
    let view: CommissionsView = api.get_json(&path)?;
    let mut out = String::new();
    for head in view.commissions {
        out.push_str(&head.id.0);
        out.push(' ');
        out.push_str(&head.status);
        out.push('\n');
    }
    Ok(out)
}

fn cancel(api: &ControlApi, id: &str, reason: &str, envelope: &Path) -> Result<String> {
    let view: CommissionShowView = api.get_json(&format!("/commissions/{id}"))?;
    let statement = signed_statement(envelope, view.intent.as_bytes())?;
    let cancelled: CommissionCancelledView = api.send_json(
        "POST",
        &format!("/commissions/{id}/cancel"),
        &CancelCommissionRequest { statement, reason: reason.to_owned() },
    )?;
    Ok(format!("{} {} ({reason})\n", cancelled.id, cancelled.status))
}

/// Put a landed commission whose member never resolved back in the line.
///
/// The envelope is a Reopen-door signature over the commission's intent digest,
/// which is why the intent is read first: the operator signs the commission
/// they are looking at. The coordinator refuses a workpiece some bloom actually
/// resolved, so a wrong id is answered rather than acted on.
fn reopen(api: &ControlApi, id: &str, reason: &str, envelope: &Path) -> Result<String> {
    let view: CommissionShowView = api.get_json(&format!("/commissions/{id}"))?;
    let statement = signed_statement(envelope, view.intent.as_bytes())?;
    let reopened: CommissionReopenedView = api.send_json(
        "POST",
        &format!("/commissions/{id}/reopen"),
        &ReopenCommissionRequest { statement, reason: reason.to_owned() },
    )?;
    Ok(format!("{} {} ({reason})\n", reopened.id, reopened.status))
}

fn current_revision(api: &ControlApi, id: &str) -> Result<Option<Digest>> {
    match api.get_json_or_not_found::<CommissionShowView>(&format!("/commissions/{id}"))? {
        Some(view) => Ok(view.current_revision),
        None => bail!("no commission named {id}"),
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

    use super::{Command, CommissionCli, load_intent, refuse_unread_digest, signed_statement};
    use aether_bloomery::Digest;
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

    #[test]
    fn author_is_a_verb_on_the_sibling_binary() {
        let cli = CommissionCli::try_parse_from([
            "bloomery-commission",
            "author",
            "--id",
            "issue-1",
            "--intent-file",
            "intent.txt",
            "--scope-file",
            "scope.md",
            "--seed-file",
            "seed",
        ])
        .unwrap_or_else(|error| panic!("author must parse: {error}"));
        match cli.command {
            Command::Author { id, signer, .. } => {
                assert_eq!(id, "issue-1");
                assert_eq!(signer, "operator");
            }
            other => panic!("expected author, got {other:?}"),
        }
    }

    #[test]
    fn approve_refuses_when_neither_envelope_nor_seed_is_given() {
        match super::run(["bloomery-commission", "approve", "issue-1", "--scope", &"aa".repeat(32)]) {
            Ok(output) => panic!("neither source must be refused, got {output}"),
            Err(error) => {
                let message = error.to_string();
                assert!(
                    message.contains("--envelope") && message.contains("--seed-file"),
                    "refusal names both sources, got {message}"
                );
            }
        }
    }

    #[test]
    fn approve_refuses_when_both_envelope_and_seed_are_given() {
        match super::run([
            "bloomery-commission",
            "approve",
            "issue-1",
            "--scope",
            &"aa".repeat(32),
            "--envelope",
            "envelope.json",
            "--seed-file",
            "seed",
        ]) {
            Ok(output) => panic!("both sources must be refused, got {output}"),
            Err(error) => {
                let message = error.to_string();
                assert!(message.contains("not both"), "refusal names the conflict, got {message}");
            }
        }
    }

    #[test]
    fn a_stored_digest_that_is_not_the_revision_is_refused() {
        let expected = Digest::from_bytes([1; 32]);
        let stored = Digest::from_bytes([2; 32]);
        match refuse_unread_digest(stored, expected) {
            Ok(_) => panic!("a mismatched stored digest must be refused"),
            Err(error) => {
                let message = error.to_string();
                assert!(message.contains(&stored.to_hex()), "refusal names the stored digest: {message}");
                assert!(message.contains(&expected.to_hex()), "refusal names the local address: {message}");
            }
        }
    }
}
