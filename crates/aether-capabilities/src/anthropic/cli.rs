//! `claude`-subprocess backend for the `aether.anthropic` cap
//! (ADR-0050). The user runs Claude through a subscription via the
//! local CLI, not direct API access (`project_llm_sink_design`), so
//! `aether.anthropic.cli.send` is a first-class call surface: spawn
//! `claude`, pipe the prompt to stdin, read the completion from
//! stdout, route stderr to the actor log ring. `Usage` carries only
//! `wall_clock_ms` — the subprocess reports no token counts.
//!
//! The blocking `std::process::Command` call runs on issue 1013's
//! spawn-and-die ephemeral thread, never on the dispatcher.

use std::io::{ErrorKind, Write};
use std::process::{Command, Stdio};
use std::time::Instant;

use crate::contentgen::adapter::{AdapterUsage, AnthropicRequest, AnthropicResponse};

/// Sentinel returned when the `claude` binary isn't on PATH so the cap
/// maps it onto `AnthropicError::CliNotFound`. Matched as a string
/// prefix — the dispatch boundary is `Result<_, String>`.
pub const CLI_NOT_FOUND: &str = "cli-not-found";

/// `claude`-subprocess adapter. Holds the binary name (default
/// `"claude"`); the model + prompt ride per-request. The model is
/// passed via `--model` so the user's CLI selects the right backend.
pub struct ClaudeCliAdapter {
    binary: String,
}

impl Default for ClaudeCliAdapter {
    fn default() -> Self {
        Self {
            binary: String::from("claude"),
        }
    }
}

impl ClaudeCliAdapter {
    /// Build an adapter that invokes `binary`. Production uses the
    /// default `"claude"`; tests point it at a missing binary to
    /// exercise the `CliNotFound` path without depending on the host.
    #[must_use]
    pub fn new(binary: String) -> Self {
        Self { binary }
    }

    /// Run a completion through the `claude` subprocess. Returns the
    /// completion text or a free-form error string. A missing binary
    /// surfaces as [`CLI_NOT_FOUND`]; the cap maps that onto
    /// `AnthropicError::CliNotFound`.
    pub fn cli_send(&self, req: &AnthropicRequest) -> Result<AnthropicResponse, String> {
        let started = Instant::now();

        let mut command = Command::new(&self.binary);
        command
            .arg("--print")
            .arg("--model")
            .arg(&req.model)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(system) = &req.system {
            command.arg("--system-prompt").arg(system);
        }

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Err(CLI_NOT_FOUND.to_string());
            }
            Err(e) => return Err(format!("spawn claude: {e}")),
        };

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(req.prompt.as_bytes())
                .map_err(|e| format!("write prompt to stdin: {e}"))?;
            // Drop closes the pipe so `claude` sees EOF and proceeds.
        }

        let output = child
            .wait_with_output()
            .map_err(|e| format!("wait for claude: {e}"))?;

        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.trim().is_empty() {
            tracing::warn!(
                target: "aether_capabilities::anthropic",
                stderr = %stderr.trim(),
                "claude subprocess wrote to stderr",
            );
        }

        if !output.status.success() {
            return Err(format!(
                "claude exited with {}: {}",
                output.status,
                stderr.trim()
            ));
        }

        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let wall_clock_ms =
            u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);

        Ok(AnthropicResponse {
            text,
            model_used: req.model.clone(),
            usage: AdapterUsage {
                input_tokens: 0,
                output_tokens: 0,
                wall_clock_ms,
                cost_micros: None,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{CLI_NOT_FOUND, ClaudeCliAdapter};
    use crate::contentgen::adapter::AnthropicRequest;

    fn req() -> AnthropicRequest {
        AnthropicRequest {
            model: "claude-test".to_string(),
            prompt: "hello".to_string(),
            system: None,
            max_tokens: None,
            temperature: None,
        }
    }

    #[test]
    fn missing_binary_returns_cli_not_found() {
        // Point the adapter at a binary that can't exist on PATH so the
        // test never depends on whether the real `claude` is installed.
        let adapter = ClaudeCliAdapter::new(
            "aether-nonexistent-claude-binary-xyzzy".to_string(),
        );
        let err = adapter
            .cli_send(&req())
            .expect_err("a missing binary must error");
        assert_eq!(err, CLI_NOT_FOUND);
    }
}
