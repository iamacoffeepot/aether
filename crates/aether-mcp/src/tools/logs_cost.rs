use aether_data::{Kind, tagged_id};
use aether_kinds::{CostTail, CostTailResult};
use rmcp::ErrorData as McpError;

use crate::args::{ActorCostArgs, ActorCostResponse, ActorCostRow, ActorLogEntry, ActorLogsArgs, ActorLogsResponse};

use super::Mcp;
use super::envelope::engine_envelope;
use super::ids::{parse_engine_id, parse_kind_id, static_kind_name};
use super::render::{internal, internal_msg, json};

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
/// the log cap's pre-issue-776 conversion).
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

pub(super) async fn actor_logs(mcp: &Mcp, args: ActorLogsArgs) -> Result<String, McpError> {
    let engine = parse_engine_id(&args.engine_id)?;
    let engine_id_str = args.engine_id.clone();
    let mailbox_name = args.mailbox_name.clone();
    let min_level = match args.level.as_deref() {
        Some(s) => Some(parse_level(s)?),
        None => None,
    };
    let request =
        aether_kinds::LogTail { max: args.max.unwrap_or(0), min_level, since: args.since, contains: args.contains };
    let reply = mcp.session.call_one(engine_envelope(engine, &args.mailbox_name, &request)).await.map_err(internal)?;
    match aether_kinds::LogTailResult::decode_from_bytes(&reply.payload) {
        Some(aether_kinds::LogTailResult::Ok { entries, next_since, truncated_before }) => {
            let response = ActorLogsResponse {
                engine_id: engine_id_str,
                mailbox_name,
                entries: entries
                    .into_iter()
                    .map(|e| ActorLogEntry {
                        timestamp_unix_ms: e.timestamp_unix_ms,
                        level: level_to_str(e.level).to_owned(),
                        target: e.target,
                        message: e.message,
                        sequence: e.sequence,
                    })
                    .collect(),
                next_since,
                truncated_before,
            };
            json(&response)
        }
        // Issue 963: name the agent-supplied mailbox in the error
        // so an `actor_logs` against an unregistered mailbox (which
        // the substrate now answers with a synthesized
        // `LogTailResult::Err`, mailer.rs `None` arm) reads as
        // "that mailbox doesn't exist" rather than a bare relayed
        // substrate string.
        Some(aether_kinds::LogTailResult::Err { error }) => {
            Err(internal_msg(&actor_logs_err_message(&mailbox_name, &error)))
        }
        None => Err(internal_msg("undecodable LogTailResult")),
    }
}

pub(super) async fn actor_cost(mcp: &Mcp, args: ActorCostArgs) -> Result<String, McpError> {
    let engine = parse_engine_id(&args.engine_id)?;
    let engine_id_str = args.engine_id.clone();
    let mailbox_name = args.mailbox_name.clone();
    // Optional kind filter: accept a tagged `knd-…` id or a raw
    // decimal `u64`, matching the rest of the MCP id surface.
    let kind = match args.kind_id.as_deref() {
        Some(s) => Some(parse_kind_id(s)?),
        None => None,
    };
    let request = CostTail { kind };
    let reply = mcp.session.call_one(engine_envelope(engine, &args.mailbox_name, &request)).await.map_err(internal)?;
    match CostTailResult::decode_from_bytes(&reply.payload) {
        Some(CostTailResult::Ok { rows }) => {
            let response = ActorCostResponse {
                engine_id: engine_id_str,
                mailbox_name,
                rows: rows
                    .into_iter()
                    .map(|r| ActorCostRow {
                        // Render the kind id as the ADR-0064 tagged
                        // string the rest of the MCP wire uses, falling
                        // back to a hex literal on an unencodable id.
                        kind_id: tagged_id::encode(r.kind_id.0).unwrap_or_else(|| format!("{:#x}", r.kind_id.0)),
                        // The substrate ships `kind_name: None` (the
                        // cost table holds ids, not names); resolve it
                        // best-effort from the static kind inventory
                        // the MCP harness ships with. Component-defined
                        // kinds stay `None`.
                        kind_name: r.kind_name.or_else(|| static_kind_name(r.kind_id)),
                        mean_nanos: r.mean_nanos,
                        mad_nanos: r.mad_nanos,
                        samples: r.samples,
                    })
                    .collect(),
            };
            json(&response)
        }
        Some(CostTailResult::Err { error }) => Err(internal_msg(&format!("actor_cost: {mailbox_name} — {error}"))),
        None => Err(internal_msg("undecodable CostTailResult")),
    }
}
