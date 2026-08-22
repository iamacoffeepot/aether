//! Where the amendment's requested paths come from, and what makes them
//! admissible.
//!
//! Two sources, unioned: the journaled request the declining lane produced
//! (ADR-0207), and whatever the operator adds with `--path`. Both are checked
//! the same way — the lane is untrusted, and an operator typo is no better
//! than a lane's glob.

use anyhow::{Result, bail};

use crate::bloom::dto::{BloomView, MemberView};

/// The paths an amendment is being asked to add, and where each came from.
#[derive(Debug)]
pub struct Requested {
    /// The globs, in request order: the lane's first, then the operator's.
    pub globs: Vec<String>,
    /// The lane's one-line reason per path, for the printed plan. Empty for a
    /// path the operator supplied.
    pub reasons: Vec<(String, String)>,
    /// How many requests this member has made in this bloom, when it made any.
    pub requests: u32,
}

/// The member of `bloom` named `workpiece`.
pub fn member<'a>(bloom: &'a BloomView, workpiece: &str) -> Result<&'a MemberView> {
    bloom
        .members
        .iter()
        .find(|member| member.workpiece == workpiece)
        .ok_or_else(|| anyhow::anyhow!("bloom {} has no member {workpiece}", bloom.id))
}

/// Collect the journaled request and the operator's extra paths.
///
/// A member with no journaled request may still be amended, but only when the
/// operator names the paths themselves: amending a member that asked for
/// nothing, on the strength of nothing, is the shape that turns a boundary
/// into a suggestion.
pub fn collect(member: &MemberView, extra: &[String]) -> Result<Requested> {
    let awaiting = member.awaiting_surface.as_ref();
    if awaiting.is_none() && extra.is_empty() {
        bail!("member {} carries no surface request; pass --path to amend it anyway", member.workpiece);
    }

    let mut globs: Vec<String> = Vec::new();
    let mut reasons: Vec<(String, String)> = Vec::new();
    for request in awaiting.map(|awaiting| awaiting.paths.as_slice()).unwrap_or_default() {
        if !globs.contains(&request.path) {
            globs.push(request.path.clone());
            reasons.push((request.path.clone(), request.reason.clone()));
        }
    }
    for glob in extra {
        if !globs.contains(glob) {
            globs.push(glob.clone());
        }
    }

    Ok(Requested { globs, reasons, requests: awaiting.map_or(0, |awaiting| awaiting.requests) })
}

/// The request must name the revision the member is actually sealed at.
///
/// A request bound to an older revision is one a human has already moved past;
/// widening on it would chain a successor off a revision the bloom no longer
/// carries, and the seal would refuse it as stale after the key had signed.
pub fn binding_holds(member: &MemberView) -> Result<()> {
    if let Some(awaiting) = &member.awaiting_surface
        && awaiting.scope_revision != member.scope_revision
    {
        bail!(
            "member {}'s request names revision {} but the bloom sealed it at {}; re-scope rather than amend",
            member.workpiece,
            awaiting.scope_revision,
            member.scope_revision,
        );
    }
    Ok(())
}
