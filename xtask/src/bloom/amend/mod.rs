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
mod tier;

#[cfg(test)]
mod tests;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use aether_bloomery::{KeyId, Tier, TierVerdict, gate_widening, surface_additions, surface_intersection, tier_verdict};
use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};

use super::client::{Client, bloom_in};
use super::dto::{BloomSpec, BloomView, CommissionShowView, DigestHex, ScopeRevisionView};
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
    current: ScopeRevisionView,
    commission: CommissionShowView,
    requested: Requested,
    added: Vec<String>,
    widened: Vec<String>,
    verdict: TierVerdict,
    source: PolicySource,
    /// Sibling members whose declared surface the widening now overlaps, with
    /// the globs both permit. Advisory: the seal journals the overlap and may
    /// derive a dependency edge from it.
    new_overlaps: Vec<(String, Vec<String>)>,
}

pub fn run(client: &Client<'_>, args: &AmendArgs, policy_file: &Path) -> Result<String> {
    let Some(plan) = preflight(client, args, policy_file)? else {
        return Ok(format!("member {} already holds every requested path; nothing to amend\n", args.workpiece));
    };

    let mut out = describe(&plan, args.accept_tier.tier());
    if args.dry_run {
        out.push_str("\ndry run: nothing was written\n");
        return Ok(out);
    }

    let key = OperatorKey::load(
        KeyId(args.signer.clone()),
        args.seed_file.as_deref().context("--seed-file is required to sign the amendment's approval")?,
    )?;

    let widened = plan.current.to_revision().with_widened_surface(&plan.added);
    out.push_str("writing the widened revision; the commission tip advances here\n");
    let scope = revision::write_widened(client, &plan.workpiece, &widened)?;
    let _ = writeln!(out, "revision   {scope}");

    if revision::approve(client, &plan.commission, &plan.workpiece, scope, &key)? {
        let _ = writeln!(out, "approved   by {}", key.signer.0);
    } else {
        out.push_str("approved   already stored\n");
    }

    out.push_str(&supersede(client, args, &plan, scope, policy_file)?);
    Ok(out)
}

/// P1 – P8. Entirely reads; `None` means the request is already covered.
fn preflight(client: &Client<'_>, args: &AmendArgs, policy_file: &Path) -> Result<Option<AmendPlan>> {
    let view = client.view()?;
    let bloom = bloom_in(&view, &args.bloom_id)?;
    let member = request::member(bloom, &args.workpiece)?;
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
    if tip != member.scope_revision {
        bail!(
            "commission {} is at revision {tip} but the bloom sealed member {} at {}; a human moved the scope — \
             re-scope and supersede rather than amend",
            args.workpiece,
            args.workpiece,
            member.scope_revision,
        );
    }
    siblings_are_sealable(client, bloom, &args.workpiece)?;

    let added = surface_additions(&current.declared_surface, &requested.globs)
        .map_err(|glob| anyhow::anyhow!("requested path `{glob}` is outside the declared-surface grammar"))?;
    if added.is_empty() {
        return Ok(None);
    }

    let spec = client.spec_for(&args.bloom_id)?;
    let (policy, source) = tier::resolve_policy(client, &spec, policy_file)?;
    let verdict = tier_verdict(&policy, &current.declared_surface, &added);
    if let Err(offending) = gate_widening(&verdict, args.accept_tier.tier()) {
        bail!(
            "{}\nthe amendment is refused: {} above --accept-tier {:?}. The member stays parked.",
            tier::render(&verdict, &source, args.accept_tier.tier()),
            offending.iter().map(|(glob, tier)| format!("`{glob}` resolves {tier:?}")).collect::<Vec<_>>().join(", "),
            args.accept_tier.tier(),
        );
    }

    let mut widened_surface = current.declared_surface.clone();
    widened_surface.extend(added.iter().cloned());
    let new_overlaps = overlaps(client, bloom, &args.workpiece, &widened_surface)?;

    Ok(Some(AmendPlan {
        workpiece: args.workpiece.clone(),
        spec,
        current,
        commission,
        requested,
        added,
        widened: widened_surface,
        verdict,
        source,
        new_overlaps,
    }))
}

/// P4: every sibling must be at its own commission tip and carry an approval.
///
/// Checked before anything is signed, because the successor seal refuses on a
/// stale sibling scope or a missing sibling approval — and by then the
/// operator's key has already signed and the commission tip has already moved.
fn siblings_are_sealable(client: &Client<'_>, bloom: &BloomView, amended: &str) -> Result<()> {
    for member in &bloom.members {
        if member.workpiece == amended {
            continue;
        }
        let sibling = client.commission(&member.workpiece)?;
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
    }
    Ok(())
}

/// P8: advisory report of the sibling surfaces the widening now touches.
fn overlaps(
    client: &Client<'_>,
    bloom: &BloomView,
    amended: &str,
    widened: &[String],
) -> Result<Vec<(String, Vec<String>)>> {
    let mut found = Vec::new();
    for member in &bloom.members {
        if member.workpiece == amended {
            continue;
        }
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

    out.push_str("added\n");
    for glob in &plan.added {
        let _ = writeln!(out, "  + {glob}");
    }

    out.push_str(&tier::render(&plan.verdict, &plan.source, ceiling));
    for (sibling, shared) in &plan.new_overlaps {
        let _ = writeln!(out, "overlap    {sibling}: {}", shared.join(", "));
    }
    out
}
