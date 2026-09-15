//! Render a [`ScopeRevision`] as managed-heading markdown.
//!
//! Markdown is derived from the signed fields. The stored bytes are the
//! revision encoding, never a byte-preserved issue body.

use alloc::string::String;
use alloc::vec::Vec;

use super::approval::SurfacePattern;
use super::commission::ScopeRevision;

const PROBLEM: &str = "Problem statement";
const DESIGN: &str = "Design notes";
const PLAN: &str = "Implementation plan";
const DEPENDS: &str = "Depends on";
const SURFACE: &str = "Declared surface";
const CRATES: &str = "Declared crates";
const PROTECTED: &str = "Protected files";
const READS: &str = "Reads";
const DOGFOOD: &str = "Dogfood brief";

const SIZE_LABEL: &str = "**Size:**";
const MODEL_LABEL: &str = "**Implementation model:**";
const REASON_LABEL: &str = "**Routing reason:**";

/// What the renderer writes on the reason line.
///
/// [`super::ScopeRouting`] stores size and model and discards the reason the
/// parser validated, so a re-render has no authored reason to restate and
/// says so rather than inventing one.
const RERENDERED_REASON: &str = "re-rendered from the stored revision, which carries no authored reason";

impl ScopeRevision {
    /// Work-order text a lane or an outward replica reads.
    ///
    /// Every section comes from its own signed field: the problem, the design,
    /// the plan and its routing lines, the surface declaration, and the dogfood
    /// brief when the scope asked for one. A GitHub issue body is never an
    /// input, and neither is [`Self::description`] beyond the heading below.
    ///
    /// The fields are the authority because they are what the rest of the
    /// estate reads. [`Self::declared_surface`] is what the seal door and the
    /// containment gate check; `problem` / `design` / `plan` are what the
    /// completeness gate counts and what the revision doors refuse when empty.
    /// A renderer that echoed a stored body instead would hand the lane text no
    /// gate ever looked at — which is how a commission carrying a one-line
    /// title in `description` and nothing in its fields dispatched a lane that
    /// had a subject and no work order.
    ///
    /// [`Self::description`] is the one-line summary it was named for, so it
    /// renders as the order's heading and nowhere else. A description holding a
    /// whole body — what the markdown parse path stores, so the next edit of a
    /// work order starts from the bytes an approval covers — is not a summary
    /// and is left out: the fields it was rendered from are right below it.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        if let Some(summary) = self.summary_heading() {
            out.push_str("# ");
            out.push_str(summary);
            out.push_str("\n\n");
        }
        out.push_str(&self.render_fields());
        out
    }

    /// [`Self::description`] when it is the one-line summary the field is for,
    /// and `None` when it carries a body instead.
    fn summary_heading(&self) -> Option<&str> {
        let summary = self.description.trim();
        (!summary.is_empty() && !summary.contains('\n')).then_some(summary)
    }

    /// Managed headings rendered from the signed fields, ignoring
    /// [`Self::description`].
    ///
    /// The parse path stores this as the revision's description so the next
    /// edit of the work order starts from the same bytes the approval covers.
    #[must_use]
    pub fn render_fields(&self) -> String {
        let mut out = String::new();
        push_section(&mut out, PROBLEM, &self.problem);
        push_section(&mut out, DESIGN, &self.design);
        out.push_str("## ");
        out.push_str(PLAN);
        out.push_str("\n\n");
        out.push_str(self.plan.trim());
        out.push_str("\n\n");
        out.push_str(SIZE_LABEL);
        out.push(' ');
        out.push_str(&self.routing.size);
        out.push('\n');
        out.push_str(MODEL_LABEL);
        out.push(' ');
        out.push_str(&self.routing.model);
        out.push('\n');
        out.push_str(REASON_LABEL);
        out.push(' ');
        out.push_str(RERENDERED_REASON);
        out.push('\n');
        if !self.dependencies.is_empty() {
            out.push_str("\n## ");
            out.push_str(DEPENDS);
            out.push_str("\n\n");
            for dep in &self.dependencies {
                out.push_str("- ");
                out.push_str(&dep.0);
                out.push('\n');
            }
        }
        push_declaration(&mut out, self);
        if !self.declared_reads.is_empty() {
            push_list(&mut out, READS, &self.declared_reads);
        }
        if !self.dogfood_brief.trim().is_empty() {
            out.push('\n');
            push_section(&mut out, DOGFOOD, &self.dogfood_brief);
        }
        out
    }
}

/// The surface-declaration blocks a revision renders to.
///
/// A crate-declared scope renders the blocks it was written with, not the globs
/// they expanded to: the derived surface is a machine artifact of the workspace
/// graph, and re-rendering it as the operator's own declaration would turn the
/// next edit of this work order into a hand-maintained file list — the thing the
/// crate block exists to stop.
fn push_declaration(out: &mut String, revision: &ScopeRevision) {
    if revision.declared_crates.is_empty() {
        push_list(out, SURFACE, &revision.declared_surface);
    } else {
        push_list(out, CRATES, &revision.declared_crates);
        let protected = protected_files(&revision.declared_surface);
        if !protected.is_empty() {
            push_list(out, PROTECTED, &protected);
        }
    }
}

fn push_list(out: &mut String, name: &str, entries: &[String]) {
    out.push_str("\n## ");
    out.push_str(name);
    out.push_str("\n\n");
    for entry in entries {
        out.push_str(entry);
        out.push('\n');
    }
}

/// The file-granular entries of a derived surface — what `## Protected files`
/// put there.
///
/// Read back out of the surface rather than stored beside it: a derived
/// surface's only literal entries are the protected ones (a crate subtree is a
/// `dir/**`, a shared root likewise), and the granularity check refuses any
/// literal the approval policy does not name, so nothing else can be sitting in
/// that position.
fn protected_files(surface: &[String]) -> Vec<String> {
    surface
        .iter()
        .filter(|glob| matches!(SurfacePattern::parse(glob), Some(SurfacePattern::Exact(_))))
        .cloned()
        .collect()
}

fn push_section(out: &mut String, name: &str, body: &str) {
    out.push_str("## ");
    out.push_str(name);
    out.push_str("\n\n");
    out.push_str(body.trim());
    out.push_str("\n\n");
}

#[cfg(test)]
mod tests {
    use alloc::string::{String, ToString};
    use alloc::vec;
    use alloc::vec::Vec;

    use super::{DESIGN, DOGFOOD, PLAN, PROBLEM, SURFACE};
    use crate::ids::WorkpieceId;
    use crate::values::commission::{SCOPE_REVISION_SCHEMA, ScopeRevision, ScopeRouting};

    fn revision(description: &str) -> ScopeRevision {
        ScopeRevision {
            schema: SCOPE_REVISION_SCHEMA,
            workpiece: WorkpieceId(String::from("issue-5995")),
            predecessor: None,
            problem: String::from("the lane reads a title and nothing else"),
            design: String::from("render each section from its own field"),
            plan: String::from("1. render\n2. refuse an empty section at the door"),
            declared_surface: vec![String::from("crates/aether-bloomery/**")],
            dogfood_brief: String::from("seal a member and read its prompt"),
            routing: ScopeRouting { size: String::from("M"), model: String::from("construct: test") },
            dependencies: Vec::new(),
            description: description.to_string(),
            implements: Vec::new(),
            declared_crates: Vec::new(),
            declared_reads: Vec::new(),
        }
    }

    /// The byte offset of `needle` in `haystack`, or a panic naming the order
    /// that was missing it.
    fn at(haystack: &str, needle: &str) -> usize {
        haystack.find(needle).unwrap_or_else(|| panic!("the work order states {needle:?}:\n{haystack}"))
    }

    // Tripwire: every typed section reaches the lane, in the order a reader
    // works through them. The pre-fix renderer returned `description` verbatim,
    // so a commission whose summary was a one-line title dispatched a lane that
    // had the title and none of the four sections — which is how six members
    // were built from titles on 2026-09-14.
    #[test]
    fn every_typed_section_renders_in_order_and_the_description_is_only_the_heading() {
        let revision = revision("thread the typed sections into the lane prompt");
        let order = revision.render();

        assert!(
            order.starts_with("# thread the typed sections into the lane prompt\n\n"),
            "the one-line summary is the heading: {order}"
        );
        assert_eq!(
            order.matches("thread the typed sections into the lane prompt").count(),
            1,
            "the summary appears as the heading and nowhere else: {order}"
        );

        for (section, body) in [
            (PROBLEM, revision.problem.as_str()),
            (DESIGN, revision.design.as_str()),
            (PLAN, revision.plan.as_str()),
            (DOGFOOD, revision.dogfood_brief.as_str()),
        ] {
            assert!(order.contains(body), "## {section} carries its field verbatim: {order}");
        }
        assert!(order.contains("crates/aether-bloomery/**"), "the declared surface reaches the lane: {order}");

        let order = order.as_str();
        let positions =
            [at(order, PROBLEM), at(order, DESIGN), at(order, PLAN), at(order, SURFACE), at(order, DOGFOOD)];
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "problem, design, plan, surface, dogfood in that order: {order}"
        );
    }

    // Tripwire: the markdown parse path stores the whole rendered work order in
    // `description` so the next edit starts from the bytes an approval covers.
    // A renderer that treated that body as a heading would open every such
    // order with a `# ## Problem statement` line.
    #[test]
    fn a_description_holding_a_body_renders_no_heading() {
        let carried = revision("");
        let order = revision(&carried.render()).render();

        assert_eq!(order, carried.render(), "a body-valued description changes nothing the fields render");
    }
}
