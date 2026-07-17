#[allow(clippy::wildcard_imports)]
use super::super::test_support::*;
#[allow(clippy::wildcard_imports)]
use super::super::*;

/// `send_mail` is a best-effort batch: a bad `kind_name` and a bad
/// `engine_id` fail locally in `deliver_one`, while a well-formed
/// item addressed at an unknown engine round-trips to the hub and
/// comes back a `CallSettled::Err`. Every item reports `error: ...`
/// and none aborts its siblings.
#[tokio::test]
async fn send_mail_reports_per_item_errors() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let out = mcp
        .send_mail(Parameters(SendMailArgs {
            mails: vec![
                MailSpec {
                    engine_id: "00000000-0000-0000-0000-000000000001".to_owned(),
                    mail: EngineMailSpec {
                        recipient_name: "aether.fs".to_owned(),
                        kind_name: "not.a.real.kind".to_owned(),
                        params: None,
                    },
                },
                MailSpec {
                    engine_id: "not-a-uuid".to_owned(),
                    mail: EngineMailSpec {
                        recipient_name: "aether.fs".to_owned(),
                        kind_name: "aether.fs.list".to_owned(),
                        params: None,
                    },
                },
                MailSpec {
                    engine_id: "00000000-0000-0000-0000-000000000002".to_owned(),
                    mail: EngineMailSpec {
                        recipient_name: "aether.fs".to_owned(),
                        kind_name: "aether.fs.list".to_owned(),
                        params: Some(serde_json::json!({ "namespace": "save", "prefix": "" })),
                    },
                },
            ],
            fire_and_forget: false,
            replies: ReplyProjection::Terminal,
        }))
        .await
        .expect("send_mail returns a status array, not a tool error");
    let statuses: Vec<MailStatus> = serde_json::from_str(&out).expect("status array");
    assert_eq!(statuses.len(), 3);
    for status in &statuses {
        assert!(status.status.starts_with("error: "), "item {} should be an error: {}", status.index, status.status);
    }
}

/// `send_mail_traced` with an unknown kind in the batch is
/// rejected up front — the batch is encoded before any RPC,
/// mirroring `capture_frame`'s all-or-fail bundle semantics.
#[tokio::test]
async fn send_mail_traced_bad_spec_is_tool_error() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let result = mcp
        .send_mail_traced(Parameters(SendMailTracedArgs {
            engine_id: "00000000-0000-0000-0000-000000000001".to_owned(),
            mails: vec![EngineMailSpec {
                recipient_name: "aether.render".to_owned(),
                kind_name: "not.a.real.kind".to_owned(),
                params: None,
            }],
            settlement_timeout_ms: None,
            fire_and_forget: false,
            full: false,
        }))
        .await;
    assert!(result.is_err(), "an unknown kind in the batch should be a tool error");
}

/// Issue 1242: `fire_and_forget: true` is non-blocking — a
/// well-formed item is dispatched without awaiting any reply, so the
/// call returns `status: "dispatched"` with empty `replies` well
/// under the await timeout, even against an unknown engine (the
/// server's eventual error `ReplyEnd` is dropped as an unrouted
/// frame, never awaited). Contrast `delivered`, which blocks on
/// settlement.
#[tokio::test]
async fn send_mail_fire_and_forget_is_non_blocking() {
    use std::time::Instant;

    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let started = Instant::now();
    let out = mcp
        .send_mail(Parameters(SendMailArgs {
            mails: vec![MailSpec {
                // A well-formed item to an engine the hub doesn't
                // supervise: the dispatch chain never settles with a
                // reply, so a blocking call would wait — fire-and-
                // forget returns at once.
                engine_id: "00000000-0000-0000-0000-000000000099".to_owned(),
                mail: EngineMailSpec {
                    recipient_name: "aether.fs".to_owned(),
                    kind_name: "aether.fs.list".to_owned(),
                    params: Some(serde_json::json!({ "namespace": "save", "prefix": "" })),
                },
            }],
            fire_and_forget: true,
            replies: ReplyProjection::All,
        }))
        .await
        .expect("send_mail returns a status array");
    assert!(started.elapsed() < Duration::from_secs(5), "fire-and-forget must not block on settlement");
    let statuses: Vec<MailStatus> = serde_json::from_str(&out).expect("status array");
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].status, "dispatched", "fire-and-forget reports dispatched, not delivered");
    assert!(statuses[0].replies.is_empty(), "fire-and-forget carries no replies");
    assert!(!statuses[0].timed_out, "dispatch is not a timeout");
}

fn traced_response_node() -> MailNodeJson {
    MailNodeJson {
        mail_id: MailIdJson { sender: "aether.chassis".to_owned(), correlation_id: 1 },
        parent: None,
        sender: "aether.chassis".to_owned(),
        recipient: "aether.fs".to_owned(),
        kind: "aether.fs.list".to_owned(),
        t_construct_start: 1,
        t_sent: 1,
        t_received: Some(2),
        t_finished: Some(3),
        thread_name: Some("aether-worker-0".to_owned()),
    }
}

#[test]
fn traced_response_serializes_compact_and_full_settled_shapes_precisely() {
    let compact = serde_json::to_value(SendMailTracedResponse {
        status: "settled".to_owned(),
        root: Some(MailIdJson { sender: "aether.chassis".to_owned(), correlation_id: 1 }),
        mails: None,
        tree: Some(vec!["aether.chassis → aether.fs  aether.fs.list  +0µs".to_owned()]),
        node_count: Some(1),
        in_flight: Some(0),
        replies: Some(Vec::new()),
    })
    .expect("compact response serializes");
    assert!(compact["mails"].is_null());
    assert_eq!(compact["tree"].as_array().map(Vec::len), Some(1));
    assert_eq!(compact["node_count"], 1);

    let full = serde_json::to_value(SendMailTracedResponse {
        status: "settled".to_owned(),
        root: Some(MailIdJson { sender: "aether.chassis".to_owned(), correlation_id: 1 }),
        mails: Some(vec![traced_response_node()]),
        tree: None,
        node_count: Some(1),
        in_flight: Some(0),
        replies: Some(Vec::new()),
    })
    .expect("full response serializes");
    assert_eq!(full["mails"].as_array().map(Vec::len), Some(1));
    assert!(!full.as_object().expect("full response object").contains_key("tree"));
    assert_eq!(full["node_count"], 1);
}

#[test]
fn traced_response_omits_projection_fields_on_timeout_and_dispatch() {
    let timeout = serde_json::to_value(SendMailTracedResponse {
        status: "timeout".to_owned(),
        root: None,
        mails: None,
        tree: None,
        node_count: None,
        in_flight: None,
        replies: None,
    })
    .expect("timeout response serializes");
    let timeout = timeout.as_object().expect("timeout response object");
    assert!(timeout.get("mails").is_some_and(serde_json::Value::is_null));
    assert!(!timeout.contains_key("tree"));
    assert!(!timeout.contains_key("node_count"));

    let dispatched = serde_json::to_value(SendMailTracedResponse {
        status: "dispatched".to_owned(),
        root: Some(MailIdJson { sender: "aether.chassis".to_owned(), correlation_id: 1 }),
        mails: None,
        tree: None,
        node_count: None,
        in_flight: None,
        replies: None,
    })
    .expect("dispatched response serializes");
    let dispatched = dispatched.as_object().expect("dispatched response object");
    assert!(dispatched.get("mails").is_some_and(serde_json::Value::is_null));
    assert!(!dispatched.contains_key("tree"));
    assert!(!dispatched.contains_key("node_count"));
}
