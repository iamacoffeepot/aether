use super::McpError;

/// Issue 963: render an `actor_logs` `LogTailResult::Err` into a
/// tool-error message that names the agent-supplied mailbox, so an
/// unregistered-mailbox query reads as "that mailbox doesn't exist"
/// rather than a bare relayed substrate string. Factored out so the
/// formatting is unit-testable without standing up a live engine.
pub(super) fn actor_logs_err_message(mailbox_name: &str, error: &str) -> String {
    format!("actor_logs: mailbox \"{mailbox_name}\" — {error}")
}

/// Map ADR-0023 §4's level string to the `0..=4` byte the
/// `aether.log.*` kinds carry. Case-insensitive. Returns an
/// `invalid_params` error on unknown strings so a typoed `"Warn "`
/// surfaces at the tool boundary rather than reaching the substrate.
pub(super) fn parse_level(s: &str) -> Result<u8, McpError> {
    match s.to_ascii_lowercase().as_str() {
        "trace" => Ok(0),
        "debug" => Ok(1),
        "info" => Ok(2),
        "warn" => Ok(3),
        "error" => Ok(4),
        other => Err(McpError::invalid_params(
            format!("unknown level {other:?}; expected trace|debug|info|warn|error"),
            None,
        )),
    }
}

/// Inverse of [`parse_level`]: render the `0..=4` byte back to the
/// canonical lowercase level string. Out-of-band bytes render as
/// `"info"` (matches the existing fallback in
/// `aether-capabilities::log`'s pre-issue-776 conversion).
pub(super) fn level_to_str(level: u8) -> &'static str {
    match level {
        0 => "trace",
        1 => "debug",
        3 => "warn",
        4 => "error",
        // 2 is "info"; out-of-band bytes also render as "info".
        _ => "info",
    }
}
