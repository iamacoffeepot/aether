//! Parse a managed-heading markdown file into a canonical [`ScopeRevision`].

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::Path;

use aether_bloomery::{Digest, SCOPE_REVISION_SCHEMA, ScopeRevision, ScopeRouting, SurfacePattern, WorkpieceId};
use anyhow::{Result, anyhow, bail};

use super::crates::{WorkspaceCrates, derive_surface};

const PROBLEM: &str = "Problem statement";
const DESIGN: &str = "Design notes";
const PLAN: &str = "Implementation plan";
const DEPENDS: &str = "Depends on";
const SURFACE: &str = "Declared surface";
const CRATES: &str = "Declared crates";
const PROTECTED: &str = "Protected files";
const READS: &str = "Reads";
const DOGFOOD: &str = "Dogfood brief";

const MANAGED: &[&str] =
    &[PROBLEM, DESIGN, PLAN, "Sub-issues", DEPENDS, SURFACE, CRATES, PROTECTED, READS, DOGFOOD, "Side findings"];

/// The three labels that close `## Implementation plan`, shared by the parser
/// that requires them and the renderer that writes them.
///
/// Stated once because they are the one place the two sides must agree
/// character for character: a renderer that stopped one label short of what the
/// parser demands wrote every revision a description its own parser refused.
const SIZE_LABEL: &str = "**Size:**";
const MODEL_LABEL: &str = "**Implementation model:**";
const REASON_LABEL: &str = "**Routing reason:**";

/// What the renderer writes on the reason line.
///
/// [`ScopeRouting`] stores size and model and discards the reason the parser
/// validated, so a re-render has no authored reason to restate and says so
/// rather than inventing one.
const RERENDERED_REASON: &str = "re-rendered from the stored revision, which carries no authored reason";

/// Render `markdown` as the next scope revision for `workpiece`.
///
/// The surface arrives one of two ways. `## Declared crates` names the crates
/// the work is about and the surface is *derived* — those crates, every
/// workspace crate that depends on them, the shared roots, and whatever
/// `## Protected files` names. `## Declared surface` states the globs
/// literally, and is what every scope written before the crate block used.
/// Exactly one of the two, because a scope that carried both would leave the
/// tier resolver with two different answers about what the work intends.
pub fn parse_revision(workpiece: &str, markdown: &str, predecessor: Option<Digest>) -> Result<ScopeRevision> {
    let sections = managed_sections(markdown)?;
    let problem = required_body(&sections, PROBLEM)?;
    let design = required_body(&sections, DESIGN)?;
    let (plan, routing) = plan_and_routing(required_span(&sections, PLAN)?)?;
    let (declared_crates, declared_surface) = parse_declaration(&sections)?;
    let declared_reads = sections.get(READS).map(|span| parse_crates(READS, span)).transpose()?.unwrap_or_default();
    let dogfood_brief = sections.get(DOGFOOD).map(|span| body_of(span)).unwrap_or_default();
    let dependencies = sections.get(DEPENDS).map(|span| parse_workpieces(span)).unwrap_or_default();

    let revision = ScopeRevision {
        schema: SCOPE_REVISION_SCHEMA,
        workpiece: WorkpieceId(workpiece.to_owned()),
        predecessor,
        problem,
        design,
        plan,
        declared_surface,
        dogfood_brief,
        routing,
        dependencies,
        description: String::new(),
        implements: Vec::new(),
        declared_crates,
        declared_reads,
    };
    Ok(ScopeRevision { description: render_work_order(&revision), ..revision })
}

/// The declared crates and the surface they resolve to.
///
/// The crate block wins when present, and the two blocks together are a
/// refusal rather than a merge: a scope that says both is a scope whose author
/// does not yet know which one the gate will read.
fn parse_declaration(sections: &BTreeMap<String, String>) -> Result<(Vec<String>, Vec<String>)> {
    let crates = sections.get(CRATES).map(|span| parse_crates(CRATES, span)).transpose()?;
    let protected = sections.get(PROTECTED).map(|span| parse_protected(span)).transpose()?.unwrap_or_default();

    match (crates, sections.get(SURFACE)) {
        (Some(_), Some(_)) => bail!("declare either ## {CRATES} or ## {SURFACE}, not both"),
        (Some(crates), None) => {
            let root = WorkspaceCrates::find_root(&env::current_dir()?)?;
            let surface = derive_surface(&WorkspaceCrates::load(&root)?, &crates, &protected)?;
            Ok((crates, surface))
        }
        (None, Some(span)) => {
            if !protected.is_empty() {
                bail!("## {PROTECTED} names files for a ## {CRATES} declaration; a ## {SURFACE} block lists its own");
            }
            Ok((Vec::new(), parse_surface(span)?))
        }
        (None, None) => bail!("missing required managed heading: ## {SURFACE} or ## {CRATES}"),
    }
}

/// Work-order text the seal persists for construct.
///
/// A stored advisory description wins when the operator put one on the
/// revision. Otherwise the signed managed headings are rendered. A GitHub
/// issue body is never an input.
///
/// The surface declaration is the exception: it is always rendered from the
/// revision's own fields, over whatever block the stored description carries.
/// [`declared_surface`](ScopeRevision::declared_surface) is what the seal door
/// and the containment gate read, so it is the authority and the block is its
/// rendering. An operator answering a parked surface request writes the
/// successor as the current revision with a widened field and every other field
/// — the description included — carried unchanged
/// ([`with_widened_surface`](ScopeRevision::with_widened_surface), the shape
/// `cargo xtask bloom amend` re-pins the member at). A renderer that echoed a
/// description frozen one revision ago would hand the re-dispatched lane the
/// exact surface it had just declined against.
#[must_use]
pub fn task_text(revision: &ScopeRevision) -> String {
    if revision.description.trim().is_empty() {
        return render_work_order(revision);
    }
    retarget_declaration(&revision.description, revision)
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

fn render_work_order(revision: &ScopeRevision) -> String {
    let mut out = String::new();
    push_section(&mut out, PROBLEM, &revision.problem);
    push_section(&mut out, DESIGN, &revision.design);
    out.push_str("## ");
    out.push_str(PLAN);
    out.push_str("\n\n");
    out.push_str(revision.plan.trim());
    out.push_str("\n\n");
    out.push_str(SIZE_LABEL);
    out.push(' ');
    out.push_str(&revision.routing.size);
    out.push('\n');
    out.push_str(MODEL_LABEL);
    out.push(' ');
    out.push_str(&revision.routing.model);
    out.push('\n');
    out.push_str(REASON_LABEL);
    out.push(' ');
    out.push_str(RERENDERED_REASON);
    out.push('\n');
    if !revision.dependencies.is_empty() {
        out.push_str("\n## ");
        out.push_str(DEPENDS);
        out.push_str("\n\n");
        for dep in &revision.dependencies {
            out.push_str("- ");
            out.push_str(&dep.0);
            out.push('\n');
        }
    }
    push_declaration(&mut out, revision);
    if !revision.declared_reads.is_empty() {
        push_list(&mut out, READS, &revision.declared_reads);
    }
    if !revision.dogfood_brief.trim().is_empty() {
        out.push('\n');
        push_section(&mut out, DOGFOOD, &revision.dogfood_brief);
    }
    out
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

/// Load `path` and parse it as a revision for `workpiece`.
pub(super) fn load_revision(workpiece: &str, path: &Path, predecessor: Option<Digest>) -> Result<ScopeRevision> {
    let markdown = fs::read_to_string(path).map_err(|error| anyhow!("read {}: {error}", path.display()))?;
    parse_revision(workpiece, &markdown, predecessor).map_err(|error| anyhow!("{}: {error}", path.display()))
}

fn managed_sections(body: &str) -> Result<BTreeMap<String, String>> {
    let mut headings: Vec<(usize, String)> = Vec::new();
    let mut offset = 0;
    for line in body.split_inclusive('\n') {
        let text = line.trim_end_matches(['\n', '\r']);
        if let Some(name) = text.strip_prefix("## ")
            && MANAGED.contains(&name)
        {
            if headings.iter().any(|(_, existing)| existing == name) {
                bail!("duplicate managed heading: ## {name}");
            }
            headings.push((offset, name.to_owned()));
        }
        offset += line.len();
    }

    for required in [PROBLEM, DESIGN, PLAN] {
        if headings.iter().all(|(_, name)| name != required) {
            bail!("missing required managed heading: ## {required}");
        }
    }
    // The surface declaration is required too, but which heading carries it is
    // the scope's choice, so the pair is checked where it is read rather than
    // by naming one of them here.
    if headings.iter().all(|(_, name)| name != SURFACE && name != CRATES) {
        bail!("missing required managed heading: ## {SURFACE} or ## {CRATES}");
    }

    let mut sections = BTreeMap::new();
    for (index, (start, name)) in headings.iter().enumerate() {
        let end = headings.get(index + 1).map_or(body.len(), |(next, _)| *next);
        let mut span = &body[*start..end];
        if index + 1 < headings.len() && span.ends_with("\r\n\r\n") {
            span = &span[..span.len() - 2];
        } else if index + 1 < headings.len() && span.ends_with("\n\n") {
            span = &span[..span.len() - 1];
        }
        sections.insert(name.clone(), span.to_owned());
    }
    Ok(sections)
}

fn required_span<'a>(sections: &'a BTreeMap<String, String>, name: &str) -> Result<&'a str> {
    match sections.get(name) {
        Some(span) => Ok(span),
        None => bail!("missing required managed heading: ## {name}"),
    }
}

fn required_body(sections: &BTreeMap<String, String>, name: &str) -> Result<String> {
    let body = body_of(required_span(sections, name)?);
    if body.is_empty() {
        bail!("empty required managed section: ## {name}");
    }
    Ok(body)
}

fn body_of(span: &str) -> String {
    let rest = match span.split_once('\n') {
        Some((_, rest)) => rest,
        None => "",
    };
    rest.trim().to_owned()
}

fn plan_and_routing(span: &str) -> Result<(String, ScopeRouting)> {
    let body = body_of(span);
    let mut non_empty: Vec<&str> = body.lines().filter(|line| !line.trim().is_empty()).collect();
    if non_empty.len() < 3 {
        bail!("implementation plan must end with Size, Implementation model, and Routing reason lines");
    }
    let reason = non_empty.pop();
    let model_line = non_empty.pop();
    let size_line = non_empty.pop();
    let (Some(size_line), Some(model_line), Some(reason_line)) = (size_line, model_line, reason) else {
        bail!("implementation plan must end with Size, Implementation model, and Routing reason lines");
    };
    if !reason_line.starts_with(REASON_LABEL) {
        bail!("routing lines must be the final three non-empty Implementation plan lines");
    }
    let size = labeled(size_line, SIZE_LABEL)?;
    let model = labeled(model_line, MODEL_LABEL)?;
    if size.is_empty() || model.is_empty() {
        bail!("Size and Implementation model routing lines must be non-empty");
    }

    let mut plan_lines: Vec<&str> = body.lines().collect();
    while plan_lines.last().is_some_and(|line| line.trim().is_empty()) {
        plan_lines.pop();
    }
    if plan_lines.len() >= 3 {
        plan_lines.truncate(plan_lines.len() - 3);
    }
    let plan = plan_lines.join("\n").trim().to_owned();
    if plan.is_empty() {
        bail!("empty required managed section: ## {PLAN}");
    }
    Ok((plan, ScopeRouting { size, model }))
}

fn labeled(line: &str, prefix: &str) -> Result<String> {
    match line.trim().strip_prefix(prefix) {
        Some(value) => Ok(value.trim().to_owned()),
        None => bail!("expected {prefix} routing line, found {line}"),
    }
}

fn parse_surface(span: &str) -> Result<Vec<String>> {
    let globs = list_entries(span);
    if let Some(bad) = globs.iter().find(|entry| SurfacePattern::parse(entry).is_none()) {
        bail!("declared surface {bad:?} is outside the surface grammar");
    }
    if globs.is_empty() {
        bail!("declared surface lists no paths");
    }
    Ok(globs)
}

/// The crate names a `## Declared crates` block lists, in declaration order.
fn parse_crates(heading: &str, span: &str) -> Result<Vec<String>> {
    let names = list_entries(span);
    if names.is_empty() {
        bail!("## {heading} lists no crate names");
    }
    if let Some(bad) = names.iter().find(|name| name.contains('/') || name.contains('*')) {
        bail!("## {heading} entry {bad:?} is a path, not a crate name");
    }
    Ok(names)
}

/// The literal paths a `## Protected files` block names.
///
/// Each must be a concrete path inside the declared-surface grammar: this block
/// exists so a scope can say "this work touches a file the policy guards", and
/// a glob here would claim a whole subtree at the guarded file's tier.
fn parse_protected(span: &str) -> Result<Vec<String>> {
    let paths = list_entries(span);
    for path in &paths {
        match SurfacePattern::parse(path) {
            Some(SurfacePattern::Exact(_)) => {}
            Some(SurfacePattern::Subtree(_)) => bail!("protected file {path:?} is a subtree glob, not a file"),
            None => bail!("protected file {path:?} is outside the surface grammar"),
        }
    }
    Ok(paths)
}

/// The non-empty, un-fenced, bullet-stripped lines of a managed list block.
fn list_entries(span: &str) -> Vec<String> {
    let mut entries = Vec::new();
    let mut in_fence = false;
    for line in body_of(span).lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        let entry = trimmed.strip_prefix("- ").unwrap_or(trimmed);
        if !entry.is_empty() {
            entries.push(entry.to_owned());
        }
    }
    entries
}

fn parse_workpieces(span: &str) -> Vec<WorkpieceId> {
    body_of(span)
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            let entry = trimmed.strip_prefix("- ").unwrap_or(trimmed);
            let token = entry.split_whitespace().next()?;
            let sentinel = token.strip_suffix('.').unwrap_or(token).to_ascii_lowercase();
            if sentinel == "n/a" || sentinel == "none" {
                return None;
            }
            Some(WorkpieceId(entry.to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use aether_bloomery::WorkpieceId;

    use super::{parse_revision, parse_workpieces, render_work_order, task_text};

    fn fixture() -> &'static str {
        "\
## Problem statement\n\
\n\
Need a CLI.\n\
\n\
## Design notes\n\
\n\
Separate binary.\n\
\n\
## Implementation plan\n\
\n\
Ship bloomery-commission.\n\
\n\
**Size:** m\n\
**Implementation model:** sonnet\n\
**Routing reason:** focused CLI\n\
\n\
## Declared surface\n\
\n\
```text\n\
crates/aether-chassis-bloomery/src/commission/**\n\
```\n\
\n\
## Dogfood brief\n\
\n\
Create then show.\n"
    }

    #[test]
    fn managed_headings_become_a_revision() {
        let revision = parse_revision("issue-5047", fixture(), None)
            .unwrap_or_else(|error| panic!("a complete managed file must parse: {error}"));
        assert_eq!(revision.workpiece.0, "issue-5047");
        assert_eq!(revision.problem, "Need a CLI.");
        assert_eq!(revision.design, "Separate binary.");
        assert_eq!(revision.plan, "Ship bloomery-commission.");
        assert_eq!(revision.routing.size, "m");
        assert_eq!(revision.routing.model, "sonnet");
        assert_eq!(revision.declared_surface, ["crates/aether-chassis-bloomery/src/commission/**"]);
        assert_eq!(revision.dogfood_brief, "Create then show.");
        assert!(revision.predecessor.is_none());
        assert!(
            revision.description.contains("Ship bloomery-commission."),
            "the verb stores the rendered work order, not an empty parallel body: {}",
            revision.description
        );

        let task = task_text(&revision);
        assert!(task.contains("## Problem statement"), "seal renders the managed heading: {task}");
        assert!(task.contains("Need a CLI."), "seal reads the commission problem, not a GitHub issue: {task}");
        assert!(task.contains("## Design notes") && task.contains("Separate binary."), "{task}");
        assert!(task.contains("**Size:** m") && task.contains("**Implementation model:** sonnet"), "{task}");
        assert!(task.contains("crates/aether-chassis-bloomery/src/commission/**"), "{task}");
    }

    #[test]
    fn a_rendered_work_order_parses_back_into_the_revision_it_came_from() {
        // Tripwire: `parse_revision` stores its own `render_work_order` output
        // as the revision's description, and that text is what the next scope
        // of this commission starts from. The two sides are computed from each
        // other, so the equality drifts the moment one grows a managed block or
        // a required line the other does not write — which is how every
        // revision written for a while stored a description its own parser
        // refused.
        let full = crate_declared()
            .replace("## Declared crates", "## Depends on\n\n- issue-5286\n\n## Declared crates")
            .replace("## Protected files", "## Reads\n\n- aether-data\n\n## Protected files");

        for (label, markdown) in
            [("surface-declared", fixture().to_owned()), ("crate-declared", crate_declared()), ("every-block", full)]
        {
            let revision = parse_revision("issue-5047", &markdown, None)
                .unwrap_or_else(|error| panic!("{label}: the fixture parses: {error}"));

            let reparsed = parse_revision("issue-5047", &render_work_order(&revision), None).unwrap_or_else(|error| {
                panic!("{label}: the rendered work order must parse: {error}\n{}", render_work_order(&revision))
            });
            assert_eq!(reparsed, revision, "{label}: the rendered work order must reproduce the revision it came from");
        }
    }

    #[test]
    fn a_widened_revision_renders_its_own_surface_over_the_body_it_carried() {
        // Tripwire: an amendment writes its successor as the current revision
        // with a widened `declared_surface` and every other field — the
        // description included — carried unchanged. A renderer that echoed that
        // description would hand the re-dispatched lane the exact surface it had
        // just declined against, and the lane would decline again.
        let revision =
            parse_revision("issue-5047", fixture(), None).unwrap_or_else(|error| panic!("the fixture parses: {error}"));
        let widened = revision.with_widened_surface(&["scripts/**".to_owned()]);
        assert!(!widened.description.contains("scripts/**"), "the amendment carries the body it inherited unchanged");

        let task = task_text(&widened);
        assert!(task.contains("scripts/**"), "the rendered order states the widened field: {task}");
        assert!(
            task.contains("crates/aether-chassis-bloomery/src/commission/**"),
            "widening keeps what the surface already carried: {task}"
        );
        assert_eq!(task.matches("## Declared surface").count(), 1, "one declaration block, not two: {task}");
        assert!(task.contains("Ship bloomery-commission."), "the rest of the order survives the splice: {task}");
        assert!(task.contains("## Dogfood brief") && task.contains("Create then show."), "{task}");
    }

    #[test]
    fn a_glob_outside_the_grammar_is_refused() {
        let markdown = fixture().replace("crates/aether-chassis-bloomery/src/commission/**", "this is not a path glob");
        match parse_revision("issue-5047", &markdown, None) {
            Ok(_) => panic!("an invalid surface glob must not become a revision"),
            Err(error) => {
                let message = error.to_string();
                assert!(
                    message.contains("outside the surface grammar"),
                    "operator-readable grammar refusal, got {message}"
                );
            }
        }
    }

    fn crate_declared() -> String {
        fixture().replace(
            "## Declared surface\n\n```text\ncrates/aether-chassis-bloomery/src/commission/**\n```\n",
            "## Declared crates\n\n- aether-math\n\n## Protected files\n\n- Cargo.lock\n",
        )
    }

    #[test]
    fn declared_crates_derive_the_surface_and_render_back_as_crates() {
        let revision = parse_revision("issue-5047", &crate_declared(), None)
            .unwrap_or_else(|error| panic!("a crate-declared file must parse: {error}"));

        assert_eq!(revision.declared_crates, ["aether-math"]);
        for expected in ["crates/aether-math/**", "xtask/**", "docs/guide/**", "Cargo.lock"] {
            assert!(
                revision.declared_surface.iter().any(|glob| glob == expected),
                "the derived surface carries {expected}: {:?}",
                revision.declared_surface
            );
        }
        assert!(
            revision.declared_surface.iter().any(|glob| glob == "crates/aether-kinds/**"),
            "the reverse-dependency closure reaches a dependent of the declared crate: {:?}",
            revision.declared_surface
        );

        // The rendered work order is what the next edit of this scope starts
        // from, so it has to say what the operator said. Re-rendering the
        // expansion would turn the block back into a hand-maintained file list.
        let task = task_text(&revision);
        assert!(task.contains("## Declared crates") && task.contains("aether-math"), "{task}");
        assert!(task.contains("## Protected files") && task.contains("Cargo.lock"), "{task}");
        assert!(!task.contains("## Declared surface"), "{task}");
    }

    #[test]
    fn a_reads_block_round_trips_and_widens_nothing() {
        // ADR-0204: a read is not authority. It must survive parse and render
        // so the door can turn it into conditional ordering, and it must leave
        // the declared surface exactly where the crate block put it — a read
        // that quietly widened the surface would be an authority grant nobody
        // asked for.
        let without = parse_revision("issue-5258", &crate_declared(), None)
            .unwrap_or_else(|error| panic!("the fixture parses: {error}"));
        let with_reads = parse_revision(
            "issue-5258",
            &crate_declared().replace("## Protected files", "## Reads\n\n- aether-data\n\n## Protected files"),
            None,
        )
        .unwrap_or_else(|error| panic!("a reads block must parse: {error}"));

        assert_eq!(with_reads.declared_reads, ["aether-data"]);
        assert_eq!(with_reads.declared_surface, without.declared_surface, "a read widens no surface");
        assert!(without.declared_reads.is_empty(), "an undeclared scope reads nothing");

        let task = task_text(&with_reads);
        assert!(task.contains("## Reads") && task.contains("aether-data"), "{task}");
    }

    #[test]
    fn a_reads_block_naming_a_path_refuses() {
        // The same guard the crate block has, and for the same reason: a path
        // here would be a file-granular forecast, which the crate blocks exist
        // to abolish. The message has to name which block, because the two
        // share a parser.
        let error = parse_revision(
            "issue-5258",
            &crate_declared()
                .replace("## Protected files", "## Reads\n\n- crates/aether-data/src/lib.rs\n\n## Protected files"),
            None,
        )
        .expect_err("a path in a reads block must refuse");

        assert!(error.to_string().contains("Reads"), "{error}");
    }

    #[test]
    fn both_declaration_blocks_together_are_refused() {
        // Two answers to "what does this work intend" would leave the tier
        // resolver picking one silently.
        let markdown = crate_declared()
            .replace("## Declared crates", "## Declared surface\n\ncrates/aether-math/**\n\n## Declared crates");
        let error = parse_revision("issue-5047", &markdown, None).expect_err("both blocks must refuse");

        assert!(error.to_string().contains("not both"), "got {error}");
    }

    #[test]
    fn a_protected_subtree_glob_is_refused() {
        // `## Protected files` is what lifts the tier, so a subtree there would
        // claim a whole directory at the guarded file's tier.
        let markdown = crate_declared().replace("- Cargo.lock", "- crates/aether-data/**");
        let error = parse_revision("issue-5047", &markdown, None).expect_err("a glob must refuse");

        assert!(error.to_string().contains("not a file"), "got {error}");
    }

    #[test]
    fn a_missing_problem_heading_is_refused() {
        match parse_revision("issue-5047", "## Design notes\n\nnotes\n", None) {
            Ok(_) => panic!("an incomplete plan must not parse"),
            Err(error) => {
                let message = error.to_string();
                assert!(message.contains("Problem statement"), "got {message}");
            }
        }
    }

    #[test]
    fn none_and_n_a_sentinels_are_absent_dependencies() {
        // The line authors write is `- None.`. A prefix-only `N/A` test
        // treated that as a workpiece id, and the first reader was seal.
        for line in ["- None.", "None", "none", "N/A", "N/A — pure umbrella; no implementation PR"] {
            let span = format!("## Depends on\n\n{line}\n");
            assert!(parse_workpieces(&span).is_empty(), "{line:?} must parse to zero dependencies");
        }

        assert_eq!(
            parse_workpieces("## Depends on\n\n- issue-5286\n"),
            [WorkpieceId("issue-5286".to_owned())],
            "a real workpiece id is still a dependency"
        );
        assert_eq!(
            parse_workpieces("## Depends on\n\n- nonexistent-thing\n"),
            [WorkpieceId("nonexistent-thing".to_owned())],
            "a token that merely begins with none is still a dependency"
        );
    }
}
