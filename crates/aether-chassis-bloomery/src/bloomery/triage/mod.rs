//! Repair-lap triage (#4959): does a repair's diff plausibly address the
//! finding it was dispatched for?
//!
//! A repair lap can satisfy every mechanical gate without repairing anything.
//! Bloom `10a1228c` is the live case: the finding named `representative()`
//! coverage in `golden_decisions.rs`, and the refine lane "repaired" it — twice —
//! by swapping `unwrap()` for `expect()` in that file. Nothing could disagree,
//! because a coverage gap fails no compiler, so the dodge was only discovered a
//! full aggregate-review round later, each round costing an Opus judge lap.
//!
//! So the loop gets a step between a repair lap completing and the re-judge
//! dispatch, and it is host-side and mechanical: read what the finding names,
//! read what the lap changed, and bounce the lap when the two do not intersect.
//! No judge round is spent to learn what a diff inspection already shows.
//!
//! **Where it applies** is decided by the admission broker's `is_weave_repair`:
//! the composition workpiece's weave repair, and only that. Post-ADR-0191 the composition is the only workpiece
//! whose findings are a judge's prose — an aggregate refusal no longer re-opens a
//! member, so every finding reaching a member's `Refine` is mechanical gate
//! output, which names the symptom's types and location rather than the thing a
//! fix must change. The member loop also already has its own unrepaired-candidate
//! detector in ADR-0178's repeated-verifier accounting, and it costs no judge
//! round to collect.
//!
//! **Advisory-strict.** The triage refuses only when the diff *demonstrably*
//! changes nothing the finding names. Everything uncertain passes: a finding that
//! names no symbol, a lap whose diff was never filed, a diff past the size cap,
//! an empty change set. A false bounce costs the workpiece one lap; a false pass
//! costs a judge round — so the rules lean toward passing wherever they are
//! unsure, and the one place they are strict is the incident's own shape (changed
//! lines, not the file's untouched context; see [`diff`]).
//!
//! **A bounce is a failing repair lap and nothing new.** It admits as an
//! ordinary non-passing [`Fact::AttemptCompleted`](aether_bloomery::Fact), so it
//! spends the retry budget a refused lap spends, its capture is discarded, and a
//! workpiece that dodges repeatedly wedges to the operator instead of opening a
//! second loop of its own.
//!
//! The step up the issue describes — one cheap model call, "here is the finding,
//! here is the diff, does this plausibly address it" — is deliberately not here.
//! This is the mechanical floor it would sit on.

mod diff;
mod names;

use diff::changed_surface;
use names::named_surface;

/// The largest repair diff the triage will read. Past it the lap passes
/// untriaged: a bounded mechanical check that cannot afford to be wrong does not
/// guess about a change it did not read, and a repair lap that rewrote a
/// quarter-megabyte of source is not the dodge this catches.
pub const MAX_TRIAGED_DIFF_BYTES: usize = 256 * 1024;

/// What the triage concluded about one repair lap.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TriageVerdict {
    /// The finding named no symbol, so there was no claim to test. Passes.
    NothingNamed,
    /// No diff was available — the lap's executor backend files none, the diff
    /// was past [`MAX_TRIAGED_DIFF_BYTES`], or it parsed to no change at all.
    /// Passes.
    NotInspected,
    /// The lap changed something the finding names. Passes, naming what matched
    /// so a reader can see what the triage credited.
    Addressed(String),
    /// The lap changed nothing the finding names. Bounces, carrying what the
    /// finding named so the next lap can be told what it missed.
    Dodged(Vec<String>),
}

impl TriageVerdict {
    /// Whether this verdict bounces the lap. Exactly one variant does.
    #[must_use]
    pub fn bounces(&self) -> bool {
        matches!(self, Self::Dodged(_))
    }
}

/// Triage one repair lap: `finding` is the advisory text its work order was
/// assembled from, `diff` its captured change.
///
/// The decision, in order:
///
/// 1. A diff that is absent, oversized, or parses to no substantive change is
///    not inspected — pass.
/// 2. A finding that names no **symbol** is not triaged — pass. A finding that
///    names only a file names a *location*, and locations move: a compiler
///    diagnostic points at where a symptom surfaced, not at where the fix
///    belongs, and the mechanical `Verify` failures that dispatch most repair
///    laps are exactly that shape. Holding a repair to a file it was never told
///    to edit would bounce honest laps on the highest-volume path in the loop.
///    Paths are still extracted — that is how a name that is a *path* is kept
///    from being read as a symbol, and how the bounce note says where to look.
/// 3. **The symbols decide.** At least one must appear as a whole word in what
///    the lap changed. A named path the lap touched does not rescue it: editing
///    the file a finding named while leaving the thing it named alone is
///    precisely the dodge.
#[must_use]
pub fn triage_repair(finding: &str, diff: Option<&str>) -> TriageVerdict {
    let Some(diff) = diff.filter(|diff| diff.len() <= MAX_TRIAGED_DIFF_BYTES) else {
        return TriageVerdict::NotInspected;
    };
    let changed = changed_surface(diff);
    if changed.is_empty() {
        return TriageVerdict::NotInspected;
    }
    let named = named_surface(finding);
    if named.symbols.is_empty() {
        return TriageVerdict::NothingNamed;
    }

    named
        .symbols
        .iter()
        .find(|symbol| changed.mentions(symbol))
        .map_or_else(|| TriageVerdict::Dodged(named.named()), |symbol| TriageVerdict::Addressed(symbol.clone()))
}

/// The section a bounce appends to the workpiece's own findings row, so the lap
/// that follows is told what the one before it missed rather than re-reading the
/// identical prose and repeating itself.
///
/// Appended to the workpiece's row and never to the bloom-scoped one: the frozen
/// aggregate set is what a delta-confirm review is framed against, and a note
/// about one lane's lap has no business in it.
#[must_use]
pub fn triage_note(named: &[String]) -> String {
    format!(
        "## Repair triage\n\nThe previous repair lap changed nothing this finding names ({}). It was bounced \
         without a re-judge and its work was discarded, and the lap spent a retry off this workpiece's budget. \
         Repair what the finding names — or, if the right fix genuinely lives elsewhere, say so in the lap's \
         message so the next reader can see the reasoning.",
        named.iter().map(|name| format!("`{name}`")).collect::<Vec<_>>().join(", "),
    )
}

#[cfg(test)]
mod tests;
