//! Assembling a bloom's landing proposal from what its lanes wrote.
//!
//! A landed bloom's mainline commit is the proposal's title, forever — mainline
//! squash-merges with it as the subject. So the title is the one the model that
//! made the change wrote, read back out of the store the construct/refine lane
//! filed it in, and the body is that message's prose plus the closing lines the
//! bloom's membership addresses.
//!
//! Everything here **falls back to a valid title, never to a blocked landing**.
//! A member that named nothing, or a message whose subject the repository's
//! `Lint title` check would refuse, drops to the floor title. A GitHub issue
//! title is not a rung — GitHub is a replica.

use aether_bloomery::{Adjudication, BloomId, Disposition, Event, Fact};
use aether_bloomery_github::{LandingProposal, canonical_issue_number};
use aether_data::wire::from_bytes;

use crate::store::StoreBackend;

/// The Conventional Commits types `.github/workflows/lint-title.yml` accepts.
/// The gate and this predictor change together: a title this admits and the gate
/// refuses blocks the landing at the very last step, after the bloom has already
/// resolved.
const ACCEPTED_TYPES: [&str; 7] = ["feat", "fix", "chore", "docs", "perf", "refactor", "flake"];

/// One member's contribution to the proposal: the message its lane wrote, and
/// the object its workpiece addresses.
struct Member {
    /// The commit message the member's resolving candidate was captured under,
    /// when its lane wrote one.
    message: Option<String>,
    /// The issue the workpiece addresses, when it addresses one at all — a
    /// workpiece id that names no object contributes no closing line rather
    /// than a guessed number.
    issue: Option<u64>,
}

/// Assemble `bloom`'s landing proposal.
///
/// A store fault propagates: the caller stops its ack prefix and re-drains, which
/// is the same answer every other store read in the drain loop gives. A GitHub
/// issue title is not a fallback — GitHub is a replica, never an input.
pub(super) fn assemble(store: &mut dyn StoreBackend, bloom: &BloomId) -> rusqlite::Result<LandingProposal> {
    let members = roster(store, bloom)?;
    let waived = adjudications(store, bloom)?;
    Ok(LandingProposal { title: title_for(&members), body: body_for(&members, &waived) })
}

/// The operator adjudications this bloom carries, oldest first (#4957).
///
/// Read out of the journal rather than a projection of it, because the journal
/// is where the operator's words are: the adjudication fact carries the reason
/// verbatim, and nothing else in the store does. The scan is a whole-table read,
/// which is affordable exactly here — a bloom lands once, and a coordinator's
/// journal is the events of the blooms it has run, not a growing log of every
/// attempt.
///
/// A row that does not decode is skipped rather than propagated. This is the
/// proposal's prose, and the same rule the title fallbacks follow applies: a
/// malformed row costs the body a sentence, never the bloom its landing.
fn adjudications(store: &mut dyn StoreBackend, bloom: &BloomId) -> rusqlite::Result<Vec<Adjudication>> {
    Ok(store
        .list_events()?
        .iter()
        .filter_map(|bytes| from_bytes::<Event>(bytes).ok())
        .filter_map(|event| match event.fact {
            Fact::OperatorAdjudication { bloom: admitted, adjudication } if admitted == *bloom => Some(adjudication),
            _ => None,
        })
        .collect())
}

/// The waived-findings section, or `None` when the bloom was never adjudicated.
///
/// The point of carrying it into the proposal is that the merged history names
/// what was waived and why: a landing that only its coordinator knows was
/// overridden reads, forever after, as a landing that passed its gates.
fn waivers_section(waived: &[Adjudication]) -> Option<String> {
    if waived.is_empty() {
        return None;
    }
    let lines: Vec<String> = waived
        .iter()
        .map(|adjudication| {
            let disposition = match adjudication.disposition {
                Disposition::Accepted => "accepted".to_owned(),
                Disposition::Deferred { issue } => format!("deferred to #{issue}"),
            };
            format!(
                "- {} by {} ({disposition}): {}",
                finding_count(adjudication),
                adjudication.operator,
                adjudication.reason
            )
        })
        .collect();

    Some(format!("### Adjudicated findings\n\n{}", lines.join("\n")))
}

/// How many composition findings one adjudication closed, spelled for prose.
fn finding_count(adjudication: &Adjudication) -> String {
    match adjudication.findings.len() {
        1 => "1 composition finding".to_owned(),
        count => format!("{count} composition findings"),
    }
}

/// The bloom's members, in workpiece order, each with whatever its lane left.
///
/// The persisted work-order roster is the membership read the host already has
/// — the same one the aggregate review's findings decomposition attributes
/// against — because the land outbox payload carries three digests and no
/// membership.
fn roster(store: &mut dyn StoreBackend, bloom: &BloomId) -> rusqlite::Result<Vec<Member>> {
    store
        .list_dispatch_descriptions(bloom.0.as_bytes())?
        .into_iter()
        .map(|(workpiece, _)| workpiece)
        .filter(|workpiece| !workpiece.is_empty())
        .map(|workpiece| {
            let message = store.lookup_candidate_commit_message(bloom.0.as_bytes(), &workpiece)?;
            let issue = canonical_issue_number(&workpiece);
            Ok(Member { message, issue })
        })
        .collect()
}

/// The proposal's title, or `None` to land under the adapter's floor.
///
/// A bloom lands under an authored title only when one member authored it: a
/// several-member bloom is several changes, and picking one member's subject to
/// stand for all of them would name the mainline commit after a fraction of what
/// it carries.
fn title_for(members: &[Member]) -> Option<String> {
    let [member] = members else {
        return None;
    };
    member.message.as_deref().and_then(subject_of).filter(|subject| title_is_lint_valid(subject)).map(str::to_owned)
}

/// The proposal's body: what the lanes wrote, then whatever an operator waived
/// to get here, then one closing line per member that addresses an object. The
/// provenance footer is the source port's and is appended below this.
fn body_for(members: &[Member], waived: &[Adjudication]) -> String {
    let mut sections: Vec<String> = match members {
        // One member: the title already carries its subject, so the body is the
        // message's prose and nothing else.
        [member] => member.message.as_deref().and_then(body_of).map(str::to_owned).into_iter().collect(),
        // Several: each member's whole message becomes its own section, headed by
        // its subject, because no single one of them is the title.
        members => members.iter().filter_map(|member| member.message.as_deref()).map(section_of).collect(),
    };
    sections.extend(waivers_section(waived));
    sections.extend(members.iter().filter_map(|member| member.issue).map(|issue| format!("Closes #{issue}")));
    sections.join("\n\n")
}

/// One member's message as a body section: its subject as a heading, its prose
/// below.
fn section_of(message: &str) -> String {
    match (subject_of(message), body_of(message)) {
        (Some(subject), Some(body)) => format!("### {subject}\n\n{body}"),
        (Some(subject), None) => format!("### {subject}"),
        (None, body) => body.unwrap_or_default().to_owned(),
    }
}

/// A commit message's subject — its first line, when it has a non-empty one.
fn subject_of(message: &str) -> Option<&str> {
    message.lines().next().map(str::trim).filter(|subject| !subject.is_empty())
}

/// A commit message's body — everything past the subject line, trimmed. `None`
/// for a subject-only message.
fn body_of(message: &str) -> Option<&str> {
    message.split_once('\n').map(|(_, body)| body.trim()).filter(|body| !body.is_empty())
}

/// Whether `title` is a header the repository's required `Lint title` check
/// would accept — the mirror of `.github/workflows/lint-title.yml`: a
/// Conventional Commits header whose type is one of [`ACCEPTED_TYPES`], whose
/// scope is optional (`requireScope: false`) but non-empty when written, and
/// whose subject is non-empty and does not start with an uppercase letter
/// (`subjectPattern: ^(?![A-Z]).+$`).
///
/// Fail-closed: anything this cannot recognize is not lint-valid, so an
/// unrecognized shape falls back to a title that is rather than being proposed
/// and refused.
fn title_is_lint_valid(title: &str) -> bool {
    let Some((header, subject)) = title.split_once(": ") else {
        return false;
    };
    let subject_accepted = !subject.is_empty() && !subject.starts_with(|first: char| first.is_ascii_uppercase());

    subject_accepted && type_and_scope_are_accepted(header.strip_suffix('!').unwrap_or(header))
}

/// Whether a header's type and optional scope are the accepted ones, `header`
/// having already had its breaking-change `!` stripped.
fn type_and_scope_are_accepted(header: &str) -> bool {
    let Some((kind, scope)) = header.split_once('(') else {
        return ACCEPTED_TYPES.contains(&header);
    };
    // A scope was written, so it has to say something: `feat(): x` names no
    // scope in two characters more than `feat: x` does.
    let scope_accepted = scope.strip_suffix(')').is_some_and(|scope| !scope.is_empty() && !scope.contains(['(', ')']));

    scope_accepted && ACCEPTED_TYPES.contains(&kind)
}

#[cfg(test)]
mod tests {
    use super::{ACCEPTED_TYPES, body_of, subject_of, title_is_lint_valid};

    // The predictor and the gate change together: a title this admits and
    // `.github/workflows/lint-title.yml` refuses blocks the landing at the very
    // last step, after a bloom has already resolved and its work is on a branch.
    #[test]
    fn the_validator_mirrors_the_lint_title_gate() {
        assert!(title_is_lint_valid("feat(crate:aether-text): shelf-pack the glyph atlas"));
        assert!(title_is_lint_valid("chore: bump the pinned toolchain"), "the gate does not require a scope");
        assert!(title_is_lint_valid("refactor(xtask)!: split the lane arms"), "a breaking-change marker is accepted");

        assert!(!title_is_lint_valid("wip(xtask): split the lane arms"), "`wip` is outside the closed type set");
        assert!(!title_is_lint_valid("feat(xtask): Shelf-pack the atlas"), "the subject may not start uppercase");
        assert!(!title_is_lint_valid("feat(xtask): "), "an empty subject is no subject");
        assert!(!title_is_lint_valid("shelf-pack the glyph atlas"), "prose with no header is not a title");
        assert!(!title_is_lint_valid("feat(): shelf-pack the atlas"), "a written scope has to say something");
        assert!(!title_is_lint_valid("feat(xtask):shelf-pack"), "the gate's separator is a colon and a space");
    }

    // Every type the gate lists is one this admits — a type dropped from the
    // array silently sends its blooms to the floor title.
    #[test]
    fn every_accepted_type_makes_a_valid_title() {
        for kind in ACCEPTED_TYPES {
            assert!(title_is_lint_valid(&format!("{kind}(xtask): do the thing")), "`{kind}` is an accepted type");
        }
    }

    #[test]
    fn a_message_splits_into_its_subject_and_prose() {
        let message = "fix(crate:aether-fs): reject a traversing path\n\nThe adapter joined the path.\n";
        assert_eq!(subject_of(message), Some("fix(crate:aether-fs): reject a traversing path"));
        assert_eq!(body_of(message), Some("The adapter joined the path."));

        // A subject-only message has prose nowhere, which is an absence rather
        // than an empty section in the proposal body.
        assert_eq!(subject_of("chore: tidy"), Some("chore: tidy"));
        assert_eq!(body_of("chore: tidy"), None);
        assert_eq!(body_of("chore: tidy\n\n   \n"), None);
        assert_eq!(subject_of("\n\nprose only"), None);
    }
}
