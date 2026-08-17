//! Incremental review reports written by the Claude-harness MCP tools.
//!
//! The findings file is append-only JSONL. `None` of a missing file and an
//! empty file are the same state: the reviewer reported nothing. A truncated
//! line or an unknown `class` is a lane shortfall, not a candidate verdict.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Typed id of the stdio MCP server the Claude review harness forks.
pub(super) const REVIEW_REPORT: &str = "review.report";

pub(super) const FINDINGS_NAME: &str = "review-findings.jsonl";
pub(super) const NOTES_NAME: &str = "review-notes.jsonl";
pub(super) const MCP_CONFIG_NAME: &str = "review-mcp.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Reports {
    Clean { findings: Vec<FindingReport> },
    Malformed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FindingReport {
    pub summary: String,
    pub detail: String,
    pub class: FindingClass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FindingClass {
    Defect,
    Environment,
}

impl FindingClass {
    pub(super) fn parse(value: &str) -> Option<Self> {
        match value {
            "defect" => Some(Self::Defect),
            "environment" => Some(Self::Environment),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Defect => "defect",
            Self::Environment => "environment",
        }
    }
}

pub(super) fn findings_path(out: &Path) -> PathBuf {
    out.join(FINDINGS_NAME)
}

pub(super) fn notes_path(out: &Path) -> PathBuf {
    out.join(NOTES_NAME)
}

pub(super) fn load_reports(path: &Path) -> Reports {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Reports::Clean { findings: Vec::new() };
        }
        Err(error) => {
            return Reports::Malformed { reason: format!("could not read findings file: {error}") };
        }
    };
    parse_reports(&text)
}

pub(super) fn parse_reports(text: &str) -> Reports {
    let mut findings = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match parse_finding_line(line) {
            Ok(finding) => findings.push(finding),
            Err(reason) => {
                return Reports::Malformed { reason: format!("line {}: {reason}", index + 1) };
            }
        }
    }
    Reports::Clean { findings }
}

fn parse_finding_line(line: &str) -> Result<FindingReport, String> {
    let value: Value = serde_json::from_str(line).map_err(|_| "truncated line".to_owned())?;
    let summary = value
        .get("summary")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| "missing summary".to_owned())?
        .to_owned();
    let detail = value.get("detail").and_then(Value::as_str).unwrap_or("").to_owned();
    let class = value
        .get("class")
        .and_then(Value::as_str)
        .ok_or_else(|| "unknown class".to_owned())
        .and_then(|class| FindingClass::parse(class).ok_or_else(|| "unknown class".to_owned()))?;
    Ok(FindingReport { summary, detail, class })
}

pub(super) fn render_reports(findings: &[FindingReport]) -> String {
    findings.iter().map(render_finding).collect::<Vec<_>>().join("\n")
}

fn render_finding(finding: &FindingReport) -> String {
    if finding.detail.trim().is_empty() {
        format!("- {}", finding.summary)
    } else {
        format!("- {}\n  {}", finding.summary, finding.detail.trim())
    }
}

pub(super) fn load_notes(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let notes: Vec<String> = text
        .lines()
        .filter_map(|line| {
            let value: Value = serde_json::from_str(line).ok()?;
            value.get("text").and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty()).map(str::to_owned)
        })
        .collect();
    (!notes.is_empty()).then(|| notes.join("\n"))
}

pub(super) fn append_finding(path: &Path, summary: &str, detail: &str, class: FindingClass) -> io::Result<()> {
    append_jsonl(
        path,
        &serde_json::json!({
            "summary": summary,
            "detail": detail,
            "class": class.as_str(),
        }),
    )
}

pub(super) fn append_note(path: &Path, text: &str) -> io::Result<()> {
    append_jsonl(path, &serde_json::json!({ "text": text }))
}

fn append_jsonl(path: &Path, value: &Value) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{FindingClass, FindingReport, Reports, parse_reports, render_reports};

    #[test]
    fn an_empty_findings_file_is_a_clean_report() {
        assert_eq!(parse_reports(""), Reports::Clean { findings: Vec::new() });
        assert_eq!(parse_reports("\n\n"), Reports::Clean { findings: Vec::new() });
    }

    #[test]
    fn a_truncated_line_or_unknown_class_is_malformed() {
        assert!(matches!(
            parse_reports("{\"summary\":\"x\",\"detail\":\"y\",\"class\":\"defect\"}\n{"),
            Reports::Malformed { .. }
        ));
        assert!(matches!(
            parse_reports(r#"{"summary":"x","detail":"y","class":"warning"}"#),
            Reports::Malformed { reason } if reason.contains("unknown class")
        ));
    }

    #[test]
    fn reports_render_in_file_order() {
        let rendered = render_reports(&[
            FindingReport {
                summary: "empty input panics".to_owned(),
                detail: "src/lib.rs: the index is unguarded".to_owned(),
                class: FindingClass::Defect,
            },
            FindingReport {
                summary: "host could not run git diff".to_owned(),
                detail: "bwrap: loopback failed".to_owned(),
                class: FindingClass::Environment,
            },
        ]);
        assert!(rendered.starts_with("- empty input panics\n  src/lib.rs: the index is unguarded"));
        assert!(rendered.contains("- host could not run git diff\n  bwrap: loopback failed"));
    }
}
