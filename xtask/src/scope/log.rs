//! Append-only setter call log for a `scope.fill` run.
//!
//! Each `scope set` is its own process and [`WorkpieceBuilder`] has no
//! rehydrate constructor, so the calls cannot mutate a live builder. They
//! append. Finalize replays the log in ordinal order through the real setters,
//! which is what reproduces generations: [`WorkpieceBuilder`] counts calls and
//! last-write-wins by maximum generation.
//!
//! The value rides as text, not a [`aether_bloomery::WorkpieceFact`] digest, because a builder
//! rehydrated from records alone can report presence but cannot resolve
//! content. Persisted bytes are `aether_data::wire`.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::Path;

use aether_bloomery::{FieldKind, WorkpieceBuilder, WorkpieceId};
use aether_data::wire::{take_from_bytes, to_vec};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// One setter invocation, in the order the model issued it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FieldCall {
    /// Which authored field this call writes.
    pub kind: FieldKind,
    /// Call order, assigned at append time.
    pub ordinal: u32,
    /// The field's authored text. A list kind groups consecutive calls of the
    /// same kind into one setter invocation so last-write-wins replaces a
    /// whole list rather than dropping every item but the last.
    pub value: String,
}

/// The log file name under the run directory.
const CALLS_FILE: &str = "calls";

/// Append one setter call to `{run}/calls`.
///
/// # Errors
/// The run directory cannot be created, the existing log cannot be decoded, or
/// the new record cannot be written.
pub fn append(run: &Path, kind: FieldKind, value: String) -> Result<()> {
    fs::create_dir_all(run).with_context(|| format!("create {}", run.display()))?;
    let path = run.join(CALLS_FILE);
    let ordinal = match fs::read(&path) {
        Ok(bytes) => u32::try_from(decode_calls(&bytes)?.len()).unwrap_or(u32::MAX),
        Err(error) if error.kind() == ErrorKind::NotFound => 0,
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let encoded = to_vec(&FieldCall { kind, ordinal, value }).context("encode a field call")?;
    let mut file =
        OpenOptions::new().create(true).append(true).open(&path).with_context(|| format!("open {}", path.display()))?;
    file.write_all(&encoded).with_context(|| format!("append {}", path.display()))?;
    Ok(())
}

/// Load the call log under `run`, in ordinal order.
///
/// A missing file is an empty log — the model wrote nothing — not an
/// environment fault. Corrupt bytes are an error the lane maps to
/// `environment`.
///
/// # Errors
/// The file exists but cannot be read or decoded.
pub fn load(run: &Path) -> Result<Vec<FieldCall>> {
    let path = run.join(CALLS_FILE);
    match fs::read(&path) {
        Ok(bytes) => decode_calls(&bytes),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

/// Replay `calls` through a fresh builder for `workpiece`.
///
/// Consecutive calls of a repeated kind become one setter invocation (one
/// generation, slot order). Consecutive restatements of a singular kind are
/// one setter call each, so last-write-wins still counts every bump.
#[must_use]
pub fn replay(workpiece: WorkpieceId, calls: &[FieldCall]) -> WorkpieceBuilder {
    let mut builder = WorkpieceBuilder::new(workpiece);
    for group in grouped(calls) {
        apply_group(&mut builder, group);
    }
    builder
}

/// Winning generation's texts for `kind`, in slot order. Empty when unwritten.
#[must_use]
pub fn winning_texts(calls: &[FieldCall], kind: FieldKind) -> Vec<&str> {
    let Some(group) =
        grouped(calls).into_iter().rev().find(|group| group.first().is_some_and(|call| call.kind == kind))
    else {
        return Vec::new();
    };
    if is_repeated(kind) {
        group.iter().map(|call| call.value.as_str()).collect()
    } else {
        group.last().map(|call| call.value.as_str()).into_iter().collect()
    }
}

fn decode_calls(bytes: &[u8]) -> Result<Vec<FieldCall>> {
    let mut rest = bytes;
    let mut calls = Vec::new();
    while !rest.is_empty() {
        let at = bytes.len() - rest.len();
        let (call, next) =
            take_from_bytes::<FieldCall>(rest).with_context(|| format!("decode {CALLS_FILE} at byte {at}"))?;
        calls.push(call);
        rest = next;
    }
    calls.sort_by_key(|call| call.ordinal);
    Ok(calls)
}

fn grouped(calls: &[FieldCall]) -> Vec<&[FieldCall]> {
    let mut groups = Vec::new();
    let mut start = 0;
    while start < calls.len() {
        let kind = calls[start].kind;
        let mut end = start + 1;
        while end < calls.len() && calls[end].kind == kind {
            end += 1;
        }
        groups.push(&calls[start..end]);
        start = end;
    }
    groups
}

fn apply_group(builder: &mut WorkpieceBuilder, group: &[FieldCall]) {
    let Some(first) = group.first() else {
        return;
    };
    let values = group.iter().map(|call| call.value.as_str());
    match first.kind {
        FieldKind::Problem => {
            for value in values {
                builder.problem(value);
            }
        }
        FieldKind::Success => {
            for value in values {
                builder.success(value);
            }
        }
        FieldKind::Approach => {
            for value in values {
                builder.approach(value);
            }
        }
        FieldKind::RoutingHint => {
            for value in values {
                builder.routing_hint(value);
            }
        }
        FieldKind::Evidence => {
            builder.evidence(values);
        }
        FieldKind::RejectedOption => {
            builder.rejected_option(values);
        }
        FieldKind::PlanStep => {
            builder.plan_step(values);
        }
        FieldKind::Acceptance => {
            builder.acceptance(values);
        }
        FieldKind::DeclaredSurface => {
            builder.declared_surface(values);
        }
        FieldKind::Edge => {
            builder.edge(values);
        }
        FieldKind::InverseSearch | FieldKind::Implements => {}
    }
}

fn is_repeated(kind: FieldKind) -> bool {
    matches!(
        kind,
        FieldKind::Evidence
            | FieldKind::RejectedOption
            | FieldKind::PlanStep
            | FieldKind::Acceptance
            | FieldKind::DeclaredSurface
            | FieldKind::Edge
            | FieldKind::InverseSearch
    )
}

/// Parse a CLI field name into an authored [`FieldKind`].
///
/// # Errors
/// The name is unknown, or names a derived field the model must not author.
pub fn parse_field(name: &str) -> Result<FieldKind> {
    match name {
        "problem" => Ok(FieldKind::Problem),
        "evidence" => Ok(FieldKind::Evidence),
        "success" => Ok(FieldKind::Success),
        "approach" => Ok(FieldKind::Approach),
        "rejected-option" => Ok(FieldKind::RejectedOption),
        "plan-step" => Ok(FieldKind::PlanStep),
        "acceptance" => Ok(FieldKind::Acceptance),
        "declared-surface" => Ok(FieldKind::DeclaredSurface),
        "edge" => Ok(FieldKind::Edge),
        "routing-hint" => Ok(FieldKind::RoutingHint),
        "inverse-search" => {
            bail!("inverse-search is derived by the lane from plan-step symbols; it cannot be authored")
        }
        "implements" => bail!("implements is derived from ADR digests; it cannot be authored"),
        other => bail!(
            "unknown field `{other}`; accepted authored fields: problem, evidence, success, approach, \
             rejected-option, plan-step, acceptance, declared-surface, edge, routing-hint \
             (inverse-search and implements are derived and cannot be set)"
        ),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::super::set;
    use super::{append, load, parse_field, replay, winning_texts};
    use aether_bloomery::{FieldKind, ScopeRouting, WorkpieceId};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::{env, fs, process};

    fn scratch(tag: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!("aether-scope-log-{tag}-{}-{seq}", process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn workpiece() -> WorkpieceId {
        WorkpieceId(String::from("issue-5303"))
    }

    fn routing() -> ScopeRouting {
        ScopeRouting { size: String::from("l"), model: String::from("grok-4.6") }
    }

    #[test]
    fn a_plan_step_with_a_backtick_survives_the_transport() {
        // Tripwire: an argv scalar truncates at the first shell metacharacter,
        // leaving a field that looks filled. The value arrives by file so a
        // multi-paragraph plan step containing quotes, backticks and newlines
        // is stored byte for byte.
        let run = scratch("backtick");
        let step = "Replay through `WorkpieceBuilder::finish` after the \"log\".\n\nThen call `verify_scope`.";
        set("plan-step", &run, &write_value(&run, "step.txt", step)).unwrap();
        set("problem", &run, &write_value(&run, "problem.txt", "the sketch has no program")).unwrap();
        set("declared-surface", &run, &write_value(&run, "surface.txt", "xtask/**")).unwrap();

        let calls = load(&run).unwrap();
        let builder = replay(workpiece(), &calls);
        let revision = builder.finish(None, routing()).expect("coherent record set");
        assert_eq!(revision.plan, format!("1. {step}"));
        assert_eq!(winning_texts(&calls, FieldKind::PlanStep), [step]);
    }

    #[test]
    fn consecutive_plan_steps_replay_as_one_generation() {
        let run = scratch("list");
        append(&run, FieldKind::PlanStep, String::from("first")).unwrap();
        append(&run, FieldKind::PlanStep, String::from("second")).unwrap();
        append(&run, FieldKind::Problem, String::from("the problem")).unwrap();
        append(&run, FieldKind::DeclaredSurface, String::from("xtask/**")).unwrap();

        let calls = load(&run).unwrap();
        let revision = replay(workpiece(), &calls).finish(None, routing()).expect("coherent record set");
        assert_eq!(revision.plan, "1. first\n2. second");
        assert_eq!(winning_texts(&calls, FieldKind::PlanStep), ["first", "second"]);
    }

    #[test]
    fn a_later_plan_step_group_replaces_the_earlier_one() {
        let run = scratch("restated");
        append(&run, FieldKind::PlanStep, String::from("old")).unwrap();
        append(&run, FieldKind::Problem, String::from("the problem")).unwrap();
        append(&run, FieldKind::PlanStep, String::from("new-a")).unwrap();
        append(&run, FieldKind::PlanStep, String::from("new-b")).unwrap();
        append(&run, FieldKind::DeclaredSurface, String::from("xtask/**")).unwrap();

        let calls = load(&run).unwrap();
        let revision = replay(workpiece(), &calls).finish(None, routing()).expect("coherent record set");
        assert_eq!(revision.plan, "1. new-a\n2. new-b");
    }

    #[test]
    fn derived_fields_are_refused_with_a_named_reason() {
        let inverse = parse_field("inverse-search").unwrap_err().to_string();
        assert!(inverse.contains("derived"), "{inverse}");
        let implements = parse_field("implements").unwrap_err().to_string();
        assert!(implements.contains("derived"), "{implements}");
        let unknown = parse_field("not-a-field").unwrap_err().to_string();
        assert!(unknown.contains("accepted authored fields"), "{unknown}");
        assert!(unknown.contains("plan-step"), "{unknown}");
    }

    fn write_value(run: &Path, name: &str, body: &str) -> PathBuf {
        let path = run.join(name);
        fs::write(&path, body).unwrap();
        path
    }
}
