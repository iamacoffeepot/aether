use super::super::mail::settle_mail_item;
#[allow(clippy::wildcard_imports)]
use super::super::test_support::*;
#[allow(clippy::wildcard_imports)]
use super::super::*;
use std::collections::VecDeque;
use tokio::task::yield_now;
use tokio::time::timeout;

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

/// Fire-and-forget does not skip the engine-owned recipient resolution added
/// by issue 4057. An unknown engine therefore fails during preparation instead
/// of reporting `dispatched`; only the application mail's settlement is
/// omitted.
#[tokio::test]
async fn send_mail_fire_and_forget_rejects_unknown_engine_during_resolution() {
    use std::time::Instant;

    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let started = Instant::now();
    let out = mcp
        .send_mail(Parameters(SendMailArgs {
            mails: vec![MailSpec {
                // Recipient resolution is routed first and reports that this
                // engine is not supervised before the application mail fires.
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
    assert!(
        statuses[0].status.starts_with("error: "),
        "fire-and-forget may await engine resolution and therefore rejects an unknown engine"
    );
    assert!(statuses[0].replies.is_empty(), "fire-and-forget carries no replies");
    assert!(!statuses[0].timed_out, "dispatch is not a timeout");
}

#[tokio::test]
async fn direct_mail_uses_the_engine_answer_and_named_mail_skips_pre_resolution() {
    let supplied = "aether.test://camera";
    let canonical = "aether.test/aether.test.child:camera";
    let engine_answer = MailboxId(0x4057_0000_0000_0001);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (_chassis, port) = boot_hub_with_address_route_loopback(engine_answer, canonical, Arc::clone(&calls));
    let mcp = connect_mcp(port);
    let engine = EngineId(Uuid::from_u128(0x4057));

    let prepared = mcp
        .prepare_direct_mail(MailSpec {
            engine_id: engine.0.to_string(),
            mail: EngineMailSpec {
                recipient_name: supplied.to_owned(),
                kind_name: "aether.fs.list".to_owned(),
                params: Some(serde_json::json!({ "namespace": "save", "prefix": "" })),
            },
        })
        .await
        .expect("direct mail prepares through the engine resolver");
    assert_eq!(prepared.envelope.to.mailbox, engine_answer);
    assert_eq!(prepared.resolved_mailbox_id, engine_answer);
    assert_eq!(prepared.canonical_recipient, canonical);
    assert_eq!(calls.lock().expect("address-route calls mutex is never poisoned").len(), 1);

    calls.lock().expect("address-route calls mutex is never poisoned").clear();
    let bundle = mcp
        .encode_mail_bundle(
            engine,
            &[EngineMailSpec {
                recipient_name: supplied.to_owned(),
                kind_name: "aether.fs.list".to_owned(),
                params: Some(serde_json::json!({ "namespace": "save", "prefix": "" })),
            }],
        )
        .await
        .expect("NamedMail bundle encodes without recipient pre-resolution");
    assert_eq!(bundle[0].recipient_name, supplied);
    assert!(
        calls.lock().expect("address-route calls mutex is never poisoned").is_empty(),
        "NamedMail remains engine-atomic and never calls the pre-resolver"
    );
}

#[tokio::test]
async fn settled_mail_reads_the_declared_reply_contract_from_the_engine_resolved_mailbox() {
    let supplied = "aether.test://declared-reply";
    let canonical = "aether.test/aether.test.child:declared-reply";
    let engine_answer = MailboxId(0x4057_0000_0000_0003);
    #[allow(clippy::disallowed_methods)]
    let locally_folded = mailbox_id_from_path(supplied);
    assert_ne!(engine_answer, locally_folded, "test answer must expose accidental client-side folding");

    let reply_descriptor =
        KindDescriptor { name: "aether.test.component.reply".to_owned(), schema: SchemaType::String };
    let reply_kind_id = KindId(kind_id_from_parts(&reply_descriptor.name, &reply_descriptor.schema));
    let reply_params = serde_json::json!("decoded through engine-resolved handler contract");
    let replies = Arc::new(Mutex::new(VecDeque::from([ScriptedRouteReply {
        events: vec![ScriptedReplyEvent {
            kind: reply_kind_id,
            payload: aether_codec::encode_schema(&reply_params, &reply_descriptor.schema)
                .expect("component reply schema encodes"),
        }],
        settle: true,
    }])));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (_chassis, port) = boot_hub_with_address_route_replies(engine_answer, canonical, Arc::clone(&calls), replies);
    let mcp = connect_mcp(port);
    let engine = EngineId(Uuid::from_u128(0x4057));
    mcp.prefill_engine(engine);
    mcp.merge_into_engine_cache(engine, vec![reply_descriptor.clone()]);
    let request_descriptor = mcp.cache_lookup(engine, "aether.fs.list").expect("static request descriptor is cached");
    let request_kind_id = KindId(kind_id_from_parts(&request_descriptor.name, &request_descriptor.schema));
    mcp.components.lock().expect("component cache mutex is never poisoned").insert(
        (engine, engine_answer),
        ComponentCapabilities {
            handlers: vec![HandlerCapability {
                id: request_kind_id,
                name: "aether.fs.list".to_owned(),
                doc: None,
                reply: aether_data::ReplyContract::One(reply_kind_id),
            }],
            ..ComponentCapabilities::default()
        },
    );
    assert!(
        !mcp.components
            .lock()
            .expect("component cache mutex is never poisoned")
            .contains_key(&(engine, locally_folded)),
        "only the engine-returned mailbox id owns the handler contract"
    );

    let status = settle_mail_item(
        &mcp,
        0,
        MailSpec {
            engine_id: engine.0.to_string(),
            mail: EngineMailSpec {
                recipient_name: supplied.to_owned(),
                kind_name: "aether.fs.list".to_owned(),
                params: Some(serde_json::json!({ "namespace": "save", "prefix": "" })),
            },
        },
        ReplyProjection::All,
    )
    .await;

    assert_eq!(status.status, "delivered");
    assert_eq!(status.replies.len(), 1);
    assert_eq!(status.replies[0].kind_name.as_deref(), Some(reply_descriptor.name.as_str()));
    assert_eq!(status.replies[0].params.as_ref(), Some(&reply_params));
    assert!(status.replies[0].payload_bytes.is_none(), "declared component reply decoded without base64 fallback");
    let calls = calls.lock().expect("address-route calls mutex is never poisoned");
    assert_eq!(calls.len(), 2, "one resolver RPC precedes one ordinary application delivery");
    assert_eq!(calls[0].kind, ResolveAddress::ID);
    assert_eq!(calls[1].mailbox, engine_answer, "ordinary send routes to the engine-returned mailbox id");
    drop(calls);
}

#[tokio::test]
async fn fire_and_forget_awaits_resolution_but_not_application_settlement() {
    let engine_answer = MailboxId(0x4057_0000_0000_0002);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (_chassis, port) = boot_hub_with_address_route_loopback(engine_answer, "aether.fs", Arc::clone(&calls));
    let mcp = connect_mcp(port);
    let engine = EngineId(Uuid::from_u128(0x4057));

    mcp.deliver_one_fire(MailSpec {
        engine_id: engine.0.to_string(),
        mail: EngineMailSpec {
            recipient_name: "aether.fs".to_owned(),
            kind_name: "aether.fs.list".to_owned(),
            params: Some(serde_json::json!({ "namespace": "save", "prefix": "" })),
        },
    })
    .await
    .expect("resolver settles before the application mail is fired");

    timeout(Duration::from_secs(2), async {
        loop {
            if calls.lock().expect("address-route calls mutex is never poisoned").len() >= 2 {
                break;
            }
            yield_now().await;
        }
    })
    .await
    .expect("fired application envelope reaches route sink");
    let calls = calls.lock().expect("address-route calls mutex is never poisoned");
    assert_eq!(calls[0].kind, ResolveAddress::ID);
    assert_eq!(calls[1].mailbox, engine_answer);
    drop(calls);
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
