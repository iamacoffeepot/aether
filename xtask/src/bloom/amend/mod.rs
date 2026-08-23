//! `cargo xtask bloom amend` — answer a parked member's surface request
//! (ADR-0207).
//!
//! A member's declared surface is a field of an immutable, content-addressed
//! `ScopeRevision`, and the approval statement's signed words *are* that
//! revision's digest. So there is no widening in place: giving a parked member
//! one more path means writing a successor revision, approving it, and
//! re-sealing the membership. Those three doors already exist; nothing composed
//! them, so the operator drove four commands and hand-authored a file whose
//! only delta was one glob.
//!
//! This is not a convenience wrapper. It is the one place the tier ladder is
//! applied to the *delta*, and the only point in the chain that can refuse
//! before a signature exists — everything downstream of a signature treats the
//! signature as the decision, and the seal door verifies only that the signer
//! is allowlisted, not that the tier permitted them.
//!
//! Preflight is entirely reads. The one irreversible step is the revision
//! write, which advances the commission's tip and leaves the member unsealable
//! until an approval lands against the new tip; it is announced before it
//! happens, and re-running the identical command converges rather than
//! compounding.

mod request;
mod revision;
mod surface;
mod tier;

#[cfg(test)]
mod tests;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use aether_bloomery::{
    ApprovalPolicy, KeyId, ScopeRevision, Tier, TierVerdict, digest_of, gate_widening, surface_intersection,
    tier_verdict,
};
use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};

use super::client::{Client, bloom_in};
use super::dto::{BloomSpec, BloomView, CommissionShowView, DigestHex, MemberView, ScopeRevisionView};
use super::{ProjectionArgs, plan, render_outcome};

use request::Requested;
use tier::PolicySource;

pub(super) use revision::OperatorKey;

/// `Tier`, as a CLI value.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum TierArg {
    Auto,
    Judge,
    Human,
}

impl TierArg {
    const fn tier(self) -> Tier {
        match self {
            Self::Auto => Tier::Auto,
            Self::Judge => Tier::Judge,
            Self::Human => Tier::Human,
        }
    }
}

#[derive(Args, Debug)]
pub struct AmendArgs {
    /// Predecessor bloom id (64 hex characters).
    #[arg(value_parser = plan::parse_bloom_id)]
    bloom_id: String,

    /// The parked member whose surface request is being answered.
    #[arg(long)]
    workpiece: String,

    /// Extra path to union in, beyond the journaled request. Repeatable.
    /// Required only when the member carries no request.
    #[arg(long = "path")]
    paths: Vec<String>,

    /// Highest tier the amendment may grant unattended.
    #[arg(long = "accept-tier", value_enum, default_value = "auto")]
    accept_tier: TierArg,

    /// Operator signing seed: 32 raw bytes or 64 hex characters, mode 0600.
    #[arg(long)]
    seed_file: Option<PathBuf>,

    /// The `KeyId` the coordinator's allowlist names for that seed.
    #[arg(long, default_value = "operator")]
    signer: String,

    /// Successor work-order description, the same requirement `supersede` has.
    #[arg(long)]
    task_file: PathBuf,

    /// Successor base: `observed` (default), `mainline`, or a 64-hex digest.
    #[arg(long, default_value = "observed", value_parser = plan::BaseChoice::parse)]
    base: plan::BaseChoice,

    /// Print the plan and the tier verdict; mutate nothing.
    #[arg(long)]
    dry_run: bool,

    #[command(flatten)]
    projection: ProjectionArgs,
}

/// Everything the preflight proved, so the mutating half never re-decides.
struct AmendPlan {
    workpiece: String,
    spec: BloomSpec,
    /// Predecessor members the day withdrew. They never integrate and their
    /// commissions are free to move on, so the successor leaves them out
    /// instead of sealing them again at a revision they have already left.
    withdrawn: Vec<String>,
    /// The commission tip, which the successor seals against when the widened
    /// surface turns out to be what the tip already declares.
    tip: DigestHex,
    current: ScopeRevisionView,
    commission: CommissionShowView,
    requested: Requested,
    /// Requested paths the policy does not name, and the glob each is admitted
    /// as, so the plan shows the ask beside the grant.
    coarsened: Vec<(String, String)>,
    /// The same rewrite over entries the current revision already carries.
    inherited: Vec<(String, String)>,
    added: Vec<String>,
    widened: Vec<String>,
    verdict: TierVerdict,
    source: PolicySource,
    /// Sibling members whose declared surface the widening now overlaps, with
    /// the globs both permit. Advisory: the seal journals the overlap and may
    /// derive a dependency edge from it.
    new_overlaps: Vec<(String, Vec<String>)>,
}

impl AmendPlan {
    /// The successor revision: the tip's bytes with the declared surface
    /// replaced.
    ///
    /// Replaced rather than appended, because widening an entry the tip already
    /// carries has to drop the file-granular spelling — appending the covering
    /// glob beside it would leave the entry the seal door refuses on.
    fn widened_revision(&self) -> ScopeRevision {
        let current = self.current.to_revision();
        ScopeRevision { predecessor: Some(digest_of(&current)), declared_surface: self.widened.clone(), ..current }
    }
}

pub fn run(client: &Client<'_>, args: &AmendArgs, policy_file: &Path) -> Result<String> {
    let plan = preflight(client, args, policy_file)?;

    let mut out = describe(&plan, args.accept_tier.tier());
    if args.dry_run {
        out.push_str("\ndry run: nothing was written\n");
        return Ok(out);
    }

    let key = OperatorKey::load(
        KeyId(args.signer.clone()),
        args.seed_file.as_deref().context("--seed-file is required to sign the amendment's approval")?,
    )?;

    // A re-run whose ask the tip already declares still has to finish: the
    // member is parked until a successor seals it, and there is nothing left to
    // write, so the tip itself is what gets approved and sealed.
    let scope = if plan.widened == plan.current.declared_surface {
        out.push_str("the tip already declares this surface; approving and sealing it as it stands\n");
        plan.tip
    } else {
        out.push_str("writing the widened revision; the commission tip advances here\n");
        revision::write_widened(client, &plan.workpiece, &plan.widened_revision())?
    };
    let _ = writeln!(out, "revision   {scope}");

    if revision::approve(client, &plan.commission, &plan.workpiece, scope, &key)? {
        let _ = writeln!(out, "approved   by {}", key.signer.0);
    } else {
        out.push_str("approved   already stored\n");
    }

    out.push_str(&supersede(client, args, &plan, scope, policy_file)?);
    Ok(out)
}

/// P1 – P8. Entirely reads, and the last point at which a refusal is free:
/// past here the commission tip has advanced and the operator's key has signed,
/// so the same refusal costs both. Every refusal the successor seal would make
/// is therefore made here, in the seal door's own words.
fn preflight(client: &Client<'_>, args: &AmendArgs, policy_file: &Path) -> Result<AmendPlan> {
    let view = client.view()?;
    let bloom = bloom_in(&view, &args.bloom_id)?;
    let member = request::member(bloom, &args.workpiece)?;
    if member.withdrawn.is_some() {
        bail!(
            "member {} was withdrawn from bloom {}; re-scope it into a later bloom rather than amend it here",
            args.workpiece,
            args.bloom_id,
        );
    }
    request::binding_holds(member)?;
    let requested = request::collect(member, &args.paths)?;

    let commission = client.commission(&args.workpiece)?;
    let current = commission
        .current
        .clone()
        .with_context(|| format!("commission {} has no current scope revision", args.workpiece))?;
    let tip = commission
        .current_revision
        .with_context(|| format!("commission {} names no current revision digest", args.workpiece))?;

    let spec = client.spec_for(&args.bloom_id)?;
    let (policy, source) = tier::resolve_policy(client, &spec, policy_file)?;
    let widening = surface::widen(&policy, &current.declared_surface, &requested.globs)
        .map_err(|glob| anyhow::anyhow!("requested path `{glob}` is outside the declared-surface grammar"))?;

    if tip_standing(tip, member.scope_revision, &current.declared_surface, &widening.widened) == TipStanding::Moved {
        bail!(
            "commission {} is at revision {tip} but the bloom sealed member {} at {}; a human moved the scope — \
             re-scope and supersede rather than amend",
            args.workpiece,
            args.workpiece,
            member.scope_revision,
        );
    }
    siblings_are_sealable(client, bloom, &args.workpiece)?;

    let verdict = tier_verdict(&policy, &widening.existing, &widening.added);
    if let Err(offending) = gate_widening(&verdict, args.accept_tier.tier()) {
        bail!(
            "{}\nthe amendment is refused: {} above --accept-tier {:?}. The member stays parked.",
            tier::render(&verdict, &source, args.accept_tier.tier()),
            offending.iter().map(|(glob, tier)| format!("`{glob}` resolves {tier:?}")).collect::<Vec<_>>().join(", "),
            args.accept_tier.tier(),
        );
    }

    granularity_holds(&policy, &args.workpiece, &widening.widened)?;
    let new_overlaps = overlaps(client, bloom, &args.workpiece, &widening.widened)?;

    Ok(AmendPlan {
        workpiece: args.workpiece.clone(),
        spec,
        withdrawn: withdrawn(bloom),
        tip,
        current,
        commission,
        requested,
        coarsened: widening.coarsened,
        inherited: widening.inherited,
        added: widening.added,
        widened: widening.widened,
        verdict,
        source,
        new_overlaps,
    })
}

/// P7: the granularity the seal door admits, in the door's own words.
///
/// Coarsening leaves nothing for this to catch on a path that has a tree to
/// widen to, so what it actually guards is the path that has none — a
/// repository-root file the policy does not name. Asked here, the operator
/// learns it while the member is still parked; asked at the seal, they learn it
/// with the tip already advanced and the signature already spent.
fn granularity_holds(policy: &ApprovalPolicy, workpiece: &str, surface: &[String]) -> Result<()> {
    if let Some(glob) = policy.unnamed_file_entries(surface).first() {
        bail!(
            "member {workpiece} declared surface {glob:?} names one file and no approval-policy rule names that \
             file; widen it to a crate glob such as crates/<crate>/src/**",
        );
    }
    Ok(())
}

/// The predecessor members the day withdrew.
fn withdrawn(bloom: &BloomView) -> Vec<String> {
    bloom.members.iter().filter(|member| member.withdrawn.is_some()).map(|member| member.workpiece.clone()).collect()
}

/// P4: every sibling must be at its own commission tip and carry an approval.
///
/// Checked before anything is signed, because the successor seal refuses on a
/// stale sibling scope or a missing sibling approval — and by then the
/// operator's key has already signed and the commission tip has already moved.
fn siblings_are_sealable(client: &Client<'_>, bloom: &BloomView, amended: &str) -> Result<()> {
    for member in siblings(bloom, amended) {
        sibling_is_sealable(member, &client.commission(&member.workpiece)?)?;
    }
    Ok(())
}

/// The members the successor still carries beside `amended`.
///
/// A withdrawn member left the line before integration, so the successor does
/// not seal it and its commission is free to have been re-scoped into a later
/// bloom. Reasoning over one refuses the amendment on a revision no seal will
/// ever read.
fn siblings<'a>(bloom: &'a BloomView, amended: &'a str) -> impl Iterator<Item = &'a MemberView> {
    bloom.members.iter().filter(move |member| member.workpiece != amended && member.withdrawn.is_none())
}

/// One sibling's half of P4, over the commission the edge returned.
fn sibling_is_sealable(member: &MemberView, sibling: &CommissionShowView) -> Result<()> {
    match sibling.current_revision {
        Some(tip) if tip == member.scope_revision => {}
        Some(tip) => bail!(
            "sibling {} is sealed at {} but its commission is at {tip}; the successor seal would refuse as stale",
            member.workpiece,
            member.scope_revision,
        ),
        None => bail!("sibling {} has no current scope revision", member.workpiece),
    }
    if sibling.approvals.is_empty() {
        bail!("sibling {} carries no stored approval; the successor seal would refuse it", member.workpiece);
    }
    Ok(())
}

/// Where the commission's tip stands relative to the revision the bloom sealed
/// the member at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TipStanding {
    /// The tip is the revision the bloom sealed the member at.
    Sealed,
    /// The tip is past the sealed revision and already declares exactly the
    /// surface this amendment would write.
    AlreadyWidened,
    /// The tip is past the sealed revision and declares something else.
    Moved,
}

/// Where the tip stands, which is what decides whether the amendment writes a
/// successor revision, seals the tip as it stands, or refuses.
///
/// The command advances the tip itself, so a run that failed downstream of the
/// revision write leaves the tip ahead of the sealed revision with nothing
/// wrong: re-reading that as a human's re-scope parks the member forever, since
/// every re-run reproduces the same mismatch. What separates the two is the
/// surface — a tip declaring exactly what this amendment would write is this
/// command's own half-finished work, and anything else is a scope somebody
/// moved for a reason this command cannot see.
fn tip_standing(tip: DigestHex, sealed: DigestHex, tip_surface: &[String], widened: &[String]) -> TipStanding {
    if tip == sealed {
        TipStanding::Sealed
    } else if tip_surface == widened {
        TipStanding::AlreadyWidened
    } else {
        TipStanding::Moved
    }
}

/// P8: advisory report of the sibling surfaces the widening now touches.
fn overlaps(
    client: &Client<'_>,
    bloom: &BloomView,
    amended: &str,
    widened: &[String],
) -> Result<Vec<(String, Vec<String>)>> {
    let mut found = Vec::new();
    for member in siblings(bloom, amended) {
        let Some(sibling) = client.commission(&member.workpiece)?.current else {
            continue;
        };
        let shared = surface_intersection(widened, &sibling.declared_surface);
        if !shared.is_empty() {
            found.push((member.workpiece.clone(), shared));
        }
    }
    Ok(found)
}

/// X: seal the successor with the amended member pinned at its new revision.
fn supersede(
    client: &Client<'_>,
    args: &AmendArgs,
    plan: &AmendPlan,
    scope: DigestHex,
    policy_file: &Path,
) -> Result<String> {
    let task = plan::read_task_file(&args.task_file)?;
    plan::require_task(&task, &args.task_file)?;
    let view = client.view()?;
    let base = plan::resolve_base(&args.base, &view);

    let mut patch = plan::successor_patch(&plan.spec, base, plan.spec.configs.clone());
    let proposals = patch.proposals.get_or_insert_with(Vec::new);
    proposals.retain(|member| !plan.withdrawn.contains(&member.workpiece));
    plan::pin_revisions(proposals, &[(plan.workpiece.clone(), scope)])?;

    let draft = client.open_draft()?;
    client.patch_draft(&draft.draft_id, &patch)?;
    let members = patch.proposals.clone().unwrap_or_default();
    // The amended member's projection carries the widened surface so the
    // successor's declared surface matches the revision it seals against.
    let surfaces: Vec<(String, String)> =
        plan.widened.iter().map(|glob| (plan.workpiece.clone(), glob.clone())).collect();
    let outcome = client.supersede(
        &args.bloom_id,
        &plan::supersede_request(
            &draft.draft_id,
            &members,
            &task,
            &args.projection.input(policy_file)?,
            &[],
            &surfaces,
        )?,
    )?;
    Ok(render_outcome(&outcome.outcome))
}

fn describe(plan: &AmendPlan, ceiling: Tier) -> String {
    let mut out = format!("member     {}\nrequests   {}\n", plan.workpiece, plan.requested.requests);
    for (path, reason) in &plan.requested.reasons {
        let _ = writeln!(out, "  lane asked for {path}: {reason}");
    }

    for (path, glob) in &plan.inherited {
        let _ = writeln!(out, "coarsened  {path} -> {glob} (already declared)");
    }

    out.push_str("added\n");
    for glob in &plan.added {
        let asked = plan
            .coarsened
            .iter()
            .filter(|(_, admitted)| admitted == glob)
            .map(|(path, _)| path.as_str())
            .collect::<Vec<_>>();
        if asked.is_empty() {
            let _ = writeln!(out, "  + {glob}");
        } else {
            let _ = writeln!(out, "  + {glob} (requested {})", asked.join(", "));
        }
    }

    out.push_str(&tier::render(&plan.verdict, &plan.source, ceiling));
    for (sibling, shared) in &plan.new_overlaps {
        let _ = writeln!(out, "overlap    {sibling}: {}", shared.join(", "));
    }
    out
}
