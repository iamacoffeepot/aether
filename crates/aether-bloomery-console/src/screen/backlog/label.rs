//! Workpiece identity as the pane prints it.
//!
//! A GitHub number is a dim trailing annotation when the id is a canonical
//! `issue-<N>` spelling. It is never identity, never a sort key, and never
//! a fetch path.

/// The `<N>` of a canonical `issue-<N>` id, if `workpiece` is one.
///
/// Display annotation only. Fetch and sort use the workpiece id itself.
#[must_use]
pub fn github_annotation(workpiece: &str) -> Option<u64> {
    let number = workpiece.strip_prefix("issue-")?;
    if number.is_empty() || number.starts_with('0') || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    number.parse().ok()
}

/// The pane's sort / fetch identity: the workpiece id, never its annotation.
#[must_use]
pub fn workpiece_key(workpiece: &str) -> &str {
    workpiece
}

/// How the annotation paints, or `None` when the workpiece has no number.
#[must_use]
pub fn annotation_text(workpiece: &str) -> Option<String> {
    github_annotation(workpiece).map(|number| format!("#{number}"))
}

#[cfg(test)]
mod tests {
    use super::{annotation_text, github_annotation, workpiece_key};

    #[test]
    fn a_github_number_is_an_annotation_not_identity() {
        // The plausible bug: the pane sorts or fetches by the issue number, so
        // a workpiece without one disappears and `issue-9` sorts after `issue-10`.
        assert_eq!(github_annotation("issue-5158"), Some(5158));
        assert_eq!(github_annotation("wp-local"), None);
        assert_eq!(github_annotation("issue-007"), None);
        assert_eq!(annotation_text("issue-5158").as_deref(), Some("#5158"));
        assert_eq!(annotation_text("wp-local"), None);

        let mut ids = ["issue-10", "issue-9", "wp-local"];
        ids.sort_by_key(|id| workpiece_key(id));
        assert_eq!(ids, ["issue-10", "issue-9", "wp-local"]);
        assert_eq!(workpiece_key("issue-5158"), "issue-5158");
        assert_eq!(workpiece_key("wp-local"), "wp-local");
    }
}
