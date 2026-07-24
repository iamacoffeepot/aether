#[allow(clippy::wildcard_imports)]
use super::super::test_support::*;
#[allow(clippy::wildcard_imports)]
use super::super::*;
use std::collections::VecDeque;

/// `parse_level` round-trips every documented spelling and rejects
/// unknown strings — case-insensitive (`"INFO"` and `"info"` both
/// land on `2`).
#[test]
fn parse_level_round_trips_documented_strings() {
    assert_eq!(parse_level("trace").expect("test setup: \"trace\" parses"), 0);
    assert_eq!(parse_level("debug").expect("test setup: \"debug\" parses"), 1);
    assert_eq!(parse_level("info").expect("test setup: \"info\" parses"), 2);
    assert_eq!(parse_level("warn").expect("test setup: \"warn\" parses"), 3);
    assert_eq!(parse_level("error").expect("test setup: \"error\" parses"), 4);
    assert_eq!(parse_level("INFO").expect("test setup: case-insensitive \"INFO\" parses"), 2);
    assert!(parse_level("verbose").is_err());
}

/// `level_to_str` inverts `parse_level` for in-band bytes and
/// falls back to `"info"` for out-of-band ones (matches the
/// pre-issue-776 conversion behaviour of the log cap).
#[test]
fn level_to_str_matches_parse_level_and_falls_back_to_info() {
    for level in 0..=4u8 {
        let parsed =
            parse_level(level_to_str(level)).expect("test setup: level_to_str output round-trips through parse_level");
        assert_eq!(parsed, level);
    }
    assert_eq!(level_to_str(99), "info");
}

/// `actor_logs` with a malformed `engine_id` rejects up front
/// without touching the wire.
#[tokio::test]
async fn actor_logs_bad_engine_id_is_tool_error() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let result = mcp
        .actor_logs(Parameters(ActorLogsArgs {
            engine_id: "not-a-uuid".to_owned(),
            mailbox_name: "aether.audio".to_owned(),
            max: None,
            level: None,
            since: None,
            contains: None,
        }))
        .await;
    assert!(result.is_err(), "a malformed engine_id should be a tool error");
}

/// Issue 963: the `LogTailResult::Err` arm names the agent-
/// supplied mailbox in the tool error. A live engine isn't needed
/// to inject a decoded `Err` — pin the formatting at the call
/// site's helper instead (the substrate-side synthesized-Err
/// routing is covered in `aether-substrate`'s mailer tests).
#[test]
fn actor_logs_err_message_names_mailbox() {
    let msg = actor_logs_err_message("aether.nope", "mailbox mbx-0000-0000-0000 not registered");
    assert!(msg.contains("aether.nope"), "names the mailbox: {msg}");
    assert!(msg.contains("not registered"), "carries the cause: {msg}");
}

/// iamacoffeepot/aether#1128: `actor_cost` with a malformed
/// `engine_id` rejects at the tool boundary without touching the
/// wire (mirrors `actor_logs_bad_engine_id_is_tool_error`).
#[tokio::test]
async fn actor_cost_bad_engine_id_is_tool_error() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let result = mcp
        .actor_cost(Parameters(ActorCostArgs {
            engine_id: "not-a-uuid".to_owned(),
            mailbox_name: "aether.audio".to_owned(),
            kind_id: None,
        }))
        .await;
    assert!(result.is_err(), "a malformed engine_id should be a tool error");
}

/// `actor_logs` with an unknown `level` string is rejected at
/// the tool boundary before any RPC.
#[tokio::test]
async fn actor_logs_bad_level_is_tool_error() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let result = mcp
        .actor_logs(Parameters(ActorLogsArgs {
            engine_id: "00000000-0000-0000-0000-000000000001".to_owned(),
            mailbox_name: "aether.audio".to_owned(),
            max: None,
            level: Some("verbose".to_owned()),
            since: None,
            contains: None,
        }))
        .await;
    assert!(result.is_err(), "an unknown level should be a tool error");
}

#[tokio::test]
async fn actor_logs_and_cost_route_to_the_engine_resolved_mailbox_id() {
    let engine_answer = MailboxId(0x4057_0000_0000_0200);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let replies = Arc::new(Mutex::new(VecDeque::from([
        TerrainRouteReply {
            events: vec![TerrainReplyEvent {
                kind: aether_kinds::LogTailResult::ID,
                payload: aether_kinds::LogTailResult::Ok { entries: Vec::new(), next_since: 0, truncated_before: None }
                    .encode_into_bytes(),
            }],
            settle: true,
        },
        TerrainRouteReply {
            events: vec![TerrainReplyEvent {
                kind: aether_kinds::CostTailResult::ID,
                payload: aether_kinds::CostTailResult::Ok { rows: Vec::new() }.encode_into_bytes(),
            }],
            settle: true,
        },
    ])));
    let (_chassis, port) = boot_hub_with_address_route_replies(
        engine_answer,
        "aether.test/aether.test.child:probe",
        Arc::clone(&calls),
        replies,
    );
    let mcp = connect_mcp(port);
    let engine_id = Uuid::from_u128(0x4057).to_string();

    mcp.actor_logs(Parameters(ActorLogsArgs {
        engine_id: engine_id.clone(),
        mailbox_name: "aether.test://probe".to_owned(),
        max: None,
        level: None,
        since: None,
        contains: None,
    }))
    .await
    .expect("actor logs resolves and returns");
    mcp.actor_cost(Parameters(ActorCostArgs {
        engine_id,
        mailbox_name: "aether.test://probe".to_owned(),
        kind_id: None,
    }))
    .await
    .expect("actor cost resolves and returns");

    let calls = calls.lock().expect("address-route calls mutex is never poisoned");
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[0].kind, ResolveAddress::ID);
    assert_eq!(calls[1].mailbox, engine_answer);
    assert_eq!(calls[2].kind, ResolveAddress::ID);
    assert_eq!(calls[3].mailbox, engine_answer);
    drop(calls);
}
