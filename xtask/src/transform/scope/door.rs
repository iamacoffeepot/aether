//! The seal door's refusals, mirrored in the lane that freezes.
//!
//! A scope revision the door will refuse is worthless: the lane spends a model
//! run, freezes, and the operator discovers at seal that the revision has to be
//! reworked by hand. Two of the door's refusals are computable from what the
//! lane already holds, so the lane answers them itself and fails the run with
//! findings the scoper can act on instead of binding a revision that cannot be
//! sealed.
//!
//! Both mirror an existing check rather than restating one: the granularity
//! refusal calls [`refuse_unnamed_file_entries`] — the same words the seal door
//! uses — and the description derivation reuses [`intent_title`], the rule the
//! mirror already applies to a replica's title.

use aether_bloomery::{ApprovalPolicy, ScopeRouting, StageCatalog, StageId, intent_title, split_lane_identity};

use crate::bloom::plan::refuse_unnamed_file_entries;

/// The size a workpiece takes when its routing hint states none.
const DEFAULT_SIZE: &str = "M";

/// The door's declared-surface granularity refusal, or `None` when every entry
/// is admissible.
///
/// `policy` is the checkout's approval policy. `None` is a host condition — no
/// policy file, or one outside the policy grammar — and not a reason to admit
/// the surface: without the policy the lane cannot know which files are named
/// by rule, so it falls back to the standing rule that an entry is a directory
/// glob, and the finding says which check ran.
pub(super) fn surface_granularity(
    policy: Option<&ApprovalPolicy>,
    workpiece: &str,
    surface: &[String],
) -> Option<String> {
    let Some(policy) = policy else {
        return undirected_entry(workpiece, surface);
    };
    refuse_unnamed_file_entries(policy, workpiece, surface).err().map(|refusal| format!("{refusal:#}"))
}

/// The refusal with no policy to consult: the first entry that is not a
/// directory glob. Stated as the narrower check it is, so a reader of the
/// findings knows the policy's named-file exception was never applied.
fn undirected_entry(workpiece: &str, surface: &[String]) -> Option<String> {
    surface.iter().find(|entry| !entry.ends_with("/**")).map(|entry| {
        format!(
            "member {workpiece} declared surface {entry:?} is not a directory glob, and the checkout's approval \
             policy could not be read to tell whether a rule names that file; widen it to a crate glob such as \
             crates/<crate>/src/**",
        )
    })
}

/// The door's non-nullable-section refusal for one empty section.
///
/// The third refusal the lane can answer itself. `problem`, `design` and `plan`
/// are what a construct prompt is rendered from, so every door that admits a
/// revision refuses one that leaves any of them empty — and a lane that froze
/// anyway would have spent a model run producing a work order with a heading
/// and no body.
pub(super) fn empty_section(workpiece: &str, section: &str) -> String {
    format!(
        "member {workpiece} scope revision section {section} is empty, which every door that admits a revision \
         refuses: author it through the scoping call log so the frozen revision carries it",
    )
}

/// A non-empty description for the frozen revision, or the finding that no
/// source supplied one.
///
/// [`WorkpieceBuilder::finish`](aether_bloomery::WorkpieceBuilder::finish)
/// renders an empty description, and the seal door refuses a member whose
/// revision carries one, so the lane derives it here from the three sources it
/// holds, in order: the work order's own markdown heading, the authored
/// `success` statement, then the first sentence of `problem`.
///
/// The heading leads because it is the one source authored as a name. The lane
/// identity line is peeled off first so an order with no heading of its own
/// cannot take the `Workpiece:` pin's.
pub(super) fn description(task: Option<&str>, success: &[String], problem: &[String]) -> Result<String, String> {
    let order = task.map_or("", |task| split_lane_identity(task).0);
    if let Some(title) = intent_title(order.as_bytes()) {
        return Ok(title);
    }
    if let Some(text) = first_nonblank(success) {
        return Ok(String::from(text));
    }
    if let Some(text) = first_nonblank(problem) {
        return Ok(first_sentence(text));
    }
    Err(String::from(
        "the frozen revision would carry an empty description, which the seal door refuses: the work order names no \
         markdown heading, and both the authored success and problem fields are empty.",
    ))
}

/// The routing the frozen revision carries.
///
/// The seal door's completeness gate counts `exactly_one_model_routing`, and an
/// empty `model` counts zero — so a revision frozen with the builder's blank
/// routing is refused before anything reads it. ADR-0208 makes routing a hint
/// rather than a pin and nothing resolves this string back to a model, so the
/// contract is simply one routing the gate can count; naming the catalog's
/// Construct seat keeps the hint true of the line that will build the work.
///
/// `size` comes from the authored routing hint when it states one of S / M / L,
/// because that is the scoper's own judgement of the lap, and falls back to
/// [`DEFAULT_SIZE`] rather than refusing: a hint is prose, and a hint that
/// states risk without stating a letter is not an incomplete workpiece.
pub(super) fn routing(hints: &[String]) -> ScopeRouting {
    ScopeRouting {
        size: authored_size(hints).unwrap_or_else(|| String::from(DEFAULT_SIZE)),
        model: format!("construct: {}", StageCatalog::profile_of(StageId::Construct).model),
    }
}

fn authored_size(hints: &[String]) -> Option<String> {
    hints
        .iter()
        .flat_map(|hint| hint.split(|character: char| !character.is_ascii_alphanumeric()))
        .map(str::to_ascii_uppercase)
        .find(|token| matches!(token.as_str(), "S" | "M" | "L"))
}

fn first_nonblank(texts: &[String]) -> Option<&str> {
    texts.iter().map(|text| text.trim()).find(|text| !text.is_empty())
}

/// The text up to and including its first sentence-ending period — a period
/// that ends the text or is followed by whitespace, so `lib.rs` and
/// `crates/foo` survive intact. Text with no such period is its own first
/// sentence.
fn first_sentence(text: &str) -> String {
    let text = text.trim();
    let end = text
        .char_indices()
        .find(|&(index, character)| {
            character == '.' && text[index + 1..].chars().next().is_none_or(char::is_whitespace)
        })
        .map_or(text.len(), |(index, _)| index + 1);
    String::from(&text[..end])
}

#[cfg(test)]
mod tests {
    use super::{description, routing, surface_granularity};
    use aether_bloomery::{ApprovalPolicy, ApprovalRule, Tier};

    fn policy() -> ApprovalPolicy {
        ApprovalPolicy {
            default: Tier::Judge,
            rules: vec![
                ApprovalRule { glob: "/Cargo.toml".to_owned(), tier: Tier::Human },
                ApprovalRule { glob: "xtask/**".to_owned(), tier: Tier::Auto },
            ],
        }
    }

    fn surface(entry: &str) -> Vec<String> {
        vec![entry.to_owned()]
    }

    #[test]
    fn a_file_entry_the_policy_does_not_name_refuses_before_the_freeze() {
        // The measured class (2026-09-13): the lane's instructions called a
        // concrete path a glob, its own verify accepted one, and all three
        // frozen revisions were then refused at the seal door and reworked by
        // hand. The lane must reach the same answer the door does.
        let refusal = surface_granularity(Some(&policy()), "issue-5916", &surface("xtask/src/transform/scope/mod.rs"))
            .expect("a single file no rule names is refused");
        assert!(refusal.contains("issue-5916"), "the finding names the member: {refusal}");
        assert!(refusal.contains("xtask/src/transform/scope/mod.rs"), "and the offending entry: {refusal}");
        assert!(refusal.contains("crates/<crate>/src/**"), "and the shape to widen to: {refusal}");
    }

    #[test]
    fn a_directory_glob_and_a_policy_named_file_both_pass() {
        assert!(surface_granularity(Some(&policy()), "issue-5916", &surface("xtask/src/**")).is_none());
        assert!(surface_granularity(Some(&policy()), "issue-5916", &surface("Cargo.toml")).is_none());
    }

    #[test]
    fn without_a_policy_a_file_entry_still_refuses() {
        // A missing or malformed policy must not become the thing that admits
        // the surface the door will refuse: the fallback is the standing rule.
        let refusal = surface_granularity(None, "issue-5916", &surface("Cargo.toml"))
            .expect("no policy means no file is known to be named");
        assert!(refusal.contains("Cargo.toml"), "the finding names the entry: {refusal}");
        assert!(surface_granularity(None, "issue-5916", &surface("xtask/src/**")).is_none());
    }

    #[test]
    fn the_description_prefers_the_orders_heading_over_the_authored_fields() {
        // `Workpiece:` is peeled first, so an order with a heading of its own
        // supplies the title and the pin line never becomes one.
        let task = "Workpiece: issue-5924\n\n# Declare crate-glob surfaces\n\nBody prose.\n";
        assert_eq!(
            description(Some(task), &["ignored".to_owned()], &[]).expect("the heading is a description"),
            "Declare crate-glob surfaces",
        );
    }

    #[test]
    fn a_headingless_order_falls_back_through_success_to_the_problems_first_sentence() {
        // The shape all three of 2026-09-13's lane-frozen revisions had: an
        // order with no heading and an empty description, which the seal door
        // refuses closed. Success leads; problem's first sentence is the floor.
        let task = "Workpiece: issue-5924\n\nfix the lane so the door admits its surfaces\n";
        assert_eq!(
            description(Some(task), &["The lane declares directory globs.".to_owned()], &[]).expect("success"),
            "The lane declares directory globs.",
        );
        assert_eq!(
            description(
                Some(task),
                &[String::new()],
                &["Step 3 names xtask/src/lib.rs as a glob. The door disagrees.".to_owned()],
            )
            .expect("problem"),
            "Step 3 names xtask/src/lib.rs as a glob.",
        );
    }

    #[test]
    fn the_routing_states_one_model_and_takes_its_size_from_the_hint() {
        // The gate counts `exactly_one_model_routing`; the builder's blank
        // routing counts zero, which is what refused every lane-frozen
        // revision at seal on 2026-09-13. A hint that states no letter must
        // still produce a complete routing rather than a refusal.
        let hinted = routing(&["Judgement: mechanical, size L, low risk.".to_owned()]);
        assert_eq!(hinted.size, "L");
        assert!(hinted.model.starts_with("construct: "), "the routing names the construct seat: {hinted:?}");
        assert!(hinted.model.len() > "construct: ".len(), "and names a model: {hinted:?}");

        assert_eq!(routing(&["no letter here".to_owned()]).size, "M", "a letterless hint is not incompleteness");
        assert_eq!(routing(&[]).size, "M", "and neither is an unwritten hint");
    }

    #[test]
    fn no_heading_no_success_and_no_problem_is_a_refusal() {
        let finding = description(Some("Workpiece: issue-5924\n\nprose only\n"), &[], &[])
            .expect_err("nothing supplies a description");
        assert!(finding.contains("success"), "the finding names the empty fields: {finding}");
        assert!(finding.contains("problem"), "the finding names the empty fields: {finding}");
    }
}
