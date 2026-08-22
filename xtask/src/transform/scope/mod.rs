//! The `scope.fill` lane (ADR-0208): assemble the prompt from the lane's
//! in-repo instruction source, run the resolved harness, replay the setter
//! call log through [`aether_bloomery::WorkpieceBuilder`], derive inverse-search, verify the
//! workpiece against its own declared surface, and stamp a review-shaped
//! evidence envelope.

use std::env;
use std::fs;
use std::path::{Path, PathBuf, absolute};

use aether_bloomery::{
    FieldKind, LANE_WORKPIECE_HEADER, NamedPath, NamedSymbol, PathOrigin, SCOPE_FILL_COMMAND, SCOPE_VERIFY_SCHEMA,
    ScopeRouting, ScopeVerifyInput, ScopeVerifyReport, WorkpieceId, WorkpieceRefusal, encode_hex, split_lane_identity,
    verify_scope,
};
use anyhow::Result;
use serde_json::{Value, json};

use crate::scope::{load, replay, winning_texts};
use crate::symbols::references::{self, ReferenceSearch, Role};
use crate::transform::claude::assemble_construct_prompt;
use crate::transform::{LaneRun, Measurements, TransformArgs, run_model_lane, write_evidence_json};

/// The lane-owned in-repo instruction source. Embedded at build time so the
/// scoping lane owns its process natively — the prompt is assembled from this
/// text, never from `.claude/skills/scope` in the worker's checkout.
pub(super) const SCOPE_INSTRUCTIONS: &str = include_str!("scope_instructions.md");

/// The three-valued status the local backend already knows how to read, matching
/// the review lane's contract.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ScopeStatus {
    Pass,
    Fail,
    Environment,
}

fn status_token(status: ScopeStatus) -> &'static str {
    match status {
        ScopeStatus::Pass => "pass",
        ScopeStatus::Fail => "fail",
        ScopeStatus::Environment => "environment",
    }
}

/// Pass when the verify report refuses nothing. Advisory buckets — including
/// `resolved_outside` — must not map to fail.
fn status_from_report(report: &ScopeVerifyReport) -> ScopeStatus {
    if report.refused() {
        ScopeStatus::Fail
    } else {
        ScopeStatus::Pass
    }
}

fn findings_from_report(report: &ScopeVerifyReport) -> Option<String> {
    report.refused().then(|| report.refusal_paths().join("\n"))
}

/// Stamp the broker-matched `nonce` and the lane's terminal status onto the
/// result `record`. A run with no findings stamps no findings key rather than
/// an empty string.
fn stamp_scope_evidence(
    nonce: Option<&str>,
    status: ScopeStatus,
    findings: Option<String>,
    record: &Value,
    measured: Measurements,
) -> Value {
    let mut evidence = json!({
        "command": SCOPE_FILL_COMMAND,
        "nonce": nonce,
        "status": status_token(status),
        "result_record": record,
    });
    if let Some(findings) = findings
        && let Some(object) = evidence.as_object_mut()
    {
        object.insert("findings".to_owned(), Value::String(findings));
    }
    measured.stamp(&mut evidence);
    evidence
}

fn assemble_scope_prompt(subject: Option<&str>, task: Option<&str>, run_dir: &Path, setter: &str) -> String {
    let mut prompt = assemble_construct_prompt(SCOPE_INSTRUCTIONS, subject, task, None);
    prompt.push_str(&emission_section(run_dir, setter));
    prompt
}

fn emission_section(run_dir: &Path, setter: &str) -> String {
    let run = run_dir.display();
    format!(
        "\n## Emission\n\n\
         This run's directory is `{run}`. The setter binary is `{setter}`.\n\n\
         Fill each authored field by invoking the setter as its own process, value by file — never \
         as a `--value` argv scalar:\n\n\
         ```\n\
         cargo xtask scope set <field> --run {run} --value-file <path>\n\
         ```\n\n\
         `--value-file -` reads stdin. `<field>` is one of: problem, evidence, success, approach, \
         rejected-option, plan-step, acceptance, declared-surface, edge, routing-hint.\n\n\
         `inverse-search` and `implements` are derived; do not set them. Write no source, open \
         nothing, and stop when the authored fields are written.\n"
    )
}

fn setter_binary() -> String {
    env::current_exe().ok().map_or_else(|| "cargo xtask".to_owned(), |path| path.display().to_string())
}

fn run_directory(out: &Path) -> PathBuf {
    let _ = fs::create_dir_all(out);
    out.canonicalize().unwrap_or_else(|_| absolute(out).unwrap_or_else(|_| out.to_path_buf()))
}

/// The `scope.fill` lane: assemble the prompt with the shared cached prefix,
/// run the resolved harness, replay the call log, and stamp evidence.
pub(super) fn run_scope(args: &TransformArgs) -> Result<()> {
    let run_dir = run_directory(&args.out);
    let prompt = assemble_scope_prompt(args.subject.as_deref(), args.task.as_deref(), &run_dir, &setter_binary());
    let run = run_model_lane(&prompt, args)?;
    write_evidence_json(&args.out, &finalize(args, &run_dir, run))
}

fn finalize(args: &TransformArgs, run_dir: &Path, run: LaneRun) -> Value {
    let LaneRun { record, measured } = run;
    let calls = match load(run_dir) {
        Ok(calls) => calls,
        Err(error) => {
            return stamp_scope_evidence(
                args.nonce.as_deref(),
                ScopeStatus::Environment,
                Some(format!("the scoping lane could not read the call log: {error:#}")),
                &record,
                measured,
            );
        }
    };

    let workpiece =
        workpiece_from_task(args.task.as_deref()).unwrap_or_else(|| WorkpieceId(String::from("unspecified")));
    let builder = replay(workpiece, &calls);
    let surface = owned(winning_texts(&calls, FieldKind::DeclaredSurface));
    let steps = owned(winning_texts(&calls, FieldKind::PlanStep));

    let rev = args.subject.as_deref().unwrap_or("HEAD");
    let verify_input = match project_verify_input(&steps, &surface, rev) {
        Ok(input) => input,
        Err(error) => {
            return stamp_scope_evidence(
                args.nonce.as_deref(),
                ScopeStatus::Environment,
                Some(format!("the scoping lane could not read the subject tree: {error:#}")),
                &record,
                measured,
            );
        }
    };
    let report = verify_scope(&verify_input);

    match builder.finish(None, ScopeRouting { size: String::new(), model: String::new() }) {
        Err(refusal) => stamp_scope_evidence(
            args.nonce.as_deref(),
            ScopeStatus::Fail,
            Some(unfillable_findings(&refusal)),
            &bind_result(record, None, Some(&verify_input)),
            measured,
        ),
        Ok(revision) => {
            let status = status_from_report(&report);
            stamp_scope_evidence(
                args.nonce.as_deref(),
                status,
                findings_from_report(&report),
                &bind_result(record, Some(&revision.to_canonical()), Some(&verify_input)),
                measured,
            )
        }
    }
}

fn bind_result(mut record: Value, revision: Option<&[u8]>, verify_input: Option<&ScopeVerifyInput>) -> Value {
    if let Some(object) = record.as_object_mut() {
        if let Some(revision) = revision {
            object.insert("revision".to_owned(), json!(encode_hex(revision)));
        }
        if let Some(input) = verify_input {
            object.insert("verify_input".to_owned(), json!(encode_hex(&input.to_canonical())));
        }
    }
    record
}

fn workpiece_from_task(task: Option<&str>) -> Option<WorkpieceId> {
    let (_, identity) = split_lane_identity(task?);
    let id = identity?.strip_prefix(LANE_WORKPIECE_HEADER)?.trim();
    (!id.is_empty()).then(|| WorkpieceId(String::from(id)))
}

fn unfillable_findings(refusal: &WorkpieceRefusal) -> String {
    match refusal {
        WorkpieceRefusal::NoPlanStep { .. } => {
            String::from("the sketch carries no plan step; it cannot be filled as written.")
        }
        WorkpieceRefusal::EmptyDeclaredSurface { .. } => {
            String::from("the sketch declares no surface; it cannot be filled as written.")
        }
        WorkpieceRefusal::InvalidSurface { slot, text } => {
            format!("declared-surface slot {slot} is outside the surface grammar: {text}")
        }
        WorkpieceRefusal::BlankEdge { slot, .. } => {
            format!("edge slot {slot} is a blank workpiece id; it cannot be filled as written.")
        }
        WorkpieceRefusal::MissingProblem { .. } => {
            String::from("the sketch carries no problem statement; it cannot be filled as written.")
        }
        WorkpieceRefusal::BlankProblem { .. } => {
            String::from("the problem statement is blank; it cannot be filled as written.")
        }
    }
}

fn project_verify_input(steps: &[String], surface: &[String], rev: &str) -> Result<ScopeVerifyInput> {
    let mut named_paths = named_paths_from_plan(steps);
    let mut named_symbols = Vec::new();
    for symbol in symbols_from_plan(steps) {
        match references::search(&symbol, rev, surface)? {
            ReferenceSearch::Unresolvable { symbol, .. } => {
                named_symbols.push(NamedSymbol { symbol, definitions: Vec::new() });
            }
            ReferenceSearch::Resolved(resolved) => {
                let mut definitions = Vec::new();
                for path in resolved.paths {
                    if path.role == Role::Defining {
                        named_paths.push(NamedPath {
                            path: path.path.clone(),
                            origin: PathOrigin::InverseSearch { symbol: symbol.clone() },
                        });
                        definitions.push(path.path);
                    }
                }
                named_symbols.push(NamedSymbol { symbol, definitions });
            }
        }
    }
    Ok(ScopeVerifyInput { schema: SCOPE_VERIFY_SCHEMA, named_paths, named_symbols, declared_surface: surface.to_vec() })
}

fn named_paths_from_plan(steps: &[String]) -> Vec<NamedPath> {
    let mut named = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        let number = u32::try_from(index.saturating_add(1)).unwrap_or(u32::MAX);
        for path in extract_paths(step) {
            named.push(NamedPath { path, origin: PathOrigin::PlanStep { step: number } });
        }
    }
    named
}

fn symbols_from_plan(steps: &[String]) -> Vec<String> {
    let mut symbols = Vec::new();
    for step in steps {
        for span in backtick_spans(step) {
            let symbol = span.rsplit("::").next().unwrap_or(span);
            if is_identifier(symbol) && !symbols.iter().any(|existing| existing == symbol) {
                symbols.push(symbol.to_owned());
            }
        }
    }
    symbols
}

fn extract_paths(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for candidate in backtick_spans(text).into_iter().chain(bare_tokens(text)) {
        let Some(path) = as_repo_path(candidate) else {
            continue;
        };
        if !paths.iter().any(|existing| existing == &path) {
            paths.push(path);
        }
    }
    paths
}

fn as_repo_path(token: &str) -> Option<String> {
    let token = token.trim();
    let token = token.strip_suffix("(create)").map_or(token, str::trim);
    let token = token.trim_end_matches(['.', ',', ';', ':', ')']);
    if token.contains("://") || token.starts_with('/') || token.contains('\\') {
        return None;
    }
    if !token.contains('/') {
        return None;
    }
    if token.chars().any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '*'))) {
        return None;
    }
    Some(token.to_owned())
}

fn backtick_spans(text: &str) -> Vec<&str> {
    let mut spans = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find('`') else {
            break;
        };
        spans.push(&rest[..end]);
        rest = &rest[end + 1..];
    }
    spans
}

fn bare_tokens(text: &str) -> Vec<&str> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '*')))
        .filter(|token| !token.is_empty())
        .collect()
}

fn is_identifier(symbol: &str) -> bool {
    !symbol.is_empty()
        && !symbol.starts_with(|first: char| first.is_ascii_digit())
        && symbol.chars().all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn owned(texts: Vec<&str>) -> Vec<String> {
    texts.into_iter().map(String::from).collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::{assemble_scope_prompt, findings_from_report, stamp_scope_evidence, status_from_report};
    use crate::transform::Measurements;
    use crate::transform::claude::assemble_construct_prompt;
    use crate::transform::construct::CONSTRUCT_INSTRUCTIONS;
    use aether_bloomery::{NamedPath, NamedSymbol, PathOrigin, SCOPE_VERIFY_SCHEMA, ScopeVerifyReport};
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn the_scope_prompt_shares_the_cached_prefix() {
        // Tripwire: a lane that opens with its own instructions forfeits the
        // shared prompt cache on every dispatch. Conventions lead, so a scope
        // prompt and a construct prompt share the lane context byte-for-byte,
        // and the per-run Emission directory sits only after that prefix.
        let subject = Some("abc123");
        let task = Some("shared order");
        let construct = assemble_construct_prompt(CONSTRUCT_INSTRUCTIONS, subject, task, None);
        let run = Path::new("/run/scope-nonce-test");
        let scope = assemble_scope_prompt(subject, task, run, "cargo xtask");
        let prefix_len = construct.bytes().zip(scope.bytes()).take_while(|(a, b)| a == b).count();
        assert!(scope.starts_with("## Conventions\n"), "lane context leads the prompt");
        assert!(
            scope[..prefix_len].contains("Tests must earn their place"),
            "the shared conventions must sit inside the common prefix",
        );
        let emission_at = scope.find("\n## Emission\n").expect("the scope prompt names the emission section");
        assert!(emission_at >= prefix_len, "the per-run emission section must sit after the shared prefix");
        assert!(
            scope[emission_at..].contains("/run/scope-nonce-test"),
            "the emission section names this run's directory",
        );
        assert!(
            !scope[..prefix_len].contains("/run/scope-nonce-test"),
            "a run-varying path inside the cached bulk forfeits the cache on every dispatch",
        );
        assert!(scope.contains("cargo xtask scope set"), "the emission section names the setter invocation");
    }

    #[test]
    fn an_advisory_resolution_outside_the_surface_is_not_a_refusal() {
        // Tripwire: reading an advisory bucket as a refusal drives every
        // declared surface toward the reverse-dependency closure, which is the
        // failure ADR-0208 rejects an all-refusing verify to avoid.
        let record = json!({"schema": 1});
        let advisory = ScopeVerifyReport {
            schema: SCOPE_VERIFY_SCHEMA,
            uncovered: Vec::new(),
            resolved_inside: Vec::new(),
            unresolvable: Vec::new(),
            resolved_outside: vec![NamedSymbol {
                symbol: String::from("apply_containment"),
                definitions: vec![String::from("crates/aether-chassis-bloomery/src/bloomery/verify/containment.rs")],
            }],
            checked: 0,
        };
        let passed = stamp_scope_evidence(
            Some("n-1"),
            status_from_report(&advisory),
            findings_from_report(&advisory),
            &record,
            Measurements::default(),
        );
        assert_eq!(passed["status"], "pass");
        assert!(passed.get("findings").is_none(), "a run with no findings stamps no findings key");

        let refused = ScopeVerifyReport {
            schema: SCOPE_VERIFY_SCHEMA,
            uncovered: vec![NamedPath {
                path: String::from("crates/aether-chassis-bloomery/src/api/runtime/seal.rs"),
                origin: PathOrigin::PlanStep { step: 2 },
            }],
            resolved_inside: Vec::new(),
            unresolvable: Vec::new(),
            resolved_outside: Vec::new(),
            checked: 1,
        };
        let failed = stamp_scope_evidence(
            Some("n-2"),
            status_from_report(&refused),
            findings_from_report(&refused),
            &record,
            Measurements::default(),
        );
        assert_eq!(failed["status"], "fail");
        let findings = failed["findings"].as_str().expect("a refusal stamps its own text as findings");
        assert!(findings.contains("plan step 2"), "{findings}");
        assert!(findings.contains("crates/aether-chassis-bloomery/src/api/runtime/seal.rs"), "{findings}");
    }
}
