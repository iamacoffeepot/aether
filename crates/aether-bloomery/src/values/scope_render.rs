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
    /// A stored advisory description wins when the operator put one on the
    /// revision. Otherwise the signed managed headings are rendered. A GitHub
    /// issue body is never an input.
    ///
    /// The surface declaration is the exception: it is always rendered from
    /// this revision's own fields, over whatever block the stored description
    /// carries. [`Self::declared_surface`] is what the seal door and the
    /// containment gate read, so it is the authority and the block is its
    /// rendering. An operator answering a parked surface request writes the
    /// successor as the current revision with a widened field and every other
    /// field — the description included — carried unchanged
    /// ([`Self::with_widened_surface`]). A renderer that echoed a description
    /// frozen one revision ago would hand the re-dispatched lane the exact
    /// surface it had just declined against.
    #[must_use]
    pub fn render(&self) -> String {
        if self.description.trim().is_empty() {
            return self.render_fields();
        }
        retarget_declaration(&self.description, self)
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

/// `body` with its managed surface-declaration blocks replaced by the ones
/// `revision` renders to, spliced in where the first of them stood.
///
/// A body that declares no surface at all gets the declaration appended, which
/// is the honest rendering of a revision whose field says something the text
/// never did.
fn retarget_declaration(body: &str, revision: &ScopeRevision) -> String {
    let mut declaration = String::new();
    push_declaration(&mut declaration, revision);

    let mut out = String::with_capacity(body.len() + declaration.len());
    let mut spliced = false;
    let mut dropping = false;
    for line in body.split_inclusive('\n') {
        if let Some(name) = line.trim_end_matches(['\n', '\r']).strip_prefix("## ") {
            dropping = matches!(name, SURFACE | CRATES | PROTECTED);
            if dropping && !spliced {
                splice(&mut out, &declaration);
                spliced = true;
            }
        }
        if !dropping {
            out.push_str(line);
        }
    }
    if !spliced {
        splice(&mut out, &declaration);
    }
    out
}

/// Append `declaration` to `out` with exactly one blank line before it.
///
/// `declaration` opens with the newline [`push_list`] emits, so what varies is
/// how much whitespace the text it lands after already ended with.
fn splice(out: &mut String, declaration: &str) {
    if out.is_empty() {
        out.push_str(declaration.trim_start_matches('\n'));
        return;
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(declaration);
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
