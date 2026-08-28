//! `cargo xtask bloom` — operator client over the coordinator REST surface.
//!
//! Seals and supersedes compose typed bodies (drafts, configs, edges) instead
//! of hand-rolled JSON. `seal` reads each member's commission before opening a
//! draft so the membership pins the stored revision. `supersede` defaults the
//! successor onto the current observed head, reuses the predecessor's sealed
//! configs by digest, and carries each member's scope revision so the
//! workpiece claim transfers.

mod amend;
mod archive;
mod client;
mod dto;
mod hex;
mod http;
mod plan;
mod profiles;
mod roll;
mod status;
mod upgrade;

use std::env;
use std::path::{Path, PathBuf};

use aether_bloomery::{BackendObjectId, DEFAULT_HTTP_PORT, Digest, KeyId, OperatorProposal, Outcome, digest_of};
use aether_bloomery_git::command::run_ok;
use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};

use crate::bloom::amend::{AmendArgs, OperatorKey};
use crate::bloom::client::{Client, bloom_in};
use crate::bloom::plan::BaseChoice;
use crate::bloom::roll::RollArgs;
use crate::bloom::upgrade::UpgradeArgs;

/// The repository's approval policy — the fallback the coordinator itself
/// loads when a bloom seals none of its own.
pub const DEFAULT_POLICY_FILE: &str = "approval-policy.toml";

/// One coordinator the command talks to.
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    /// Bearer token for the commission routes, when one is configured. `None`
    /// sends no `Authorization` header at all, which is what every route
    /// outside `/commissions` expects.
    pub token: Option<String>,
}

impl Endpoint {
    fn resolve(port: Option<u16>, token: Option<&str>) -> Self {
        Self {
            host: "127.0.0.1".to_owned(),
            port: port.unwrap_or_else(coordinator_port),
            token: token.map(ToOwned::to_owned).or_else(control_token),
        }
    }
}

/// `AETHER_HTTP_CONTROL_TOKEN`, or nothing.
fn control_token() -> Option<String> {
    // Operator tooling reading the coordinator's control-route bearer token —
    // not cap config.
    #[allow(clippy::disallowed_methods)]
    env::var("AETHER_HTTP_CONTROL_TOKEN").ok().filter(|token| !token.is_empty())
}

/// `AETHER_HTTP_PORT`, then the coordinator's compiled default.
fn coordinator_port() -> u16 {
    // Operator tooling reading the coordinator's REST bind — not cap config.
    #[allow(clippy::disallowed_methods)]
    env::var("AETHER_HTTP_PORT").ok().and_then(|value| value.parse().ok()).unwrap_or(DEFAULT_HTTP_PORT)
}

/// Drive the Bloomery coordinator REST surface.
#[derive(Args, Debug)]
pub struct BloomArgs {
    /// Coordinator REST port. Defaults to `AETHER_HTTP_PORT`, then 8910.
    #[arg(long, global = true)]
    port: Option<u16>,

    /// Bearer token for the control routes. Defaults to
    /// `AETHER_HTTP_CONTROL_TOKEN`.
    #[arg(long, global = true)]
    token: Option<String>,

    /// Approval policy the declared-surface granularity check reads before a
    /// seal is sent. The coordinator's seal door is the authority; this
    /// refuses the same shape earlier and in the same words.
    #[arg(long = "approval-policy", global = true, default_value = DEFAULT_POLICY_FILE)]
    approval_policy: PathBuf,

    #[command(subcommand)]
    command: BloomCommand,
}

#[derive(Subcommand, Debug)]
enum BloomCommand {
    /// List live blooms, statuses, and supersession links.
    Status,
    /// Shape and seal a new bloom.
    Seal(SealArgs),
    /// Seal a successor on the current observed head and transfer the claim.
    Supersede(SupersedeArgs),
    /// Drive the ADR-0186 day roll: quiesce, advance fleet main under the
    /// coverage-map barrier, cut tomorrow from that main, and hand over the
    /// repoint.
    Roll(RollArgs),
    /// Fold-test a candidate coordinator and replace the running binary if it holds.
    Upgrade(UpgradeArgs),
    /// Answer a parked member's surface request: widen its scope revision,
    /// approve the widening, and seal the successor (ADR-0207).
    Amend(AmendArgs),
    /// Move aged evidence directories and resolved session trees onto the
    /// archive tier. Refuses unless the coordinator is between blooms.
    /// Nothing is ever deleted.
    Archive(archive::ArchiveArgs),
    /// Take one member out of a walking bloom without superseding it (#5327).
    Withdraw(WithdrawArgs),
    /// Run one member's current stage again on the candidate it already holds.
    Retry(RetryArgs),
    /// Run `verify.base` again on a red receipt whose failure does not describe
    /// the tree.
    Reverify(ReverifyBaseArgs),
    /// Propose a signed change onto the day's branch (ADR-0205). The
    /// coordinator writes it when the board is clear.
    Propose(ProposeArgs),
    /// Hand a wedged member a candidate you produced yourself and let the
    /// ordinary gates judge it (#4957).
    Repair(RepairArgs),
    /// Answer the suppression requests a member's candidate is carrying
    /// (ADR-0193).
    Suppression(SuppressionArgs),
    /// Retire an open commission whose work landed outside its bloom.
    Cancel(CancelArgs),
    /// Put a landed commission whose member never resolved back in the line.
    Reopen(ReopenArgs),
}

#[derive(Args, Debug)]
struct WithdrawArgs {
    /// The bloom the member belongs to (64 hex characters).
    #[arg(value_parser = plan::parse_bloom_id)]
    bloom_id: String,

    /// The member to withdraw.
    workpiece: String,

    /// Why, in your own words. Required; a blank one is refused at the door.
    #[arg(long)]
    reason: String,

    /// Who is deciding. Recorded as the decider.
    #[arg(long, default_value = "operator")]
    operator: String,

    /// Also withdraw every member that depends on this one. Without it, a
    /// withdrawal that would strand a dependent is refused, naming them.
    #[arg(long)]
    cascade: bool,
}

#[derive(Args, Debug)]
struct RetryArgs {
    /// The bloom the member belongs to (64 hex characters).
    #[arg(value_parser = plan::parse_bloom_id)]
    bloom_id: String,

    /// The member to re-dispatch.
    workpiece: String,

    /// The stage to run again. Defaults to wherever the member is sitting;
    /// naming a different one is refused rather than applied, so a retry aimed
    /// from a stale read of the board does not spend a roll on the wrong stage.
    #[arg(long)]
    stage: Option<String>,

    /// Why, in your own words. Required; a blank one is refused at the door.
    #[arg(long)]
    reason: String,

    /// Who is deciding. Recorded as the decider.
    #[arg(long, default_value = "operator")]
    operator: String,
}

#[derive(Args, Debug)]
struct ReverifyBaseArgs {
    /// The base commit to re-verify (64 hex characters). Defaults to the red
    /// alert's own commit; naming a different one is refused rather than
    /// applied, so a re-verify aimed from a stale board does not spend a
    /// whole-workspace build on the wrong tree.
    #[arg(long, value_parser = plan::parse_digest_flag)]
    base: Option<dto::DigestHex>,

    /// Why this red does not describe the tree, in your own words. Required;
    /// a blank one is refused at the door.
    #[arg(long)]
    reason: String,

    /// Who is deciding. Recorded as the decider.
    #[arg(long, default_value = "operator")]
    operator: String,
}

#[derive(Args, Debug)]
struct ProposeArgs {
    /// A commit the coordinator's repository can already reach. Name exactly
    /// one of this or `--from-worktree`.
    #[arg(long)]
    from_commit: Option<String>,

    /// A worktree whose `HEAD` the coordinator's repository can already see.
    /// Resolved to a commit, then treated as `--from-commit`.
    #[arg(long)]
    from_worktree: Option<PathBuf>,

    /// Why the coordinator should write this. Required; a blank one is refused
    /// at the door.
    #[arg(long)]
    reason: String,

    /// Who is proposing. Recorded as the decider.
    #[arg(long, default_value = "operator")]
    operator: String,

    /// Operator signing seed: 32 raw bytes or 64 hex characters, mode 0600.
    #[arg(long)]
    seed_file: Option<PathBuf>,

    /// The `KeyId` the coordinator's allowlist names for that seed.
    #[arg(long, default_value = "operator")]
    signer: String,
}

#[derive(Args, Debug)]
struct RepairArgs {
    /// The bloom the member belongs to (64 hex characters).
    #[arg(value_parser = plan::parse_bloom_id)]
    bloom_id: String,

    /// The member to repair. The reserved composition id is accepted here too:
    /// a composition whose weave repair wedged is repaired the same way.
    workpiece: String,

    /// The candidate you already pushed, as `tree:checkout` — two 64-hex
    /// digests. Name exactly one of this, `--from-commit`, or
    /// `--from-worktree`.
    #[arg(long, value_parser = plan::parse_candidate_flag)]
    candidate: Option<dto::CandidateRef>,

    /// A commit the coordinator's repository can already reach. It derives the
    /// candidate, pushes the ref, and records correspondence for you.
    #[arg(long)]
    from_commit: Option<String>,

    /// A worktree whose `HEAD` the coordinator's repository can already see.
    /// Resolved to a commit, then treated as `--from-commit`.
    #[arg(long)]
    from_worktree: Option<String>,

    /// Why you took the lap yourself. Required; a blank one is refused at the
    /// door.
    #[arg(long)]
    reason: String,

    /// Who is deciding. Recorded as the decider.
    #[arg(long, default_value = "operator")]
    operator: String,
}

#[derive(Args, Debug)]
struct SuppressionArgs {
    /// The bloom the member belongs to (64 hex characters).
    #[arg(value_parser = plan::parse_bloom_id)]
    bloom_id: String,

    /// The member whose candidate is carrying the requests.
    workpiece: String,

    /// One suppression request digest you are answering (64 hex characters).
    /// Repeatable, and at least one is required: an answer that closes nothing
    /// is not an answer, and there is deliberately no "everything standing"
    /// spelling.
    #[arg(long = "request", value_parser = plan::parse_digest_flag)]
    requests: Vec<dto::DigestHex>,

    /// The answer. `granted` lets the candidate keep its suppressions; `denied`
    /// bounces the member to a repair lap at its own budget's expense.
    #[arg(long, value_enum)]
    verdict: dto::SuppressionVerdictArg,

    /// Why. Required; for a denial it is what the repair lap is told, and for a
    /// grant it is the record of the judgment.
    #[arg(long)]
    reason: String,

    /// Who answered. Recorded as the decider.
    #[arg(long, default_value = "operator")]
    operator: String,
}

#[derive(Args, Debug)]
struct CancelArgs {
    /// The commission to retire.
    workpiece: String,

    /// Why, in your own words. Required; a blank one is refused at the door.
    #[arg(long)]
    reason: String,

    /// Operator signing seed: 32 raw bytes or 64 hex characters, mode 0600.
    #[arg(long)]
    seed_file: Option<PathBuf>,

    /// The `KeyId` the coordinator's allowlist names for that seed.
    #[arg(long, default_value = "operator")]
    signer: String,
}

#[derive(Args, Debug)]
struct ReopenArgs {
    /// The commission to restore.
    workpiece: String,

    /// Why, in your own words. Required; a blank one is refused at the door.
    #[arg(long)]
    reason: String,

    /// Operator signing seed: 32 raw bytes or 64 hex characters, mode 0600.
    #[arg(long)]
    seed_file: Option<PathBuf>,

    /// The `KeyId` the coordinator's allowlist names for that seed.
    #[arg(long, default_value = "operator")]
    signer: String,
}

/// Shape and seal a new bloom.
///
/// Declared surface, completeness, description, and approval come off each
/// member's stored revision and its approval rows — the same facts
/// `seal_draft` loads, and a caller-supplied projection of them is ignored.
#[derive(Args, Debug)]
struct SealArgs {
    /// Member work-order description, keyed onto every workpiece.
    #[arg(long)]
    task_file: PathBuf,

    /// Author and seal a configuration (`kind=file.json`). Repeatable.
    #[arg(long = "config", value_parser = plan::parse_config_flag)]
    configs: Vec<(String, PathBuf)>,

    /// Named bundle from the checked-in profiles file. Resolves to authored
    /// config digests through `POST /configs`; `--config` flags overlay after.
    #[arg(long)]
    profile: Option<String>,

    /// Draft base: `observed` (default), `mainline`, or a 64-hex digest.
    #[arg(long, default_value = "observed", value_parser = BaseChoice::parse)]
    base: BaseChoice,

    /// Workpiece ids to admit. Defaults to the `--task-file` stem.
    #[arg(long)]
    workpiece: Vec<String>,

    /// Member dependency (`dependent=dependency`). Repeatable. `issue-B=issue-A`
    /// means B depends on A.
    #[arg(long = "edge", value_parser = plan::parse_edge_flag)]
    edges: Vec<(String, String)>,

    /// Per-member scope revision (`workpiece=64-hex`). Repeatable. A member
    /// with no `--revision` entry receives the commission's current revision.
    #[arg(long = "revision", value_parser = plan::parse_revision_flag)]
    revisions: Vec<(String, dto::DigestHex)>,
}

#[derive(Args, Debug)]
struct SupersedeArgs {
    /// Predecessor bloom id (64 hex characters).
    #[arg(value_parser = plan::parse_bloom_id)]
    bloom_id: String,

    /// Successor work-order description, keyed onto every carried workpiece.
    #[arg(long)]
    task_file: PathBuf,

    /// Extra configurations to author and overlay on the predecessor's registry.
    #[arg(long = "config", value_parser = plan::parse_config_flag)]
    configs: Vec<(String, PathBuf)>,

    /// Named bundle from the checked-in profiles file. Resolves and overlays
    /// the same way `--config` does, before those flags.
    #[arg(long)]
    profile: Option<String>,

    /// Successor base: `observed` (default), `mainline`, or a 64-hex digest.
    #[arg(long, default_value = "observed", value_parser = BaseChoice::parse)]
    base: BaseChoice,

    /// Member dependency (`dependent=dependency`). Repeatable. `issue-B=issue-A`
    /// means B depends on A.
    #[arg(long = "edge", value_parser = plan::parse_edge_flag)]
    edges: Vec<(String, String)>,

    /// Predecessor member to drop from the successor. Repeatable.
    #[arg(long = "eject")]
    eject: Vec<String>,

    /// Per-member scope revision (`workpiece=64-hex`). Repeatable. A member
    /// with no `--revision` entry keeps the predecessor's revision.
    #[arg(long = "revision", value_parser = plan::parse_revision_flag)]
    revisions: Vec<(String, dto::DigestHex)>,
}

pub fn run(args: &BloomArgs) -> Result<()> {
    print!(
        "{}",
        run_on_with_policy(&Endpoint::resolve(args.port, args.token.as_deref()), &args.command, &args.approval_policy)?
    );
    Ok(())
}

/// Drive `command` against the repository's default policy file. Only the
/// tests reach for this spelling — [`run`] resolves the operator's
/// `--approval-policy` and calls [`run_on_with_policy`] directly.
#[cfg(test)]
fn run_on(endpoint: &Endpoint, command: &BloomCommand) -> Result<String> {
    run_on_with_policy(endpoint, command, Path::new(DEFAULT_POLICY_FILE))
}

fn run_on_with_policy(endpoint: &Endpoint, command: &BloomCommand, approval_policy: &Path) -> Result<String> {
    let client = Client::new(endpoint);
    match command {
        BloomCommand::Status => Ok(status::render(&client.view()?)),
        BloomCommand::Seal(args) => run_seal(&client, args, approval_policy),
        BloomCommand::Supersede(args) => run_supersede(&client, args),
        BloomCommand::Roll(args) => roll::run(&client, args),
        BloomCommand::Upgrade(args) => upgrade::run(&client, args),
        BloomCommand::Amend(args) => amend::run(&client, args, approval_policy),
        BloomCommand::Archive(args) => archive::run(&client, args),
        BloomCommand::Withdraw(args) => run_withdraw(&client, args),
        BloomCommand::Retry(args) => run_retry(&client, args),
        BloomCommand::Reverify(args) => run_reverify_base(&client, args),
        BloomCommand::Propose(args) => run_propose(&client, args),
        BloomCommand::Repair(args) => run_repair(&client, args),
        BloomCommand::Suppression(args) => run_suppression(&client, args),
        BloomCommand::Cancel(args) => run_cancel(&client, args),
        BloomCommand::Reopen(args) => run_reopen(&client, args),
    }
}

/// Refuse a bloom or a member the live view does not carry, before any write.
///
/// The read-first discipline every member verb here follows: a mistyped
/// workpiece is an operator's typo, and the coordinator's own refusal for it
/// arrives after the override has already been composed and sent.
fn require_member(client: &Client<'_>, bloom_id: &str, workpiece: &str) -> Result<()> {
    if !bloom_in(&client.view()?, bloom_id)?.members.iter().any(|member| member.workpiece == workpiece) {
        bail!("bloom {bloom_id} has no member {workpiece}");
    }
    Ok(())
}

/// Withdraw one member, naming an unknown bloom or workpiece locally before
/// any write — the same read-first shape `run_supersede` uses.
fn run_withdraw(client: &Client<'_>, args: &WithdrawArgs) -> Result<String> {
    require_member(client, &args.bloom_id, &args.workpiece)?;

    let request = dto::WithdrawRequest {
        reason: args.reason.clone(),
        operator: args.operator.clone(),
        cascade: args.cascade,
        idempotency_key: None,
    };
    Ok(render_outcome(&client.withdraw(&args.bloom_id, &args.workpiece, &request)?.outcome))
}

/// Re-dispatch one member's current stage on the candidate it already holds
/// (#5423) — the read-first shape `run_withdraw` uses, plus the two facts the
/// retry has to name.
///
/// The stage and the subject come off the board rather than out of the
/// operator's head: the coordinator refuses a retry that names either wrongly,
/// and reconstructing them by hand is how an operator spends a machinery roll on
/// a member that has already moved on. A named `--stage` is checked against the
/// cursor here so the refusal reads as a stale board rather than as a rejected
/// fact.
fn run_retry(client: &Client<'_>, args: &RetryArgs) -> Result<String> {
    let view = client.view()?;
    let bloom = bloom_in(&view, &args.bloom_id)?;
    let member = bloom
        .members
        .iter()
        .find(|member| member.workpiece == args.workpiece)
        .with_context(|| format!("bloom {} has no member {}", args.bloom_id, args.workpiece))?;
    let cursor = member
        .cursor
        .as_ref()
        .with_context(|| format!("{} has never entered the line, so it has no stage to run again", args.workpiece))?;
    if let Some(named) = &args.stage
        && !named.eq_ignore_ascii_case(&format!("{:?}", cursor.stage))
    {
        bail!("{} is at {:?}, not {named}", args.workpiece, cursor.stage);
    }
    if args.reason.trim().is_empty() {
        bail!("retry reason is required");
    }

    // The subject the fault binds to: the candidate the member is carrying, or
    // the scope revision it was admitted at when it is carrying none.
    let subject = cursor.candidate.as_ref().map_or(member.scope_revision, |candidate| candidate.tree);
    let request = dto::RetryRequest {
        stage: cursor.stage,
        subject,
        reason: args.reason.clone(),
        operator: args.operator.clone(),
        idempotency_key: None,
    };
    Ok(render_outcome(&client.retry(&args.bloom_id, &args.workpiece, &request)?.outcome))
}

/// Re-run `verify.base` on a red receipt — the read-first shape `run_retry`
/// uses, defaulting the target to the alert the board is already showing.
fn run_reverify_base(client: &Client<'_>, args: &ReverifyBaseArgs) -> Result<String> {
    let view = client.view()?;
    let Some(alert) = &view.base_alert else {
        bail!("there is no red base alert to re-verify");
    };
    if let Some(named) = args.base
        && named.digest() != alert.base
    {
        bail!("the board's red base is {}, not {named} (failed: {})", alert.base, alert.failed.join(", "));
    }
    if args.reason.trim().is_empty() {
        bail!("reverify reason is required");
    }

    let base = args.base.map_or(alert.base, dto::DigestHex::digest);
    let request = dto::ReverifyBaseRequest {
        reason: args.reason.clone(),
        operator: args.operator.clone(),
        idempotency_key: None,
    };
    Ok(render_outcome(&client.reverify_base(&base.to_hex(), &request)?.outcome))
}

/// Propose a signed operator change. The command derives the candidate so it
/// can sign the proposal digest; the coordinator re-derives from the same
/// source, records correspondence, and queues the change.
fn run_propose(client: &Client<'_>, args: &ProposeArgs) -> Result<String> {
    if args.reason.trim().is_empty() {
        bail!("propose reason is required");
    }
    let named = usize::from(args.from_commit.is_some()) + usize::from(args.from_worktree.is_some());
    if named != 1 {
        bail!("propose needs exactly one of --from-commit or --from-worktree; {named} were given");
    }

    let key = OperatorKey::load(
        KeyId(args.signer.clone()),
        args.seed_file.as_deref().context("--seed-file is required to sign the proposal")?,
    )?;
    let candidate = resolve_proposal_candidate(args)?;
    let proposal = OperatorProposal { candidate, reason: args.reason.clone(), operator: args.operator.clone() };
    let digest = digest_of(&proposal);
    let request = dto::ProposeRequest {
        candidate: None,
        from_commit: args.from_commit.clone(),
        from_worktree: args.from_worktree.as_ref().map(|path| path.display().to_string()),
        reason: args.reason.clone(),
        operator: args.operator.clone(),
        authorization: key.proposal_of(digest),
        idempotency_key: None,
    };
    let outcome = client.propose(&request)?.outcome;
    Ok(render_proposal(digest, &outcome))
}

fn resolve_proposal_candidate(args: &ProposeArgs) -> Result<aether_bloomery::CandidateRef> {
    let (repo, revision) = match (&args.from_commit, &args.from_worktree) {
        (Some(commit), None) => (PathBuf::from("."), commit.clone()),
        (None, Some(path)) => {
            let head = run_ok(path, &["rev-parse", "--verify", "--end-of-options", "HEAD"])
                .with_context(|| format!("could not read HEAD of {}", path.display()))?;
            (path.clone(), head)
        }
        _ => bail!("propose needs exactly one of --from-commit or --from-worktree"),
    };
    candidate_from_revision(&repo, &revision)
}

fn candidate_from_revision(repo: &Path, revision: &str) -> Result<aether_bloomery::CandidateRef> {
    let commit_peel = format!("{revision}^{{commit}}");
    let commit_hex = run_ok(repo, &["rev-parse", "--verify", "--end-of-options", &commit_peel])
        .with_context(|| format!("commit `{revision}` is not reachable"))?;
    let tree_peel = format!("{commit_hex}^{{tree}}");
    let tree_hex = run_ok(repo, &["rev-parse", "--verify", "--end-of-options", &tree_peel])
        .with_context(|| format!("tree of `{commit_hex}` is not reachable"))?;
    Ok(aether_bloomery::CandidateRef {
        tree: candidate_tree_digest(&object_id(&tree_hex)?),
        checkout: capture_commit_digest(&object_id(&commit_hex)?),
    })
}

fn object_id(hex: &str) -> Result<BackendObjectId> {
    aether_bloomery::decode_hex(hex)
        .map(BackendObjectId::new)
        .with_context(|| format!("resolved `{hex}` is not a git object id"))
}

/// Must match `aether-chassis-bloomery`'s candidate-tree domain tag.
fn candidate_tree_digest(tree: &BackendObjectId) -> Digest {
    #[derive(serde::Serialize)]
    struct CandidateTreeAddress<'a> {
        object: &'a [u8],
    }
    impl aether_bloomery::ContentAddressed for CandidateTreeAddress<'_> {
        const DOMAIN: &'static str = "aether.bloomery.candidate.tree";
    }
    digest_of(&CandidateTreeAddress { object: tree.as_bytes() })
}

/// Must match `aether-chassis-bloomery`'s capture-commit domain tag.
fn capture_commit_digest(commit: &BackendObjectId) -> Digest {
    #[derive(serde::Serialize)]
    struct CaptureCommitAddress<'a> {
        object: &'a [u8],
    }
    impl aether_bloomery::ContentAddressed for CaptureCommitAddress<'_> {
        const DOMAIN: &'static str = "aether.bloomery.candidate.checkout";
    }
    digest_of(&CaptureCommitAddress { object: commit.as_bytes() })
}

fn render_proposal(digest: Digest, outcome: &Outcome) -> String {
    match outcome {
        Outcome::ProposalQueued { offered: true, .. } => {
            format!("proposal {} offered to seal immediately\n", digest.to_hex())
        }
        Outcome::ProposalQueued { offered: false, .. } => {
            format!("proposal {} queued behind a walking bloom\n", digest.to_hex())
        }
        other => format!("proposal {}: {other:?}\n", digest.to_hex()),
    }
}

/// Hand a wedged member the candidate the operator produced and let the
/// ordinary gates judge it (#4957) — the read-first shape `run_withdraw` uses,
/// plus the one choice a repair body has to make.
///
/// The three sources are exclusive at the route, which answers `400` to zero or
/// two of them. Refusing here instead spends a message rather than a round trip,
/// and the local refusal can name what the operator actually typed.
fn run_repair(client: &Client<'_>, args: &RepairArgs) -> Result<String> {
    require_member(client, &args.bloom_id, &args.workpiece)?;

    Ok(render_outcome(&client.repair(&args.bloom_id, &args.workpiece, &repair_body(args)?)?.outcome))
}

/// The repair body, or the refusal for a request that names no source, more
/// than one, or no reason.
fn repair_body(args: &RepairArgs) -> Result<dto::RepairRequest> {
    if args.reason.trim().is_empty() {
        bail!("repair reason is required");
    }
    let named = usize::from(args.candidate.is_some())
        + usize::from(args.from_commit.is_some())
        + usize::from(args.from_worktree.is_some());
    if named != 1 {
        bail!("repair needs exactly one of --candidate, --from-commit, or --from-worktree; {named} were given");
    }

    Ok(dto::RepairRequest {
        candidate: args.candidate,
        from_commit: args.from_commit.clone(),
        from_worktree: args.from_worktree.clone(),
        reason: args.reason.clone(),
        operator: args.operator.clone(),
        idempotency_key: None,
    })
}

/// Answer the suppression requests a member's candidate is carrying (ADR-0193
/// §5) — the read-first shape `run_withdraw` uses.
///
/// The request digests come off the candidate's review evidence rather than off
/// `/view`, which carries no suppression channel, so there is nothing local to
/// check them against. What is checked here is what the route would refuse for
/// the same reason it refuses a blank reason: an answer that closes nothing has
/// answered nothing.
fn run_suppression(client: &Client<'_>, args: &SuppressionArgs) -> Result<String> {
    require_member(client, &args.bloom_id, &args.workpiece)?;
    if args.requests.is_empty() {
        bail!("a suppression answer must name at least one --request; one that closes nothing is not an answer");
    }
    if args.reason.trim().is_empty() {
        bail!("suppression reason is required");
    }

    let request = dto::SuppressionAnswerRequest {
        requests: args.requests.iter().map(|digest| digest.digest()).collect(),
        verdict: args.verdict.into(),
        reason: args.reason.clone(),
        operator: args.operator.clone(),
        idempotency_key: None,
    };
    Ok(render_outcome(&client.suppression(&args.bloom_id, &args.workpiece, &request)?.outcome))
}

/// Cancel one commission, naming an unknown or already-closed workpiece locally
/// before any write — the same read-first shape `run_withdraw` uses.
fn run_cancel(client: &Client<'_>, args: &CancelArgs) -> Result<String> {
    let shown =
        client.commission(&args.workpiece).with_context(|| format!("no commission named {}", args.workpiece))?;
    if shown.status != "open" {
        bail!("commission {} is {}, not open", args.workpiece, shown.status);
    }
    if args.reason.trim().is_empty() {
        bail!("cancel reason is required");
    }

    let key = OperatorKey::load(
        KeyId(args.signer.clone()),
        args.seed_file.as_deref().context("--seed-file is required to sign the cancel")?,
    )?;
    let stored = client.cancel(
        &args.workpiece,
        &dto::CancelCommissionRequest { statement: key.cancel_of(shown.intent), reason: args.reason.clone() },
    )?;
    Ok(format!("cancelled {} {} ({})\n", args.workpiece, stored.digest, stored.status))
}

/// Reopen one commission, naming an unknown or not-landed workpiece locally
/// before any write — the same read-first shape [`run_cancel`] uses.
///
/// The coordinator is the authority on whether the workpiece may come back: it
/// refuses one that a bloom actually resolved. This read only spends the
/// operator's attention rather than their signature on the obvious cases.
fn run_reopen(client: &Client<'_>, args: &ReopenArgs) -> Result<String> {
    let shown =
        client.commission(&args.workpiece).with_context(|| format!("no commission named {}", args.workpiece))?;
    if shown.status != "landed" {
        bail!("commission {} is {}, not landed", args.workpiece, shown.status);
    }
    if args.reason.trim().is_empty() {
        bail!("reopen reason is required");
    }

    let key = OperatorKey::load(
        KeyId(args.signer.clone()),
        args.seed_file.as_deref().context("--seed-file is required to sign the reopen")?,
    )?;
    let restored = client.reopen(
        &args.workpiece,
        &dto::ReopenCommissionRequest { statement: key.reopen_of(shown.intent), reason: args.reason.clone() },
    )?;
    Ok(format!("reopened {} {} ({})\n", args.workpiece, restored.digest, restored.status))
}

fn run_seal(client: &Client<'_>, args: &SealArgs, approval_policy: &Path) -> Result<String> {
    let task = plan::read_task_file(&args.task_file)?;
    plan::require_task(&task, &args.task_file)?;
    let workpieces = plan::seal_workpieces(&args.workpiece, &args.task_file)?;
    let mut pins = Vec::with_capacity(workpieces.len());
    let mut surfaces = Vec::with_capacity(workpieces.len());
    for workpiece in &workpieces {
        let (revision, declared_surface) = require_sealable_commission(client, workpiece)?;
        pins.push((workpiece.clone(), revision));
        surfaces.push((workpiece.clone(), declared_surface));
    }
    if let Some(policy) = plan::load_policy(approval_policy) {
        for (workpiece, declared_surface) in &surfaces {
            plan::refuse_unnamed_file_entries(&policy, workpiece, declared_surface)?;
        }
    }
    let default_revision = pins.first().map(|(_, digest)| *digest).context("seal names no members")?;
    pins.extend(args.revisions.iter().map(|(workpiece, digest)| (workpiece.clone(), digest.digest())));

    let view = client.view()?;
    let base = plan::resolve_base(&args.base, &view);
    let authored = plan::author_profile_and_flags(client, args.profile.as_deref(), &args.configs)?;
    let patch = plan::seal_patch(
        &workpieces,
        default_revision,
        &pins,
        base,
        authored.configs,
        authored.forecast.unwrap_or_default(),
    )?;
    let draft = client.open_draft()?;
    client.patch_draft(&draft.draft_id, &patch)?;
    let outcome = client.seal(&draft.draft_id, &plan::seal_request(&args.edges))?;
    Ok(render_outcome(&outcome.outcome))
}

/// One `--workpiece`'s commission, or a local refusal that names it.
///
/// The store already holds the revision and its approval; guessing a digest
/// from the task file can never match. Missing, not-open, and unapproved are
/// the same facts the door would 422, named here so the draft is never opened.
fn require_sealable_commission(client: &Client<'_>, workpiece: &str) -> Result<(Digest, Vec<String>)> {
    let shown = client.commission(workpiece).with_context(|| format!("no commission named {workpiece}"))?;
    if shown.status != "open" {
        bail!("commission {workpiece} is {}, not open", shown.status);
    }
    let revision =
        shown.current_revision.with_context(|| format!("commission {workpiece} names no current revision"))?;
    // The show view already scopes `approvals` to the tip. Empty is the door's
    // AbsentApproval; matching `words` here would refuse a well-formed row whose
    // bytes the JSON front rendered in a shape this mirror does not decode.
    if shown.approvals.is_empty() {
        bail!("{workpiece} carries no stored approval over its current revision");
    }
    Ok((revision, shown.current.map(|current| current.declared_surface).unwrap_or_default()))
}

fn run_supersede(client: &Client<'_>, args: &SupersedeArgs) -> Result<String> {
    let task = plan::read_task_file(&args.task_file)?;
    plan::require_task(&task, &args.task_file)?;
    let view = client.view()?;
    bloom_in(&view, &args.bloom_id)?;
    let spec = client.spec_for(&args.bloom_id)?;
    let authored = plan::author_profile_and_flags(client, args.profile.as_deref(), &args.configs)?;
    let mut configs = spec.configs().clone();
    configs.overlay(authored.configs);
    let base = plan::resolve_base(&args.base, &view);
    let mut patch = plan::successor_patch(&spec, base, configs);
    if let Some(forecast) = authored.forecast {
        patch.forecast = Some(forecast);
    }
    let proposals = patch.proposals.get_or_insert_with(Vec::new);
    plan::eject_members(proposals, &args.eject)?;
    let pins: Vec<_> = args.revisions.iter().map(|(workpiece, digest)| (workpiece.clone(), digest.digest())).collect();
    plan::pin_revisions(proposals, &pins)?;
    let draft = client.open_draft()?;
    client.patch_draft(&draft.draft_id, &patch)?;
    let outcome = client.supersede(&args.bloom_id, &plan::supersede_request(&draft.draft_id, &args.edges))?;
    Ok(render_outcome(&outcome.outcome))
}

pub fn render_outcome(outcome: &Outcome) -> String {
    format!("{}\n", serde_json::to_string_pretty(outcome).unwrap_or_else(|_| format!("{outcome:?}")))
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::io::{self, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::panic::{self, AssertUnwindSafe};
    use std::path::PathBuf;
    use std::process;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::thread;
    use std::time::Duration;

    use aether_bloomery::{
        BloomDraft, ConfigRegistry, Digest, Evidence, EvidenceKind, Forecast, Membership, WorkpieceId,
    };
    use serde_json::{Value, json};

    use super::{
        BloomCommand, CancelArgs, Endpoint, ProposeArgs, ReopenArgs, RepairArgs, SealArgs, SupersedeArgs,
        SuppressionArgs, repair_body, run_on,
    };
    use crate::bloom::dto;
    use crate::bloom::dto::DigestHex;
    use crate::bloom::hex;
    use crate::bloom::plan::BaseChoice;

    #[derive(Clone, Debug)]
    struct Recorded {
        method: String,
        path: String,
        body: Option<Value>,
    }

    fn digest(byte: u8) -> Digest {
        Digest::from_bytes([byte; 32])
    }

    fn hex_of(digest: Digest) -> String {
        hex::encode(digest.as_bytes())
    }

    fn predecessor_spec() -> (String, aether_bloomery::BloomSpec) {
        predecessor_spec_of(&[("wp-1", 7)])
    }

    fn predecessor_spec_of(members: &[(&str, u8)]) -> (String, aether_bloomery::BloomSpec) {
        let mut configs = ConfigRegistry::default();
        configs.insert_named("aether.bloomery.stage_catalog", digest(0xaa));
        let spec = BloomDraft {
            proposals: members
                .iter()
                .map(|(workpiece, revision)| Membership {
                    workpiece: WorkpieceId((*workpiece).to_owned()),
                    scope_revision: digest(*revision),
                    configs: ConfigRegistry::default(),
                    approval: Evidence { subject: digest(*revision), kind: EvidenceKind::Approval, detail: digest(9) },
                })
                .collect(),
            base: digest(1),
            configs,
            forecast: Forecast::default(),
        }
        .seal();
        (hex_of(spec.id().0), spec)
    }

    fn supersede_args(
        bloom_id: String,
        task: PathBuf,
        edges: Vec<(String, String)>,
        eject: Vec<String>,
        revisions: Vec<(String, DigestHex)>,
    ) -> BloomCommand {
        BloomCommand::Supersede(SupersedeArgs {
            bloom_id,
            task_file: task,
            configs: Vec::new(),
            profile: None,
            base: BaseChoice::Observed,
            edges,
            eject,
            revisions,
        })
    }

    fn seal_args(task: PathBuf, workpiece: Vec<String>) -> SealArgs {
        SealArgs {
            task_file: task,
            configs: Vec::new(),
            profile: None,
            base: BaseChoice::Observed,
            workpiece,
            edges: Vec::new(),
            revisions: Vec::new(),
        }
    }

    fn serve_one(mut stream: TcpStream, handler: &impl Fn(&Recorded) -> (u16, Value), log: &Mutex<Vec<Recorded>>) {
        let _ = stream.set_nonblocking(false);
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        while let Ok(n) = stream.read(&mut chunk) {
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            let Some(head_end) = buf.windows(4).position(|window| window == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&buf[..head_end]);
            let content_length = head.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().ok()).flatten()
            });
            let body_start = head_end + 4;
            if let Some(length) = content_length
                && buf.len() < body_start + length
            {
                continue;
            }
            let mut parts = head.split_whitespace();
            let method = parts.next().unwrap_or("").to_owned();
            let path = parts.next().unwrap_or("").to_owned();
            let body =
                content_length.and_then(|length| serde_json::from_slice(&buf[body_start..body_start + length]).ok());
            let request = Recorded { method, path, body };
            log.lock().expect("log").push(request.clone());
            let (status, reply) = handler(&request);
            let payload = serde_json::to_vec(&reply).expect("encode reply");
            let head = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&payload);
            break;
        }
    }

    fn with_fake<H, T>(handler: H, body: impl FnOnce(u16) -> T) -> (T, Vec<Recorded>)
    where
        H: Fn(&Recorded) -> (u16, Value) + Send + Sync,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake coordinator");
        listener.set_nonblocking(true).expect("nonblocking accept");
        let port = listener.local_addr().expect("local addr").port();
        let log = Mutex::new(Vec::new());
        let stop = AtomicBool::new(false);
        let result = thread::scope(|scope| {
            scope.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => serve_one(stream, &handler, &log),
                        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            // A panic inside `body` must still flip `stop`: otherwise the
            // accept loop never leaves and `thread::scope` waits out the
            // nextest slow-timeout instead of reporting the panic.
            let result = panic::catch_unwind(AssertUnwindSafe(|| body(port)));
            stop.store(true, Ordering::Relaxed);
            result
        });
        (result.unwrap_or_else(|payload| panic::resume_unwind(payload)), log.into_inner().expect("log"))
    }

    fn temp_task(name: &str, text: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!("aether-xtask-bloom-{name}-{}-{seq}", process::id()));
        fs::write(&path, text).expect("write task file");
        path
    }

    fn open_approved(workpiece: &str, revision: Digest) -> Value {
        as_json(&aether_bloomery::CommissionShowView {
            id: WorkpieceId(workpiece.to_owned()),
            intent: digest(1),
            current_revision: Some(revision),
            current_ordinal: None,
            status: "open".to_owned(),
            current: Some(aether_bloomery::ScopeRevision {
                schema: 1,
                workpiece: WorkpieceId(workpiece.to_owned()),
                predecessor: None,
                problem: "p".into(),
                design: "d".into(),
                plan: "p".into(),
                declared_surface: vec!["docs/guide/**".into()],
                dogfood_brief: String::new(),
                routing: aether_bloomery::ScopeRouting { size: "small".into(), model: "grok-4.6".into() },
                dependencies: Vec::new(),
                description: String::new(),
                implements: Vec::new(),
                declared_crates: Vec::new(),
                declared_reads: Vec::new(),
            }),
            current_unreadable: None,
            approvals: vec![aether_bloomery::Statement {
                words: revision.as_bytes().to_vec(),
                provenance: aether_bloomery::Provenance::ObservationAttestation(aether_bloomery::Observation {
                    source: "test".into(),
                }),
                parents: Vec::new(),
            }],
            scope_verify: None,
        })
    }

    fn find<'a>(log: &'a [Recorded], method: &str, suffix: &str) -> &'a Recorded {
        log.iter()
            .find(|entry| entry.method == method && entry.path.ends_with(suffix))
            .unwrap_or_else(|| panic!("missing {method} …{suffix} in {log:?}"))
    }

    #[test]
    fn supersede_defaults_observed_base_predecessor_configs_and_scope_revision() {
        // Tripwire: `cargo xtask bloom supersede <id> --task-file task.md`
        // must pin the successor on the current observed head, reuse the
        // predecessor's sealed configs by digest, and carry the predecessor's
        // scope revision. A silent change to any of those three defaults
        // would drop the claim or rebase onto the wrong tree.
        let (bloom_id, spec_wire) = predecessor_spec();
        let observed = hex_of(digest(2));
        let catalog = hex_of(digest(0xaa));
        let revision = hex_of(digest(7));
        let bloom_id_for_view = bloom_id.clone();
        let spec_for_journal = spec_wire;
        let _revision_for_view = revision.clone();

        let task = temp_task("supersede", "recover the wedged member");
        let (output, log) = with_fake(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/view") => {
                    (200, live_view(Digest::from_hex(&bloom_id_for_view).expect("id"), "wp-1", digest(7)))
                }
                (method, path) if method == "GET" && path.starts_with("/journal") => {
                    (200, journal_seal(&spec_for_journal))
                }
                ("POST", "/drafts") => (201, draft_reply("1")),
                ("PATCH", "/drafts/1") => (200, draft_reply("1")),
                (method, path) if method == "POST" && path.ends_with("/supersede") => (
                    200,
                    json!({ "outcome": { "Superseded": { "predecessor": bloom_id_for_view.clone(), "successor": hex_of(digest(3)) } } }),
                ),
                _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
            },
            |port| {
                run_on(
                    &Endpoint { host: "127.0.0.1".to_owned(), port, token: None },
                    &BloomCommand::Supersede(SupersedeArgs {
                        bloom_id: bloom_id.clone(),
                        task_file: task.clone(),
                        configs: Vec::new(),
                        profile: None,
                        base: BaseChoice::Observed,
                        edges: Vec::new(),
                        eject: Vec::new(),
                        revisions: Vec::new(),
                    }),
                )
                .expect("supersede against the fake coordinator")
            },
        );
        fs::remove_file(&task).ok();
        let patch = find(&log, "PATCH", "/drafts/1").body.as_ref().expect("patch body");
        assert_eq!(patch["base"], observed, "default base is the current observed head: {patch}");
        assert_eq!(
            patch["configs"]["entries"]["aether.bloomery.stage_catalog"], catalog,
            "predecessor configs are reused by digest: {patch}"
        );
        assert_eq!(patch["proposals"][0]["scope_revision"], revision, "predecessor scope revision is carried: {patch}");
        assert_eq!(patch["proposals"][0]["workpiece"], "wp-1");

        let supersede =
            find(&log, "POST", &format!("/blooms/{bloom_id}/supersede")).body.as_ref().expect("supersede body");
        assert_eq!(supersede["successor_draft"], "1");
        assert!(supersede.get("projections").is_none(), "the door ignores projections: {supersede}");
        assert!(supersede.get("descriptions").is_none(), "the door ignores descriptions: {supersede}");
        assert!(supersede.get("edges").is_none(), "an edgeless supersede omits the edges field: {supersede}");
        assert!(output.contains("Superseded"), "outcome is printed: {output}");
    }

    #[test]
    fn supersede_sends_declared_edges_on_the_typed_body() {
        // `--edge issue-B=issue-A` must reach the successor door as B depends
        // on A. A swapped pair, or dropping the field, would journal the
        // opposite graph or none at all — the same bug the seal flag closes.
        let (bloom_id, spec_wire) = predecessor_spec_of(&[("issue-A", 1), ("issue-B", 2)]);
        let bloom_id_for_view = bloom_id.clone();
        let spec_for_journal = spec_wire;
        let task = temp_task("supersede-edge", "recover B on A");
        let (_, log) = with_fake(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/view") => (
                    200,
                    live_bloom(
                        Digest::from_hex(&bloom_id_for_view).expect("id"),
                        &[("issue-A", digest(1)), ("issue-B", digest(2))],
                    ),
                ),
                (method, path) if method == "GET" && path.starts_with("/journal") => {
                    (200, journal_seal(&spec_for_journal))
                }
                ("POST", "/drafts") => (201, draft_reply("1")),
                ("PATCH", "/drafts/1") => (200, draft_reply("1")),
                (method, path) if method == "POST" && path.ends_with("/supersede") => (
                    200,
                    json!({ "outcome": { "Superseded": { "predecessor": bloom_id_for_view.clone(), "successor": hex_of(digest(3)) } } }),
                ),
                _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
            },
            |port| {
                run_on(
                    &Endpoint { host: "127.0.0.1".to_owned(), port, token: None },
                    &supersede_args(
                        bloom_id.clone(),
                        task.clone(),
                        vec![("issue-B".to_owned(), "issue-A".to_owned())],
                        Vec::new(),
                        Vec::new(),
                    ),
                )
                .expect("supersede --edge against the fake coordinator")
            },
        );
        fs::remove_file(&task).ok();
        let supersede =
            find(&log, "POST", &format!("/blooms/{bloom_id}/supersede")).body.as_ref().expect("supersede body");
        assert_eq!(supersede["edges"][0]["member"], "issue-B");
        assert_eq!(supersede["edges"][0]["depends_on"], "issue-A");
    }

    #[test]
    fn supersede_ejects_a_named_predecessor_from_proposals() {
        // `--eject` must drop the named member from the successor draft.
        // Leaving it in would re-admit the workpiece the operator just tried
        // to leave out.
        let (bloom_id, spec_wire) = predecessor_spec_of(&[("wp-1", 7), ("wp-2", 8)]);
        let bloom_id_for_view = bloom_id.clone();
        let spec_for_journal = spec_wire;
        let task = temp_task("supersede-eject", "recover without the wedged member");
        let (_, log) = with_fake(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/view") => (
                    200,
                    live_bloom(
                        Digest::from_hex(&bloom_id_for_view).expect("id"),
                        &[("wp-1", digest(7)), ("wp-2", digest(8))],
                    ),
                ),
                (method, path) if method == "GET" && path.starts_with("/journal") => {
                    (200, journal_seal(&spec_for_journal))
                }
                ("POST", "/drafts") => (201, draft_reply("1")),
                ("PATCH", "/drafts/1") => (200, draft_reply("1")),
                (method, path) if method == "POST" && path.ends_with("/supersede") => (
                    200,
                    json!({ "outcome": { "Superseded": { "predecessor": bloom_id_for_view.clone(), "successor": hex_of(digest(3)) } } }),
                ),
                _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
            },
            |port| {
                run_on(
                    &Endpoint { host: "127.0.0.1".to_owned(), port, token: None },
                    &supersede_args(bloom_id.clone(), task.clone(), Vec::new(), vec!["wp-2".to_owned()], Vec::new()),
                )
                .expect("supersede --eject against the fake coordinator")
            },
        );
        fs::remove_file(&task).ok();
        let patch = find(&log, "PATCH", "/drafts/1").body.as_ref().expect("patch body");
        let proposals = patch["proposals"].as_array().expect("proposals");
        assert_eq!(proposals.len(), 1, "ejected member is gone from the successor draft: {patch}");
        assert_eq!(proposals[0]["workpiece"], "wp-1");

        let supersede =
            find(&log, "POST", &format!("/blooms/{bloom_id}/supersede")).body.as_ref().expect("supersede body");
        assert_eq!(supersede["successor_draft"], "1");
        assert!(supersede.get("projections").is_none(), "the door ignores projections: {supersede}");
    }

    #[test]
    fn supersede_refuses_an_unknown_or_emptying_eject() {
        // The tool is the refuse: an unknown name that reached the door would
        // silently stay in the successor, and an emptied membership cannot seal.
        let (bloom_id, spec_wire) = predecessor_spec();
        let bloom_id_for_view = bloom_id.clone();
        let spec_for_journal = spec_wire;
        let task = temp_task("supersede-eject-refuse", "should not dispatch");
        let ((unknown, emptying), _) = with_fake(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/view") => {
                    (200, live_view(Digest::from_hex(&bloom_id_for_view).expect("id"), "wp-1", digest(7)))
                }
                (method, path) if method == "GET" && path.starts_with("/journal") => {
                    (200, journal_seal(&spec_for_journal))
                }
                _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
            },
            |port| {
                let endpoint = Endpoint { host: "127.0.0.1".to_owned(), port, token: None };
                let unknown = run_on(
                    &endpoint,
                    &supersede_args(bloom_id.clone(), task.clone(), Vec::new(), vec!["wp-z".to_owned()], Vec::new()),
                )
                .expect_err("unknown eject");
                let emptying = run_on(
                    &endpoint,
                    &supersede_args(bloom_id.clone(), task.clone(), Vec::new(), vec!["wp-1".to_owned()], Vec::new()),
                )
                .expect_err("emptying eject");
                (unknown, emptying)
            },
        );
        fs::remove_file(&task).ok();
        assert!(unknown.to_string().contains("wp-z"), "unknown eject names the workpiece: {unknown}");
        assert!(emptying.to_string().contains("no members"), "emptying eject names the empty membership: {emptying}");
    }

    #[test]
    fn supersede_pins_a_rescoped_member() {
        // `--revision wp-2=<digest>` must overwrite that member's successor
        // scope revision and approval subject so a re-scoped member can pass
        // the admission door. The unnamed sibling keeps the predecessor's
        // revision.
        let (bloom_id, spec_wire) = predecessor_spec_of(&[("wp-1", 7), ("wp-2", 8)]);
        let bloom_id_for_view = bloom_id.clone();
        let spec_for_journal = spec_wire;
        let pinned = hex_of(digest(0x99));
        let task = temp_task("supersede-revision", "recover a re-scoped member");
        let (_, log) = with_fake(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/view") => (
                    200,
                    live_bloom(
                        Digest::from_hex(&bloom_id_for_view).expect("id"),
                        &[("wp-1", digest(7)), ("wp-2", digest(8))],
                    ),
                ),
                (method, path) if method == "GET" && path.starts_with("/journal") => {
                    (200, journal_seal(&spec_for_journal))
                }
                ("POST", "/drafts") => (201, draft_reply("1")),
                ("PATCH", "/drafts/1") => (200, draft_reply("1")),
                (method, path) if method == "POST" && path.ends_with("/supersede") => (
                    200,
                    json!({ "outcome": { "Superseded": { "predecessor": bloom_id_for_view.clone(), "successor": hex_of(digest(3)) } } }),
                ),
                _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
            },
            |port| {
                run_on(
                    &Endpoint { host: "127.0.0.1".to_owned(), port, token: None },
                    &supersede_args(
                        bloom_id.clone(),
                        task.clone(),
                        Vec::new(),
                        Vec::new(),
                        vec![("wp-2".to_owned(), DigestHex::from_bytes([0x99; 32]))],
                    ),
                )
                .expect("supersede --revision against the fake coordinator")
            },
        );
        fs::remove_file(&task).ok();
        let patch = find(&log, "PATCH", "/drafts/1").body.as_ref().expect("patch body");
        assert_eq!(patch["proposals"][0]["workpiece"], "wp-1");
        assert_eq!(patch["proposals"][0]["scope_revision"], hex_of(digest(7)));
        assert_eq!(patch["proposals"][0]["approval"]["subject"], hex_of(digest(7)));
        assert_eq!(patch["proposals"][1]["workpiece"], "wp-2");
        assert_eq!(patch["proposals"][1]["scope_revision"], pinned);
        assert_eq!(patch["proposals"][1]["approval"]["subject"], pinned);

        let supersede =
            find(&log, "POST", &format!("/blooms/{bloom_id}/supersede")).body.as_ref().expect("supersede body");
        assert_eq!(supersede["successor_draft"], "1");
        assert!(supersede.get("projections").is_none(), "the door ignores projections: {supersede}");
    }

    #[test]
    fn status_renders_the_live_list() {
        let predecessor = digest(0x11);
        let successor = digest(0x22);
        let mut pred = dto::test_bloom(
            predecessor,
            aether_bloomery::BloomStatus::Superseded,
            vec![dto::test_member("wp-1", digest(7))],
        );
        pred.superseded_by = Some(aether_bloomery::BloomId(successor));
        let view = dto::test_view(
            digest(1),
            digest(2),
            vec![
                pred,
                dto::test_bloom(
                    successor,
                    aether_bloomery::BloomStatus::Sealed,
                    vec![dto::test_member("wp-1", digest(7))],
                ),
            ],
        );
        let (text, _) = with_fake(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/view") => (200, as_json(&view)),
                _ => (404, json!({ "error": "unexpected" })),
            },
            |port| {
                run_on(&Endpoint { host: "127.0.0.1".to_owned(), port, token: None }, &BloomCommand::Status)
                    .expect("status")
            },
        );
        assert!(text.contains("superseded by"), "supersession is linked: {text}");
        assert!(text.contains("sealed"), "successor status is named: {text}");
        assert!(text.contains("wp-1"), "members are listed: {text}");
    }

    fn as_json(value: &impl serde::Serialize) -> Value {
        serde_json::from_slice(&hex::to_vec(value).expect("fixture encodes")).expect("fixture json")
    }

    fn empty_view() -> Value {
        as_json(&dto::test_view(digest(1), digest(2), Vec::new()))
    }

    fn live_view(bloom_id: Digest, workpiece: &str, revision: Digest) -> Value {
        live_bloom(bloom_id, &[(workpiece, revision)])
    }

    fn live_bloom(bloom_id: Digest, members: &[(&str, Digest)]) -> Value {
        as_json(&dto::test_view(
            digest(1),
            digest(2),
            vec![dto::test_bloom(
                bloom_id,
                aether_bloomery::BloomStatus::Sealed,
                members.iter().map(|(workpiece, revision)| dto::test_member(workpiece, *revision)).collect(),
            )],
        ))
    }

    fn journal_seal(spec: &aether_bloomery::BloomSpec) -> Value {
        as_json(&aether_bloomery::JournalView {
            records: vec![aether_bloomery::JournalEntry {
                sequence: 1,
                idempotency_key: "k".to_owned(),
                event: aether_bloomery::Event {
                    idempotency_key: aether_bloomery::IdempotencyKey("k".to_owned()),
                    fact: aether_bloomery::Fact::Seal(spec.clone()),
                },
                outcome: aether_bloomery::Outcome::Duplicate,
                decider: "test".to_owned(),
            }],
            total_matched: 1,
            shown: 1,
            truncated: false,
            next_from_sequence: None,
            notice: None,
        })
    }

    fn draft_reply(id: &str) -> Value {
        as_json(&aether_bloomery::DraftView { draft_id: id.to_owned(), draft: BloomDraft::default() })
    }

    fn commission_route(path: &str) -> Option<&str> {
        path.strip_prefix("/commissions/")
    }

    #[test]
    fn seal_authors_config_and_sends_typed_bodies() {
        let catalog = hex_of(digest(0xcc));
        let catalog_for_reply = catalog.clone();
        let stored = hex_of(digest(0xaa));
        let task = temp_task("seal-task", "build the authoring layer");
        let config = temp_task("catalog.json", r#"{"bindings":[]}"#);
        let (output, log) = with_fake(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", path) if let Some(id) = commission_route(path) => (200, open_approved(id, digest(0xaa))),
                ("GET", "/view") => (200, empty_view()),
                ("POST", "/configs") => {
                    let body = request.body.as_ref().expect("config body");
                    assert_eq!(body["kind"], "aether.bloomery.stage_catalog");
                    assert!(body["value"].is_object(), "config value is the file JSON, not a hand-rolled envelope");
                    (200, json!({ "digest": catalog_for_reply, "kind": "aether.bloomery.stage_catalog" }))
                }
                ("POST", "/drafts") => (201, draft_reply("3")),
                ("PATCH", "/drafts/3") => (200, draft_reply("3")),
                ("POST", "/drafts/3/seal") => (200, json!({ "outcome": { "Sealed": hex_of(digest(4)) } })),
                _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
            },
            |port| {
                run_on(
                    &Endpoint { host: "127.0.0.1".to_owned(), port, token: None },
                    &BloomCommand::Seal(SealArgs {
                        configs: vec![("aether.bloomery.stage_catalog".to_owned(), config.clone())],
                        ..seal_args(task.clone(), vec!["wp-seal".to_owned()])
                    }),
                )
                .expect("seal against the fake coordinator")
            },
        );
        fs::remove_file(&task).ok();
        fs::remove_file(&config).ok();
        let patch = find(&log, "PATCH", "/drafts/3").body.as_ref().expect("patch body");
        assert_eq!(patch["base"], hex_of(digest(2)), "seal defaults base to observed");
        assert_eq!(patch["configs"]["entries"]["aether.bloomery.stage_catalog"], catalog);
        assert_eq!(patch["proposals"][0]["workpiece"], "wp-seal");
        assert_eq!(patch["proposals"][0]["scope_revision"], stored);

        let seal = find(&log, "POST", "/drafts/3/seal").body.as_ref().expect("seal body");
        assert!(seal.get("projections").is_none(), "the door ignores projections: {seal}");
        assert!(seal.get("descriptions").is_none(), "the door ignores descriptions: {seal}");
        assert!(seal.get("edges").is_none(), "an edgeless seal omits the edges field: {seal}");
        assert!(output.contains("Sealed"), "outcome is printed: {output}");
    }

    #[test]
    fn seal_sends_declared_edges_on_the_typed_body() {
        // `--edge issue-B=issue-A` must reach the door as B depends on A. A
        // swapped pair, or dropping the field, would journal the opposite
        // graph or none at all.
        let task = temp_task("seal-edge", "build B on A");
        let (_, log) = with_fake(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", path) if let Some(id) = commission_route(path) => (200, open_approved(id, digest(0xaa))),
                ("GET", "/view") => (200, empty_view()),
                ("POST", "/drafts") => (201, draft_reply("5")),
                ("PATCH", "/drafts/5") => (200, draft_reply("5")),
                ("POST", "/drafts/5/seal") => (200, json!({ "outcome": { "Sealed": hex_of(digest(4)) } })),
                _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
            },
            |port| {
                run_on(
                    &Endpoint { host: "127.0.0.1".to_owned(), port, token: None },
                    &BloomCommand::Seal(SealArgs {
                        edges: vec![("issue-B".to_owned(), "issue-A".to_owned())],
                        ..seal_args(task.clone(), vec!["issue-A".to_owned(), "issue-B".to_owned()])
                    }),
                )
                .expect("seal --edge against the fake coordinator")
            },
        );
        fs::remove_file(&task).ok();
        let seal = find(&log, "POST", "/drafts/5/seal").body.as_ref().expect("seal body");
        assert_eq!(seal["edges"][0]["member"], "issue-B");
        assert_eq!(seal["edges"][0]["depends_on"], "issue-A");
    }

    #[test]
    fn a_seal_issued_twice_with_the_same_idempotency_key_sends_the_key_both_times() {
        // Tripwire: before the shared SealRequest the CLI had no field for the
        // key, so a resend could not be a duplicate. Both POSTs must carry it.
        let request = dto::SealRequest { idempotency_key: Some("once".to_owned()), edges: Vec::new() };
        let ((), log) = with_fake(
            |recorded| match (recorded.method.as_str(), recorded.path.as_str()) {
                ("POST", "/drafts/1/seal") => (200, json!({ "outcome": "Duplicate" })),
                _ => (404, json!({ "error": format!("unexpected {} {}", recorded.method, recorded.path) })),
            },
            |port| {
                let endpoint = Endpoint { host: "127.0.0.1".to_owned(), port, token: None };
                let client = crate::bloom::client::Client::new(&endpoint);
                client.seal("1", &request).expect("first seal");
                client.seal("1", &request).expect("second seal");
            },
        );
        let keys: Vec<_> = log
            .iter()
            .filter_map(|entry| entry.body.as_ref().and_then(|body| body.get("idempotency_key")).cloned())
            .collect();
        assert_eq!(keys, vec![json!("once"), json!("once")], "both POSTs carry the same key: {log:?}");
    }

    #[test]
    fn a_seal_takes_each_members_revision_from_the_store() {
        // Before this change, a member without `--revision` received a bare
        // sha256 of the task file, which no store row can match. The store's
        // `current_revision` is the digest the door will admit.
        let task = temp_task("seal-store-revision", "build A and B");
        let rev_a = hex_of(digest(0x11));
        let rev_b = hex_of(digest(0x22));
        let (_, log) = with_fake(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/commissions/issue-A") => (200, open_approved("issue-A", digest(0x11))),
                ("GET", "/commissions/issue-B") => (200, open_approved("issue-B", digest(0x22))),
                ("GET", "/view") => (200, empty_view()),
                ("POST", "/drafts") => (201, draft_reply("6")),
                ("PATCH", "/drafts/6") => (200, draft_reply("6")),
                ("POST", "/drafts/6/seal") => (200, json!({ "outcome": { "Sealed": hex_of(digest(4)) } })),
                _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
            },
            |port| {
                run_on(
                    &Endpoint { host: "127.0.0.1".to_owned(), port, token: None },
                    &BloomCommand::Seal(seal_args(task.clone(), vec!["issue-A".to_owned(), "issue-B".to_owned()])),
                )
                .expect("seal against stored commissions")
            },
        );
        fs::remove_file(&task).ok();
        let patch = find(&log, "PATCH", "/drafts/6").body.as_ref().expect("patch body");
        assert_eq!(patch["proposals"][0]["workpiece"], "issue-A");
        assert_eq!(patch["proposals"][0]["scope_revision"], rev_a);
        assert_eq!(patch["proposals"][0]["approval"]["subject"], rev_a);
        assert_eq!(patch["proposals"][1]["workpiece"], "issue-B");
        assert_eq!(patch["proposals"][1]["scope_revision"], rev_b);
        assert_eq!(patch["proposals"][1]["approval"]["subject"], rev_b);

        let seal = find(&log, "POST", "/drafts/6/seal").body.as_ref().expect("seal body");
        assert!(seal.get("projections").is_none(), "the door ignores projections: {seal}");
        assert!(log.iter().any(|entry| entry.method == "GET" && entry.path == "/commissions/issue-A"));
        assert!(log.iter().any(|entry| entry.method == "GET" && entry.path == "/commissions/issue-B"));
    }

    #[test]
    fn an_unapproved_member_is_refused_before_the_draft_is_opened() {
        // An open commission with no approval over the tip is the operator
        // skipping submit. Naming that here, before POST /drafts, is the
        // refusal that is worth more than a 422 after the draft is already
        // open.
        let task = temp_task("seal-unapproved", "should not dispatch");
        let (error, log) = with_fake(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/commissions/issue-N") => {
                    let mut shown = open_approved("issue-N", digest(0x11));
                    shown["approvals"] = json!([]);
                    (200, shown)
                }
                _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
            },
            |port| {
                run_on(
                    &Endpoint { host: "127.0.0.1".to_owned(), port, token: None },
                    &BloomCommand::Seal(seal_args(task.clone(), vec!["issue-N".to_owned()])),
                )
                .expect_err("an unapproved member is refused")
            },
        );
        fs::remove_file(&task).ok();
        assert!(error.to_string().contains("issue-N"), "the refusal names the workpiece: {error}");
        assert!(
            !log.iter().any(|entry| entry.method == "POST" && entry.path.starts_with("/drafts")),
            "an unapproved member must not open a draft: {log:?}"
        );
    }

    #[test]
    fn seal_sends_per_member_scope_revisions_on_the_patch() {
        // `--revision` overlays the store default. A member the flag does not
        // name keeps the commission's current revision.
        let task = temp_task("seal-revisions", "build A and B");
        let rev_a = hex_of(digest(0x11));
        let rev_b = hex_of(digest(0x22));
        let rev_c = hex_of(digest(0x33));
        let (_, log) = with_fake(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/commissions/issue-A") => (200, open_approved("issue-A", digest(0x31))),
                ("GET", "/commissions/issue-B") => (200, open_approved("issue-B", digest(0x32))),
                ("GET", "/commissions/issue-C") => (200, open_approved("issue-C", digest(0x33))),
                ("GET", "/view") => (200, empty_view()),
                ("POST", "/drafts") => (201, draft_reply("8")),
                ("PATCH", "/drafts/8") => (200, draft_reply("8")),
                ("POST", "/drafts/8/seal") => (200, json!({ "outcome": { "Sealed": hex_of(digest(4)) } })),
                _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
            },
            |port| {
                run_on(
                    &Endpoint { host: "127.0.0.1".to_owned(), port, token: None },
                    &BloomCommand::Seal(SealArgs {
                        revisions: vec![
                            ("issue-A".to_owned(), DigestHex::from_bytes([0x11; 32])),
                            ("issue-B".to_owned(), DigestHex::from_bytes([0x22; 32])),
                        ],
                        ..seal_args(
                            task.clone(),
                            vec!["issue-A".to_owned(), "issue-B".to_owned(), "issue-C".to_owned()],
                        )
                    }),
                )
                .expect("seal --revision against the fake coordinator")
            },
        );
        fs::remove_file(&task).ok();
        let patch = find(&log, "PATCH", "/drafts/8").body.as_ref().expect("patch body");
        assert_eq!(patch["proposals"][0]["workpiece"], "issue-A");
        assert_eq!(patch["proposals"][0]["scope_revision"], rev_a);
        assert_eq!(patch["proposals"][0]["approval"]["subject"], rev_a);
        assert_eq!(patch["proposals"][1]["workpiece"], "issue-B");
        assert_eq!(patch["proposals"][1]["scope_revision"], rev_b);
        assert_eq!(patch["proposals"][1]["approval"]["subject"], rev_b);
        assert_eq!(patch["proposals"][2]["workpiece"], "issue-C");
        assert_eq!(patch["proposals"][2]["scope_revision"], rev_c);
        assert_eq!(patch["proposals"][2]["approval"]["subject"], rev_c);
    }

    #[test]
    fn seal_resolves_a_named_profile_through_the_config_route() {
        // Tripwire: `--profile opus-high` must be enough to seal. The client
        // authors the profile's kinds through POST /configs and patches the
        // returned digests — never a name, never a hand-threaded address.
        let override_digest = hex_of(digest(0xb1));
        let table_digest = hex_of(digest(0xb2));
        let override_for_reply = override_digest.clone();
        let table_for_reply = table_digest.clone();
        let task = temp_task("profile-seal", "seal from a named profile");
        let (output, log) = with_fake(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", path) if let Some(id) = commission_route(path) => (200, open_approved(id, digest(0xaa))),
                ("GET", "/view") => (200, empty_view()),
                ("POST", "/configs") => {
                    let body = request.body.as_ref().expect("config body");
                    let kind = body["kind"].as_str().expect("kind");
                    assert!(body["value"].is_object(), "profile value is authored JSON, not a digest: {body}");
                    match kind {
                        "aether.bloomery.model_override" => {
                            (200, json!({ "digest": override_for_reply, "kind": kind }))
                        }
                        "aether.bloomery.price_table" => (200, json!({ "digest": table_for_reply, "kind": kind })),
                        other => (400, json!({ "error": format!("unexpected kind {other}") })),
                    }
                }
                ("POST", "/drafts") => (201, draft_reply("9")),
                ("PATCH", "/drafts/9") => (200, draft_reply("9")),
                ("POST", "/drafts/9/seal") => (200, json!({ "outcome": { "Sealed": hex_of(digest(4)) } })),
                _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
            },
            |port| {
                run_on(
                    &Endpoint { host: "127.0.0.1".to_owned(), port, token: None },
                    &BloomCommand::Seal(SealArgs {
                        profile: Some("opus-high".to_owned()),
                        ..seal_args(task.clone(), vec!["wp-profile".to_owned()])
                    }),
                )
                .expect("seal --profile against the fake coordinator")
            },
        );
        fs::remove_file(&task).ok();

        let authored: Vec<&str> = log
            .iter()
            .filter(|entry| entry.method == "POST" && entry.path == "/configs")
            .map(|entry| entry.body.as_ref().expect("body")["kind"].as_str().expect("kind"))
            .collect();
        assert_eq!(
            authored,
            ["aether.bloomery.model_override", "aether.bloomery.price_table"],
            "the profile authors both kinds: {log:?}"
        );

        let patch = find(&log, "PATCH", "/drafts/9").body.as_ref().expect("patch body");
        assert_eq!(patch["configs"]["entries"]["aether.bloomery.model_override"], override_digest);
        assert_eq!(patch["configs"]["entries"]["aether.bloomery.price_table"], table_digest);
        assert!(output.contains("Sealed"), "outcome is printed: {output}");
    }

    fn cancel_args(workpiece: &str, reason: &str, seed_file: PathBuf) -> BloomCommand {
        BloomCommand::Cancel(CancelArgs {
            workpiece: workpiece.to_owned(),
            reason: reason.to_owned(),
            seed_file: Some(seed_file),
            signer: "operator".to_owned(),
        })
    }

    fn reopen_args(workpiece: &str, reason: &str, seed_file: PathBuf) -> BloomCommand {
        BloomCommand::Reopen(ReopenArgs {
            workpiece: workpiece.to_owned(),
            reason: reason.to_owned(),
            seed_file: Some(seed_file),
            signer: "operator".to_owned(),
        })
    }

    fn temp_seed(name: &str, bytes: [u8; 32]) -> PathBuf {
        let path = env::temp_dir().join(format!("aether-xtask-bloom-{name}-{}", process::id()));
        fs::write(&path, bytes).expect("write seed");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("set mode");
        }
        path
    }

    fn posted(log: &[Recorded]) -> Vec<&Recorded> {
        log.iter().filter(|entry| entry.method == "POST").collect()
    }

    #[test]
    fn cancel_reads_the_commission_then_posts_a_signed_cancel_over_its_intent() {
        let intent = digest(7);
        let seed = temp_seed("cancel-open", [3_u8; 32]);
        let reason = "the work landed on a sibling branch";
        let (output, log) = with_fake(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/commissions/wp-1") => (
                    200,
                    json!({
                        "id": "wp-1",
                        "intent": hex_of(intent),
                        "status": "open",
                        "approvals": []
                    }),
                ),
                ("POST", "/commissions/wp-1/cancel") => {
                    (200, json!({ "digest": hex_of(digest(9)), "id": "wp-1", "status": "cancelled" }))
                }
                _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
            },
            |port| {
                run_on(
                    &Endpoint { host: "127.0.0.1".to_owned(), port, token: None },
                    &cancel_args("wp-1", reason, seed.clone()),
                )
                .expect("cancel against the fake coordinator")
            },
        );
        fs::remove_file(&seed).ok();

        let cancel = find(&log, "POST", "/commissions/wp-1/cancel");
        let body = cancel.body.as_ref().expect("cancel body");
        assert_eq!(cancel.path, "/commissions/wp-1/cancel");
        assert_eq!(body["reason"], reason);
        assert_eq!(body["statement"]["words"], json!(intent.as_bytes().as_slice()));
        assert!(output.contains("wp-1"), "the workpiece is named: {output}");
        assert!(output.contains(&hex_of(digest(9))), "the stored statement digest is printed: {output}");
        assert!(output.contains("cancelled"), "the new status is printed: {output}");
    }

    #[test]
    fn cancel_refuses_a_commission_that_is_not_open_without_writing() {
        let seed = temp_seed("cancel-closed", [3_u8; 32]);
        let (error, log) = with_fake(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/commissions/wp-1") => (
                    200,
                    json!({
                        "id": "wp-1",
                        "intent": hex_of(digest(7)),
                        "status": "cancelled",
                        "approvals": []
                    }),
                ),
                _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
            },
            |port| {
                run_on(
                    &Endpoint { host: "127.0.0.1".to_owned(), port, token: None },
                    &cancel_args("wp-1", "landed elsewhere", seed.clone()),
                )
                .expect_err("a closed commission is refused")
            },
        );
        fs::remove_file(&seed).ok();
        assert!(error.to_string().contains("cancelled"), "the refusal names the status it found: {error}");
        assert!(posted(&log).is_empty(), "a second cancel must not write: {log:?}");
    }

    #[test]
    fn reopen_reads_the_commission_then_posts_a_signed_reopen_over_its_intent() {
        let intent = digest(7);
        let seed = temp_seed("reopen-landed", [3_u8; 32]);
        let reason = "withdrawn from the bloom that landed";
        let (output, log) = with_fake(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/commissions/wp-1") => (
                    200,
                    json!({
                        "id": "wp-1",
                        "intent": hex_of(intent),
                        "status": "landed",
                        "approvals": []
                    }),
                ),
                ("POST", "/commissions/wp-1/reopen") => {
                    (200, json!({ "digest": hex_of(digest(9)), "id": "wp-1", "status": "open" }))
                }
                _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
            },
            |port| {
                run_on(
                    &Endpoint { host: "127.0.0.1".to_owned(), port, token: None },
                    &reopen_args("wp-1", reason, seed.clone()),
                )
                .expect("reopen against the fake coordinator")
            },
        );
        fs::remove_file(&seed).ok();

        let reopen = find(&log, "POST", "/commissions/wp-1/reopen");
        let body = reopen.body.as_ref().expect("reopen body");
        assert_eq!(body["reason"], reason);
        assert_eq!(
            body["statement"]["words"],
            json!(intent.as_bytes().as_slice()),
            "the signature is bound to the commission's own intent"
        );
        assert!(output.contains("wp-1"), "the workpiece is named: {output}");
        assert!(output.contains("open"), "the restored status is printed: {output}");
    }

    #[test]
    fn reopen_refuses_a_commission_that_is_not_landed_without_writing() {
        // Read-first, exactly as the cancel is: an operator aiming at the wrong
        // status is told so before their key is asked for.
        let seed = temp_seed("reopen-open", [3_u8; 32]);
        let (error, log) = with_fake(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/commissions/wp-1") => (
                    200,
                    json!({
                        "id": "wp-1",
                        "intent": hex_of(digest(7)),
                        "status": "open",
                        "approvals": []
                    }),
                ),
                _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
            },
            |port| {
                run_on(
                    &Endpoint { host: "127.0.0.1".to_owned(), port, token: None },
                    &reopen_args("wp-1", "already in the line", seed.clone()),
                )
                .expect_err("an open commission is refused")
            },
        );
        fs::remove_file(&seed).ok();
        assert!(error.to_string().contains("open"), "the refusal names the status it found: {error}");
        assert!(posted(&log).is_empty(), "a refused reopen must not write: {log:?}");
    }

    #[test]
    fn cancel_refuses_an_unknown_workpiece_without_writing() {
        let seed = temp_seed("cancel-missing", [3_u8; 32]);
        let (error, log) = with_fake(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/commissions/wp-missing") => (404, json!({ "error": "no commission named wp-missing" })),
                _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
            },
            |port| {
                run_on(
                    &Endpoint { host: "127.0.0.1".to_owned(), port, token: None },
                    &cancel_args("wp-missing", "landed elsewhere", seed.clone()),
                )
                .expect_err("an unknown workpiece is refused")
            },
        );
        fs::remove_file(&seed).ok();
        assert!(error.to_string().contains("wp-missing"), "the refusal names the workpiece: {error}");
        assert!(posted(&log).is_empty(), "an unknown workpiece must not write: {log:?}");
    }

    // Tripwire: a signing seed another account on the host can read is a key
    // that can cancel any commission.
    #[cfg(unix)]
    #[test]
    fn cancel_refuses_a_group_readable_seed() {
        use std::os::unix::fs::PermissionsExt as _;

        let path = env::temp_dir().join(format!("aether-xtask-bloom-cancel-loose-{}", process::id()));
        fs::write(&path, [3_u8; 32]).expect("write seed");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("set mode");
        let (error, log) = with_fake(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/commissions/wp-1") => (
                    200,
                    json!({
                        "id": "wp-1",
                        "intent": hex_of(digest(7)),
                        "status": "open",
                        "approvals": []
                    }),
                ),
                _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
            },
            |port| {
                run_on(
                    &Endpoint { host: "127.0.0.1".to_owned(), port, token: None },
                    &cancel_args("wp-1", "landed elsewhere", path.clone()),
                )
                .expect_err("a loose seed is refused")
            },
        );
        fs::remove_file(&path).ok();
        assert!(error.to_string().contains("0600"), "the refusal names the fix: {error}");
        assert!(posted(&log).is_empty(), "a loose seed must not write: {log:?}");
    }

    fn view_with_member(bloom_id: &str, workpiece: &str) -> Value {
        live_view(Digest::from_hex(bloom_id).expect("id"), workpiece, digest(7))
    }

    fn repair_args(bloom_id: &str, workpiece: &str) -> RepairArgs {
        RepairArgs {
            bloom_id: bloom_id.to_owned(),
            workpiece: workpiece.to_owned(),
            candidate: None,
            from_commit: None,
            from_worktree: None,
            reason: "the lane could not produce a tree that builds".to_owned(),
            operator: "operator".to_owned(),
        }
    }

    #[test]
    fn repair_body_takes_exactly_one_source() {
        // Tripwire: the route answers 400 to zero or two sources, so a body
        // built without this check spends a round trip and a stale board read
        // to learn what the operator's own argv already said.
        let bloom_id = hex_of(digest(0xab));
        let none = repair_body(&repair_args(&bloom_id, "wp-1")).expect_err("no source");
        assert!(none.to_string().contains("exactly one"), "the refusal states the rule: {none}");

        let two = repair_body(&RepairArgs {
            from_commit: Some("abc123".to_owned()),
            from_worktree: Some("/tmp/wt".to_owned()),
            ..repair_args(&bloom_id, "wp-1")
        })
        .expect_err("two sources");
        assert!(two.to_string().contains("2 were given"), "the refusal counts what it found: {two}");

        let one = repair_body(&RepairArgs { from_commit: Some("abc123".to_owned()), ..repair_args(&bloom_id, "wp-1") })
            .expect("one source");
        assert_eq!(one.from_commit.as_deref(), Some("abc123"));
        assert!(one.candidate.is_none() && one.from_worktree.is_none());

        let blank =
            repair_body(&RepairArgs { reason: "   ".to_owned(), ..repair_args(&bloom_id, "wp-1") }).expect_err("blank");
        assert!(blank.to_string().contains("reason"), "a blank reason is refused before the source: {blank}");
    }

    fn propose_args() -> ProposeArgs {
        ProposeArgs {
            from_commit: None,
            from_worktree: None,
            reason: "flip an ADR status".to_owned(),
            operator: "operator".to_owned(),
            seed_file: None,
            signer: "operator".to_owned(),
        }
    }

    #[test]
    fn propose_needs_exactly_one_source() {
        let ((none, two, blank), log) = with_fake(
            |_| (404, json!({ "error": "unexpected" })),
            |port| {
                let endpoint = Endpoint { host: "127.0.0.1".to_owned(), port, token: None };
                let none = run_on(&endpoint, &BloomCommand::Propose(propose_args())).expect_err("no source");
                let two = run_on(
                    &endpoint,
                    &BloomCommand::Propose(ProposeArgs {
                        from_commit: Some("abc123".to_owned()),
                        from_worktree: Some(PathBuf::from("/tmp/wt")),
                        ..propose_args()
                    }),
                )
                .expect_err("two sources");
                let blank = run_on(
                    &endpoint,
                    &BloomCommand::Propose(ProposeArgs { reason: "   ".to_owned(), ..propose_args() }),
                )
                .expect_err("blank");
                (none, two, blank)
            },
        );
        assert!(none.to_string().contains("exactly one"), "the refusal states the rule: {none}");
        assert!(two.to_string().contains("2 were given"), "the refusal counts what it found: {two}");
        assert!(blank.to_string().contains("reason"), "a blank reason is refused before the source: {blank}");
        assert!(posted(&log).is_empty(), "a refused propose must not write: {log:?}");
    }

    #[test]
    fn repair_reads_the_member_then_posts_only_the_source_it_was_given() {
        // The unset sources stay off the wire entirely. A `null` sent for the
        // two the operator did not name reads as a third spelling the moment
        // the route counts keys rather than `Option`s.
        let bloom_id = hex_of(digest(0xab));
        let bloom_for_view = bloom_id.clone();
        let (output, log) = with_fake(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/view") => (200, view_with_member(&bloom_for_view, "wp-1")),
                ("POST", path) if path.ends_with("/members/wp-1/repair") => (200, json!({ "outcome": "Duplicate" })),
                _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
            },
            |port| {
                run_on(
                    &Endpoint { host: "127.0.0.1".to_owned(), port, token: None },
                    &BloomCommand::Repair(RepairArgs {
                        from_worktree: Some("/srv/lanes/wp-1".to_owned()),
                        ..repair_args(&bloom_id, "wp-1")
                    }),
                )
                .expect("repair against the fake coordinator")
            },
        );

        let body = find(&log, "POST", "/members/wp-1/repair").body.as_ref().expect("repair body").clone();
        assert_eq!(body["from_worktree"], "/srv/lanes/wp-1");
        assert!(body.get("candidate").is_none(), "an unnamed source is absent, not null: {body}");
        assert!(body.get("from_commit").is_none(), "an unnamed source is absent, not null: {body}");
        assert_eq!(body["operator"], "operator");
        assert!(output.contains("Duplicate"), "the outcome is printed: {output}");
    }

    #[test]
    fn repair_refuses_an_unknown_member_without_writing() {
        let bloom_id = hex_of(digest(0xab));
        let bloom_for_view = bloom_id.clone();
        let (error, log) = with_fake(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/view") => (200, view_with_member(&bloom_for_view, "wp-1")),
                _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
            },
            |port| {
                run_on(
                    &Endpoint { host: "127.0.0.1".to_owned(), port, token: None },
                    &BloomCommand::Repair(RepairArgs {
                        from_commit: Some("abc123".to_owned()),
                        ..repair_args(&bloom_id, "wp-typo")
                    }),
                )
                .expect_err("an unknown member is refused")
            },
        );
        assert!(error.to_string().contains("wp-typo"), "the refusal names the member: {error}");
        assert!(posted(&log).is_empty(), "a mistyped member must not journal an override: {log:?}");
    }

    #[test]
    fn suppression_posts_every_named_request_under_one_verdict() {
        // The digests are what the answer closes, so they must reach the wire
        // in the spelling the route decodes and in the order given. Dropping
        // one silently leaves a request standing that the reviewer believes
        // they answered.
        let bloom_id = hex_of(digest(0xab));
        let bloom_for_view = bloom_id.clone();
        let first = DigestHex::from_bytes([0x11; 32]);
        let second = DigestHex::from_bytes([0x22; 32]);
        let (output, log) = with_fake(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/view") => (200, view_with_member(&bloom_for_view, "wp-1")),
                ("POST", path) if path.ends_with("/members/wp-1/suppression") => {
                    (200, json!({ "outcome": "Duplicate" }))
                }
                _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
            },
            |port| {
                run_on(
                    &Endpoint { host: "127.0.0.1".to_owned(), port, token: None },
                    &BloomCommand::Suppression(SuppressionArgs {
                        bloom_id: bloom_id.clone(),
                        workpiece: "wp-1".to_owned(),
                        requests: vec![first, second],
                        verdict: dto::SuppressionVerdictArg::Denied,
                        reason: "the allow hides the bug the gate caught".to_owned(),
                        operator: "reviewer".to_owned(),
                    }),
                )
                .expect("suppression against the fake coordinator")
            },
        );

        let body = find(&log, "POST", "/members/wp-1/suppression").body.as_ref().expect("suppression body").clone();
        assert_eq!(body["requests"], json!([first.as_hex(), second.as_hex()]));
        assert_eq!(body["verdict"], "Denied", "the verdict is the wire spelling the route decodes");
        assert_eq!(body["operator"], "reviewer");
        assert!(output.contains("Duplicate"), "the outcome is printed: {output}");
    }

    #[test]
    fn suppression_refuses_an_answer_that_closes_nothing_without_writing() {
        // The route refuses an empty set for the reason it exists: there is no
        // "everything standing" spelling, so an empty one would either answer
        // nothing or answer a request the reviewer never read.
        let bloom_id = hex_of(digest(0xab));
        let bloom_for_view = bloom_id.clone();
        let (error, log) = with_fake(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/view") => (200, view_with_member(&bloom_for_view, "wp-1")),
                _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
            },
            |port| {
                run_on(
                    &Endpoint { host: "127.0.0.1".to_owned(), port, token: None },
                    &BloomCommand::Suppression(SuppressionArgs {
                        bloom_id: bloom_id.clone(),
                        workpiece: "wp-1".to_owned(),
                        requests: Vec::new(),
                        verdict: dto::SuppressionVerdictArg::Granted,
                        reason: "looks fine".to_owned(),
                        operator: "reviewer".to_owned(),
                    }),
                )
                .expect_err("an empty answer is refused")
            },
        );
        assert!(error.to_string().contains("--request"), "the refusal names the flag: {error}");
        assert!(posted(&log).is_empty(), "an empty answer must not write: {log:?}");
    }
}
