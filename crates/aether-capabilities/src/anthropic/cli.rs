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

use std::io::{ErrorKind, Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use super::DEFAULT_TIMEOUT_MS;
use crate::contentgen::adapter::{AdapterUsage, AnthropicRequest, AnthropicResponse};

/// Sentinel returned when the `claude` binary isn't on PATH so the cap
/// maps it onto `AnthropicError::CliNotFound`. Matched as a string
/// prefix — the dispatch boundary is `Result<_, String>`.
pub const CLI_NOT_FOUND: &str = "cli-not-found";

/// Sentinel prefix returned when the `claude` subprocess overruns its
/// deadline and is killed. Formatted as `timeout=<elapsed_ms>` so the
/// cap maps it onto `AnthropicError::Timeout { elapsed_ms }` (the cap
/// parses the trailing integer the way it parses `status=`).
pub const TIMEOUT_SENTINEL: &str = "timeout=";

/// How often the deadline loop polls `child.try_wait()`. Short enough
/// that a hung call is killed promptly after expiry without busy-waiting.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// `claude`-subprocess adapter. Holds the binary name (default
/// `"claude"`) and a per-request deadline; the model + prompt ride
/// per-request. The model is passed via `--model` so the user's CLI
/// selects the right backend.
pub struct ClaudeCliAdapter {
    binary: String,
    timeout: Duration,
}

impl Default for ClaudeCliAdapter {
    fn default() -> Self {
        Self {
            binary: String::from("claude"),
            timeout: Duration::from_millis(u64::from(DEFAULT_TIMEOUT_MS)),
        }
    }
}

impl ClaudeCliAdapter {
    /// Build an adapter that invokes `binary` with the given per-request
    /// `timeout`. Production wires `config.timeout`; tests point it at a
    /// missing or slow binary to exercise the `CliNotFound` / `Timeout`
    /// paths without depending on the host.
    #[must_use]
    pub fn new(binary: String, timeout: Duration) -> Self {
        Self { binary, timeout }
    }

    /// Run a completion through the `claude` subprocess. Returns the
    /// completion text or a free-form error string. A missing binary
    /// surfaces as `CLI_NOT_FOUND`; the cap maps that onto
    /// `AnthropicError::CliNotFound`. A call that overruns the adapter's
    /// `timeout` is killed + reaped and surfaces as `TIMEOUT_SENTINEL`,
    /// which the cap maps onto `AnthropicError::Timeout`.
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

        // Drain stdout on a dedicated thread so a full pipe can't stall
        // `claude` (and thus the deadline poll) while we own the `Child`
        // on this thread. stderr is small and drained after the wait.
        let stdout = child.stdout.take();
        let stdout_reader = thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut out) = stdout {
                let _ = out.read_to_end(&mut buf);
            }
            buf
        });

        // Poll for exit until the child finishes or the deadline passes.
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if started.elapsed() >= self.timeout {
                        // Deadline overrun: kill + reap so no zombie is
                        // left, join the reader, and surface the timeout.
                        let _ = child.kill();
                        let _ = child.wait();
                        let _ = stdout_reader.join();
                        let elapsed_ms =
                            u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
                        return Err(format!("{TIMEOUT_SENTINEL}{elapsed_ms}"));
                    }
                    thread::sleep(POLL_INTERVAL);
                }
                Err(e) => {
                    let _ = stdout_reader.join();
                    return Err(format!("wait for claude: {e}"));
                }
            }
        };

        let stdout_bytes = stdout_reader.join().unwrap_or_default();

        let mut stderr_bytes = Vec::new();
        if let Some(mut err) = child.stderr.take() {
            let _ = err.read_to_end(&mut stderr_bytes);
        }
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        if !stderr.trim().is_empty() {
            tracing::warn!(
                target: "aether_capabilities::anthropic",
                stderr = %stderr.trim(),
                "claude subprocess wrote to stderr",
            );
        }

        if !status.success() {
            return Err(format!("claude exited with {}: {}", status, stderr.trim()));
        }

        let text = String::from_utf8_lossy(&stdout_bytes).trim().to_string();
        let wall_clock_ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);

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
    use super::{CLI_NOT_FOUND, ClaudeCliAdapter, TIMEOUT_SENTINEL};
    use crate::contentgen::adapter::AnthropicRequest;
    use std::time::{Duration, Instant};

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
            Duration::from_secs(30),
        );
        let err = adapter
            .cli_send(&req())
            .expect_err("a missing binary must error");
        assert_eq!(err, CLI_NOT_FOUND);
    }

    /// A subprocess that outlives the deadline is killed + reaped and
    /// surfaces as `TIMEOUT_SENTINEL` carrying the elapsed ms — well
    /// under the child's nominal 5s sleep. The adapter always invokes
    /// `<binary> --print --model <model> ...`, so the stand-in must be a
    /// script that ignores its args and sleeps. We write a tiny shell
    /// script to a temp file and point the adapter at it.
    #[cfg(unix)]
    #[test]
    fn slow_binary_times_out_and_is_reaped() {
        use std::os::unix::fs::PermissionsExt;
        use std::{env, fs, process};

        let mut script = env::temp_dir();
        script.push(format!("aether-cli-timeout-{}.sh", process::id()));
        // Ignore every arg, sleep 5s. A ~50ms deadline must fire first.
        fs::write(&script, "#!/bin/sh\nsleep 5\n").expect("write stand-in script");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755))
            .expect("chmod stand-in script");

        let adapter = ClaudeCliAdapter::new(
            script.to_string_lossy().into_owned(),
            Duration::from_millis(50),
        );
        let started = Instant::now();
        let err = adapter
            .cli_send(&req())
            .expect_err("a slow binary must time out");

        let _ = fs::remove_file(&script);

        assert!(
            err.starts_with(TIMEOUT_SENTINEL),
            "expected timeout sentinel, got {err:?}",
        );
        // The deadline fired well under the child's 5s lifetime, which
        // also implies the kill returned (the child was reaped).
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "timeout took too long: {:?}",
            started.elapsed(),
        );
    }
}
