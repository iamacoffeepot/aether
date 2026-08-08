//! The bloom ref namespace names the reactors share.
//!
//! A candidate ref has two ends in this crate: the executor reactor pushes an
//! admitted capture to it, and the integrate reactor merges it when a fold has
//! to combine members. Naming it in one place keeps those ends from drifting
//! into two spellings of the same address — a mismatch that does not fail
//! loudly, it addresses a branch that is not there.
//!
//! The source port owns the rest of the namespace (`integration`, `attempt`,
//! `checkpoint`, landing) because it both writes and reads those itself. This
//! module holds only the names a chassis-side reactor has to construct.

use aether_bloomery::BloomId;
use aether_bloomery_github::short_hex;

/// The bloom-namespace ref an admitted candidate is pushed to —
/// `refs/heads/bloom/<short bloom hex>/candidate/<workpiece>` (ADR-0152),
/// force-updated because refinement supersedes. The workpiece segment is
/// sanitized to git-safe ref characters; ids are machine-authored, so this is a
/// tripwire, not a codec.
///
/// The bloom segment is [`short_hex`] — the same rendering the source port's
/// integration / attempt / checkpoint / landing refs use, so one bloom's whole
/// ref namespace reads as one namespace.
#[must_use]
pub fn candidate_ref_name(bloom: &BloomId, workpiece: &str) -> String {
    let safe: String = workpiece
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("refs/heads/bloom/{}/candidate/{safe}", short_hex(&bloom.0))
}
