//! Self-contained stdio MCP server the Claude `review.critic` harness forks.
//!
//! No crate beyond what xtask already links: JSON-RPC 2.0 over stdin/stdout,
//! two append-only tools. Paths come from `--out`; the parent chose them.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::{env, fs};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::transform::review_reports::{
    FINDINGS_NAME, FindingClass, MCP_CONFIG_NAME, NOTES_NAME, REVIEW_REPORT, append_finding, append_note,
    findings_path, notes_path,
};

/// Write the Claude `--mcp-config` that injects this server, and empty report
/// files so a reviewer that reports nothing leaves a well-defined empty file
/// rather than an absent one.
pub(super) fn prepare(out: &Path) -> Result<PathBuf> {
    fs::create_dir_all(out).with_context(|| format!("create {}", out.display()))?;
    let out = out.canonicalize().with_context(|| format!("canonicalize {}", out.display()))?;
    fs::write(out.join(FINDINGS_NAME), "")?;
    fs::write(out.join(NOTES_NAME), "")?;

    let exe = env::current_exe().context("current xtask executable")?;
    let config = json!({
        "mcpServers": {
            "review": {
                "command": exe,
                "args": ["transform", REVIEW_REPORT, "--out", &out],
            }
        }
    });
    let path = out.join(MCP_CONFIG_NAME);
    fs::write(&path, serde_json::to_vec_pretty(&config)?).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

/// Serve `report_finding` / `report_note` on stdio until stdin closes.
pub(super) fn serve(out: &Path) -> Result<()> {
    let stdin = io::stdin();
    let mut input = io::BufReader::new(stdin.lock());
    let mut output = io::stdout().lock();
    while let Some(request) = read_message(&mut input)? {
        if let Some(response) = handle(&request, out) {
            write_message(&mut output, &response)?;
        }
    }
    Ok(())
}

pub(super) fn handle(request: &Value, out: &Path) -> Option<Value> {
    let method = request.get("method")?.as_str()?;
    let id = request.get("id");
    match method {
        "initialize" => Some(ok(id?, initialize_result(request))),
        "notifications/initialized" | "initialized" => None,
        "tools/list" => Some(ok(id?, tools_list())),
        "tools/call" => Some(ok(id?, tools_call(request, out))),
        "ping" => Some(ok(id?, json!({}))),
        "resources/list" => Some(ok(id?, json!({ "resources": [] }))),
        "prompts/list" => Some(ok(id?, json!({ "prompts": [] }))),
        _ => id.map(|id| rpc_error(id, -32601, &format!("method not found: {method}"))),
    }
}

fn initialize_result(request: &Value) -> Value {
    let version = request.pointer("/params/protocolVersion").and_then(Value::as_str).unwrap_or("2024-11-05");
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "aether-review-report", "version": "0.3.0-alpha" },
    })
}

fn tools_list() -> Value {
    json!({
        "tools": [
            {
                "name": "report_finding",
                "description": "Record one confirmed defect or environment shortfall. Append-only; call as you confirm each one. There is no pass tool — a finished run that reported no defects is a pass.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "summary": { "type": "string", "description": "One sentence naming the defect or host fault." },
                        "detail": { "type": "string", "description": "Evidence: file, line, and the failure scenario." },
                        "class": { "type": "string", "enum": ["defect", "environment"], "description": "defect charges the candidate; environment means you could not judge." }
                    },
                    "required": ["summary", "detail", "class"]
                }
            },
            {
                "name": "report_note",
                "description": "Record a free-text observation. Notes land in the evidence record and never affect the stamped status.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "text": { "type": "string", "description": "The observation." }
                    },
                    "required": ["text"]
                }
            }
        ]
    })
}

fn tools_call(request: &Value, out: &Path) -> Value {
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
    match name {
        "report_finding" => report_finding(&arguments, &findings_path(out)),
        "report_note" => report_note(&arguments, &notes_path(out)),
        _ => tool_error(&format!("unknown tool: {name}")),
    }
}

fn report_finding(arguments: &Value, path: &Path) -> Value {
    let summary = arguments.get("summary").and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty());
    let detail = arguments.get("detail").and_then(Value::as_str).unwrap_or("");
    let class = arguments.get("class").and_then(Value::as_str).and_then(FindingClass::parse);
    let (Some(summary), Some(class)) = (summary, class) else {
        return tool_error("report_finding requires summary, detail, and class (defect|environment)");
    };
    match append_finding(path, summary, detail, class) {
        Ok(()) => tool_ok("recorded finding"),
        Err(error) => tool_error(&format!("could not write finding: {error}")),
    }
}

fn report_note(arguments: &Value, path: &Path) -> Value {
    let Some(text) = arguments.get("text").and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty())
    else {
        return tool_error("report_note requires text");
    };
    match append_note(path, text) {
        Ok(()) => tool_ok("recorded note"),
        Err(error) => tool_error(&format!("could not write note: {error}")),
    }
}

fn tool_ok(text: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": text }] })
}

fn tool_error(text: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": true })
}

fn ok(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn read_message(input: &mut impl BufRead) -> Result<Option<Value>> {
    let mut header = String::new();
    let read = input.read_line(&mut header)?;
    if read == 0 {
        return Ok(None);
    }
    if let Some(rest) =
        header.split_once(':').and_then(|(name, rest)| name.eq_ignore_ascii_case("content-length").then_some(rest))
    {
        let length: usize = rest.trim().parse().context("Content-Length")?;
        loop {
            let mut line = String::new();
            if input.read_line(&mut line)? == 0 {
                break;
            }
            if line.trim().is_empty() {
                break;
            }
        }
        let mut body = vec![0; length];
        input.read_exact(&mut body).context("read MCP body")?;
        return Ok(Some(serde_json::from_slice(&body).context("parse MCP body")?));
    }
    let trimmed = header.trim();
    if trimmed.is_empty() {
        return read_message(input);
    }
    Ok(Some(serde_json::from_str(trimmed).context("parse MCP line")?))
}

fn write_message(output: &mut impl Write, message: &Value) -> Result<()> {
    let body = serde_json::to_vec(message)?;
    write!(output, "Content-Length: {}\r\n\r\n", body.len())?;
    output.write_all(&body)?;
    output.flush()?;
    Ok(())
}

/// `--mcp-config` / `--strict-mcp-config` flags Claude needs to see the server.
pub(super) fn mcp_argv(config: &Path) -> [String; 3] {
    ["--mcp-config".to_owned(), config.display().to_string(), "--strict-mcp-config".to_owned()]
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::{env, fs, process};

    use serde_json::{Value, json};

    use super::{handle, mcp_argv, prepare};
    use crate::transform::review_reports::{FindingClass, Reports, load_reports};

    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!("aether-review-mcp-{tag}-{}-{seq}", process::id()));
        fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    #[test]
    fn report_finding_appends_a_line_the_lane_parses_as_a_defect() {
        let out = scratch("finding");
        let response = handle(
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "report_finding",
                    "arguments": {
                        "summary": "empty input panics",
                        "detail": "src/lib.rs: unguarded index",
                        "class": "defect"
                    }
                }
            }),
            &out,
        )
        .expect("tool response");
        assert_eq!(response["result"]["isError"], json!(null));
        match load_reports(&out.join("review-findings.jsonl")) {
            Reports::Clean { findings } => {
                assert_eq!(findings.len(), 1);
                assert_eq!(findings[0].summary, "empty input panics");
                assert_eq!(findings[0].class, FindingClass::Defect);
            }
            other => panic!("expected a clean defect report, got {other:?}"),
        }
    }

    #[test]
    fn tools_list_names_the_two_append_only_writers() {
        let listed = handle(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}), Path::new(".")).expect("list");
        let names: Vec<&str> = listed["result"]["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str))
            .collect();
        assert_eq!(names, ["report_finding", "report_note"]);
    }

    #[test]
    fn mcp_argv_points_claude_at_the_prepared_config() {
        let argv = mcp_argv(Path::new("/tmp/review-mcp.json"));
        assert_eq!(argv[0], "--mcp-config");
        assert_eq!(argv[1], "/tmp/review-mcp.json");
        assert_eq!(argv[2], "--strict-mcp-config");
    }

    #[test]
    fn prepare_writes_empty_report_files_and_a_config_that_names_this_binary() {
        let out = scratch("prepare");
        let config = prepare(&out).expect("prepare");
        assert!(config.exists());
        assert_eq!(fs::read_to_string(out.join("review-findings.jsonl")).expect("findings"), "");
        let parsed: Value = serde_json::from_slice(&fs::read(config).expect("config")).expect("json");
        assert_eq!(parsed["mcpServers"]["review"]["args"][0], "transform");
        assert_eq!(parsed["mcpServers"]["review"]["args"][1], "review.report");
    }
}
