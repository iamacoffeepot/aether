//! `cargo xtask bloom admin` — the operator's repair window (ADR-0219).
//!
//! Every other operator verb in this client is one decision the coordinator
//! acts on the moment it lands. These seven are a *session*: `enter` takes the
//! bloom out of the machine's hands, five acts repair it, and `exit` hands it
//! back and lets the ordinary gates judge what is now there.
//!
//! The commands are read-first, the discipline every member verb here follows:
//! a mistyped workpiece or a stale nonce is an operator's typo, and the
//! coordinator's own refusal for one arrives after the act has already been
//! composed and sent. So `cancel-lane` and `drop-lap` resolve their nonce
//! against the live view's outstanding orders before they write, and `waive
//! --all` reads the findings it is about to void rather than asking the
//! coordinator to guess which ones the operator meant.

mod status;

#[cfg(test)]
mod tests;

use aether_bloomery::{AdminWaiveRequest, StageId};
use anyhow::{Result, bail};
use clap::{Args, Subcommand};

use super::client::{Client, bloom_in};
use super::dto::{
    AdminCancelLaneRequest, AdminDropLapRequest, AdminRerunRequest, AdminSessionRequest, AdminSetCandidateRequest,
    BloomView, DigestHex, LiveOrderView,
};
use super::{plan, render_outcome};

/// Drive one bloom's admin session.
#[derive(Args, Debug)]
pub struct AdminArgs {
    #[command(subcommand)]
    command: AdminCommand,
}

#[derive(Subcommand, Debug)]
enum AdminCommand {
    /// Take a bloom out of the machine's hands: nothing dispatches, an executor
    /// fault costs nothing, and the acts below become admissible.
    Enter(SessionArgs),
    /// Hand the bloom back. Dispatches what the cursors now owe, and completes
    /// the landing a waived gate left nothing to finish.
    Exit(SessionArgs),
    /// Cancel one running dispatch. No attempt, repair roll, or machinery roll
    /// is spent — the lap is a lap that never ran.
    CancelLane(LaneArgs),
    /// Hand a workpiece a candidate and let the ordinary gates judge it on
    /// exit. The repair verb without its wedged precondition.
    SetCandidate(SetCandidateArgs),
    /// Run one stage again on the candidate the workpiece already holds.
    Rerun(RerunArgs),
    /// Void a red verdict's findings so the gate counts as passed for landing.
    Waive(WaiveArgs),
    /// Discard a completed lap's captured candidate. The cursor reverts to the
    /// one before it; the evidence stays on record.
    DropLap(LaneArgs),
    /// What admin can see: cursors, running lanes and their nonces, the held
    /// fold, the red verdicts and their evidence digests, and the acts this
    /// session has already journaled.
    Status(StatusArgs),
}

/// The two session edges: a bloom, a reason, and who is acting.
#[derive(Args, Debug)]
struct SessionArgs {
    /// The bloom (64 hex characters).
    #[arg(value_parser = plan::parse_bloom_id)]
    bloom_id: String,

    /// Why, in your own words. Required; a blank one is refused at the door.
    #[arg(long)]
    reason: String,

    /// Who is deciding. Recorded as the decider.
    #[arg(long, default_value = "operator")]
    operator: String,
}

/// The two nonce-addressed acts: cancel a running lane, or drop a finished
/// lap's candidate.
#[derive(Args, Debug)]
struct LaneArgs {
    /// The bloom (64 hex characters).
    #[arg(value_parser = plan::parse_bloom_id)]
    bloom_id: String,

    /// The host dispatch nonce, as `xtask bloom admin status` reports it.
    nonce: String,

    /// The workpiece the nonce belongs to. Resolved off the live view's
    /// outstanding orders when omitted; name it when the board no longer lists
    /// the nonce — a lane that already died is still an order to settle, and
    /// refusing to name it would leave the operator unable to clear it.
    #[arg(long)]
    workpiece: Option<String>,

    /// Why, in your own words. Required; a blank one is refused at the door.
    #[arg(long)]
    reason: String,

    /// Who is deciding. Recorded as the decider.
    #[arg(long, default_value = "operator")]
    operator: String,
}

#[derive(Args, Debug)]
struct SetCandidateArgs {
    /// The bloom (64 hex characters).
    #[arg(value_parser = plan::parse_bloom_id)]
    bloom_id: String,

    /// The member, or the reserved composition id.
    workpiece: String,

    /// The candidate you already pushed, as `tree:checkout` — two 64-hex
    /// digests. Name exactly one of this, `--from-commit`, or
    /// `--from-worktree`.
    #[arg(long, value_parser = plan::parse_candidate_flag)]
    candidate: Option<super::dto::CandidateRef>,

    /// A commit the coordinator's repository can already reach. It derives the
    /// candidate, pushes the ref, and records correspondence for you.
    #[arg(long)]
    from_commit: Option<String>,

    /// A worktree whose `HEAD` the coordinator's repository can already see.
    #[arg(long)]
    from_worktree: Option<String>,

    /// Why, in your own words. Required; a blank one is refused at the door.
    #[arg(long)]
    reason: String,

    /// Who is deciding. Recorded as the decider.
    #[arg(long, default_value = "operator")]
    operator: String,
}

#[derive(Args, Debug)]
struct RerunArgs {
    /// The bloom (64 hex characters).
    #[arg(value_parser = plan::parse_bloom_id)]
    bloom_id: String,

    /// The member, or the reserved composition id.
    workpiece: String,

    /// The stage to run. A member runs `verify`, `review`, or `reconcile`; the
    /// composition runs `aggregate-verify` or `aggregate-review`.
    #[arg(long, value_parser = parse_stage)]
    stage: StageId,

    /// Dispatch at once rather than when the session closes.
    #[arg(long)]
    now: bool,

    /// Why, in your own words. Required; a blank one is refused at the door.
    #[arg(long)]
    reason: String,

    /// Who is deciding. Recorded as the decider.
    #[arg(long, default_value = "operator")]
    operator: String,
}

#[derive(Args, Debug)]
struct WaiveArgs {
    /// The bloom (64 hex characters).
    #[arg(value_parser = plan::parse_bloom_id)]
    bloom_id: String,

    /// One verdict finding digest to void (64 hex characters). Repeatable.
    /// Name these or `--all`, never both.
    #[arg(long = "finding", value_parser = plan::parse_digest_flag)]
    findings: Vec<DigestHex>,

    /// Void every finding the bloom currently has open, read off the live view
    /// and listed back before the write.
    #[arg(long)]
    all: bool,

    /// The gate whose verdict is being voided.
    #[arg(long, default_value = "aggregate-review", value_parser = parse_stage)]
    gate: StageId,

    /// Acknowledge that waiving a mechanical gate lands code no gate proved.
    /// Required for a verify waiver; a review waiver needs only the reason.
    #[arg(long = "i-know-this-lands-unverified-code")]
    acknowledged_unverified: bool,

    /// Why, in your own words. Required; a blank one is refused at the door.
    #[arg(long)]
    reason: String,

    /// Who is deciding. Recorded as the decider.
    #[arg(long, default_value = "operator")]
    operator: String,
}

#[derive(Args, Debug)]
struct StatusArgs {
    /// The bloom (64 hex characters).
    #[arg(value_parser = plan::parse_bloom_id)]
    bloom_id: String,
}

/// One stage, spelled the way the rest of this client spells one: lowercase
/// with a dash between words.
///
/// Parsed against [`StageId::ALL`] rather than a hand-written table, so a stage
/// the vocabulary gains is spellable here without an edit — and an unknown one
/// is refused with the whole list rather than with a bare "invalid value".
fn parse_stage(value: &str) -> Result<StageId, String> {
    StageId::ALL
        .iter()
        .copied()
        .find(|stage| stage_name(*stage) == value)
        .ok_or_else(|| format!("unknown stage `{value}`; one of {}", stage_names().join(", ")))
}

fn stage_name(stage: StageId) -> String {
    format!("{stage:?}")
        .chars()
        .enumerate()
        .flat_map(|(index, letter)| {
            let dash = (index > 0 && letter.is_uppercase()).then_some('-');
            dash.into_iter().chain(letter.to_lowercase())
        })
        .collect()
}

fn stage_names() -> Vec<String> {
    StageId::ALL.iter().copied().map(stage_name).collect()
}

pub fn run(client: &Client<'_>, args: &AdminArgs) -> Result<String> {
    match &args.command {
        AdminCommand::Enter(args) => run_session(client, args, "enter"),
        AdminCommand::Exit(args) => run_session(client, args, "exit"),
        AdminCommand::CancelLane(args) => run_cancel_lane(client, args),
        AdminCommand::SetCandidate(args) => run_set_candidate(client, args),
        AdminCommand::Rerun(args) => run_rerun(client, args),
        AdminCommand::Waive(args) => run_waive(client, args),
        AdminCommand::DropLap(args) => run_drop_lap(client, args),
        AdminCommand::Status(args) => status::render(client, &args.bloom_id),
    }
}

fn run_session(client: &Client<'_>, args: &SessionArgs, edge: &str) -> Result<String> {
    let request =
        AdminSessionRequest { reason: args.reason.clone(), operator: args.operator.clone(), idempotency_key: None };
    Ok(render_outcome(&client.admin_session(&args.bloom_id, edge, &request)?.outcome))
}

fn run_cancel_lane(client: &Client<'_>, args: &LaneArgs) -> Result<String> {
    let request = AdminCancelLaneRequest {
        workpiece: lane_owner(client, args)?,
        nonce: args.nonce.clone(),
        reason: args.reason.clone(),
        operator: args.operator.clone(),
        idempotency_key: None,
    };
    Ok(render_outcome(&client.admin_cancel_lane(&args.bloom_id, &request)?.outcome))
}

fn run_drop_lap(client: &Client<'_>, args: &LaneArgs) -> Result<String> {
    let request = AdminDropLapRequest {
        workpiece: lane_owner(client, args)?,
        nonce: args.nonce.clone(),
        reason: args.reason.clone(),
        operator: args.operator.clone(),
        idempotency_key: None,
    };
    Ok(render_outcome(&client.admin_drop_lap(&args.bloom_id, &request)?.outcome))
}

/// The workpiece the named nonce belongs to: the operator's own `--workpiece`
/// when they gave one, and otherwise the live view's outstanding orders.
///
/// The nonce alone would be enough for the coordinator — the executor resolves
/// it against its own registry and refuses one whose order names something
/// else. Resolving it here as well is what turns "the coordinator refused your
/// act" into "no live order in this bloom is called that", which is the answer
/// an operator who mistyped a nonce off the board actually needs.
///
/// The override is what keeps that from becoming a trap. A lane can be gone
/// from the board and still hold an order the operator wants settled — a model
/// process that died without writing its evidence leaves exactly that — so
/// naming the workpiece is always available, and the act still costs nobody
/// anything whether the order is there or not.
fn lane_owner(client: &Client<'_>, args: &LaneArgs) -> Result<super::dto::WorkpieceId> {
    if let Some(named) = &args.workpiece {
        require_workpiece(client, &args.bloom_id, named)?;
        return Ok(super::dto::WorkpieceId(named.clone()));
    }
    let (view, orders) = client.live_view()?;
    bloom_in(&view, &args.bloom_id)?;
    let Some(order) = bloom_orders(&orders, &args.bloom_id).into_iter().find(|order| order.nonce == args.nonce) else {
        bail!(
            "bloom {} holds no live order under nonce {}; name --workpiece to settle a lane that is already gone",
            args.bloom_id,
            args.nonce
        );
    };
    if order.workpiece.is_empty() {
        bail!("nonce {} names a bloom-level order, which belongs to no workpiece", args.nonce);
    }
    Ok(super::dto::WorkpieceId(order.workpiece.clone()))
}

fn run_set_candidate(client: &Client<'_>, args: &SetCandidateArgs) -> Result<String> {
    require_workpiece(client, &args.bloom_id, &args.workpiece)?;
    let named = [args.candidate.is_some(), args.from_commit.is_some(), args.from_worktree.is_some()]
        .into_iter()
        .filter(|named| *named)
        .count();
    if named != 1 {
        bail!("set-candidate needs exactly one of --candidate, --from-commit, or --from-worktree; {named} were given");
    }

    let request = AdminSetCandidateRequest {
        workpiece: super::dto::WorkpieceId(args.workpiece.clone()),
        candidate: args.candidate,
        from_commit: args.from_commit.clone(),
        from_worktree: args.from_worktree.clone(),
        reason: args.reason.clone(),
        operator: args.operator.clone(),
        idempotency_key: None,
    };
    Ok(render_outcome(&client.admin_set_candidate(&args.bloom_id, &request)?.outcome))
}

fn run_rerun(client: &Client<'_>, args: &RerunArgs) -> Result<String> {
    require_workpiece(client, &args.bloom_id, &args.workpiece)?;

    let request = AdminRerunRequest {
        workpiece: super::dto::WorkpieceId(args.workpiece.clone()),
        stage: args.stage,
        now: args.now,
        reason: args.reason.clone(),
        operator: args.operator.clone(),
        idempotency_key: None,
    };
    Ok(render_outcome(&client.admin_rerun(&args.bloom_id, &request)?.outcome))
}

fn run_waive(client: &Client<'_>, args: &WaiveArgs) -> Result<String> {
    if args.all != args.findings.is_empty() {
        bail!("waive needs either --finding (repeatable) or --all, never both and never neither");
    }
    let view = client.view()?;
    let bloom = bloom_in(&view, &args.bloom_id)?;
    let findings = if args.all {
        open_findings(bloom)
    } else {
        args.findings.iter().map(|finding| finding.digest()).collect()
    };
    if findings.is_empty() {
        bail!("bloom {} has no open findings to void", args.bloom_id);
    }

    let request = AdminWaiveRequest {
        gate: args.gate,
        findings,
        acknowledged_unverified: args.acknowledged_unverified,
        reason: args.reason.clone(),
        operator: args.operator.clone(),
        idempotency_key: None,
    };
    Ok(render_outcome(&client.admin_waive(&args.bloom_id, &request)?.outcome))
}

/// Every verdict digest this bloom currently has open — the composition's own
/// findings plus, when the bloom parked on its review, the park's question.
///
/// Both channels, because an operator reading a parked bloom sees one number
/// and a waiver that voided only half of it would leave the bloom exactly as
/// stuck as it was.
pub(super) fn open_findings(bloom: &BloomView) -> Vec<aether_bloomery::Digest> {
    bloom
        .composition
        .iter()
        .flat_map(|composition| composition.findings.iter().map(|finding| finding.detail))
        .chain(bloom.review_park.iter().map(|park| park.question))
        .collect()
}

/// Refuse a bloom or a workpiece the live view does not carry, before any
/// write.
fn require_workpiece(client: &Client<'_>, bloom_id: &str, workpiece: &str) -> Result<()> {
    let view = client.view()?;
    if !addresses_workpiece(bloom_in(&view, bloom_id)?, workpiece) {
        bail!("bloom {bloom_id} addresses no workpiece {workpiece}");
    }
    Ok(())
}

/// Whether `workpiece` names something this bloom can be acted on through.
///
/// The reserved composition id is checked *first* and never against the
/// membership list, because the composition is in no bloom's membership — it is
/// the weave every member folds into. A read-first check that walked only the
/// members would refuse the one workpiece an operator repairing a red aggregate
/// review is always aiming at, which is what the sibling `repair` and `retry`
/// verbs do today despite their help text saying otherwise.
pub(super) fn addresses_workpiece(bloom: &BloomView, workpiece: &str) -> bool {
    super::dto::WorkpieceId(workpiece.to_owned()).is_composition()
        || bloom.members.iter().any(|member| member.workpiece.0 == workpiece)
}

/// The live orders this bloom currently holds, newest nonce order — shared by
/// [`status`] and, through [`lane_owner`], by the two nonce-addressed acts.
pub(super) fn bloom_orders<'a>(orders: &'a [LiveOrderView], bloom_id: &str) -> Vec<&'a LiveOrderView> {
    orders.iter().filter(|order| order.bloom.to_string() == bloom_id).collect()
}
