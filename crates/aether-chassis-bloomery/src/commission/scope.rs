//! Parse a managed-heading markdown file into a canonical [`ScopeRevision`].

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use aether_bloomery::{Digest, SCOPE_REVISION_SCHEMA, ScopeRevision, ScopeRouting, SurfacePattern, WorkpieceId};
use anyhow::{Result, anyhow, bail};

const PROBLEM: &str = "Problem statement";
const DESIGN: &str = "Design notes";
const PLAN: &str = "Implementation plan";
const DEPENDS: &str = "Depends on";
const SURFACE: &str = "Declared surface";
const DOGFOOD: &str = "Dogfood brief";

const MANAGED: &[&str] = &[PROBLEM, DESIGN, PLAN, "Sub-issues", DEPENDS, SURFACE, DOGFOOD, "Side findings"];

/// Render `markdown` as the next scope revision for `workpiece`.
pub fn parse_revision(workpiece: &str, markdown: &str, predecessor: Option<Digest>) -> Result<ScopeRevision> {
    let sections = managed_sections(markdown)?;
    let problem = required_body(&sections, PROBLEM)?;
    let design = required_body(&sections, DESIGN)?;
    let (plan, routing) = plan_and_routing(required_span(&sections, PLAN)?)?;
    let declared_surface = parse_surface(required_span(&sections, SURFACE)?)?;
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
    };
    Ok(ScopeRevision { description: render_work_order(&revision), ..revision })
}

/// Work-order text the seal persists for construct.
///
/// A stored advisory description wins when the operator put one on the
/// revision. Otherwise the signed managed headings are rendered. A GitHub
/// issue body is never an input.
pub fn task_text(revision: &ScopeRevision) -> String {
    if !revision.description.trim().is_empty() {
        return revision.description.clone();
    }
    render_work_order(revision)
}

fn render_work_order(revision: &ScopeRevision) -> String {
    let mut out = String::new();
    push_section(&mut out, PROBLEM, &revision.problem);
    push_section(&mut out, DESIGN, &revision.design);
    out.push_str("## ");
    out.push_str(PLAN);
    out.push_str("\n\n");
    out.push_str(revision.plan.trim());
    out.push_str("\n\n**Size:** ");
    out.push_str(&revision.routing.size);
    out.push_str("\n**Implementation model:** ");
    out.push_str(&revision.routing.model);
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
    out.push_str("\n## ");
    out.push_str(SURFACE);
    out.push_str("\n\n");
    for glob in &revision.declared_surface {
        out.push_str(glob);
        out.push('\n');
    }
    if !revision.dogfood_brief.trim().is_empty() {
        out.push('\n');
        push_section(&mut out, DOGFOOD, &revision.dogfood_brief);
    }
    out
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

    for required in [PROBLEM, DESIGN, PLAN, SURFACE] {
        if headings.iter().all(|(_, name)| name != required) {
            bail!("missing required managed heading: ## {required}");
        }
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
    if !reason_line.starts_with("**Routing reason:**") {
        bail!("routing lines must be the final three non-empty Implementation plan lines");
    }
    let size = labeled(size_line, "**Size:**")?;
    let model = labeled(model_line, "**Implementation model:**")?;
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
    let body = body_of(span);
    let mut globs = Vec::new();
    let mut in_fence = false;
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        let entry = trimmed.strip_prefix("- ").unwrap_or(trimmed);
        if entry.is_empty() {
            continue;
        }
        if SurfacePattern::parse(entry).is_none() {
            bail!("declared surface {entry:?} is outside the surface grammar");
        }
        globs.push(entry.to_owned());
    }
    if globs.is_empty() {
        bail!("declared surface lists no paths");
    }
    Ok(globs)
}

fn parse_workpieces(span: &str) -> Vec<WorkpieceId> {
    body_of(span)
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            let entry = trimmed.strip_prefix("- ").unwrap_or(trimmed);
            if entry.is_empty() || entry.starts_with("N/A") {
                return None;
            }
            Some(WorkpieceId(entry.to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{parse_revision, task_text};

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
}
