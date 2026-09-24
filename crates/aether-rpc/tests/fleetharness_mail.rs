//! `FleetHarness` mail + reply proofs (issue 1460, Tier-A): the rows that
//! share the settlement-aware reply-collection machinery over the real
//! hub → RPC → forked-headless stack. Three rows land as a unit —
//!
//! - **ping-pong** (the load-bearing one, deferred from #1451): a wasm
//!   component reply correlates home over the routed RPC path, plus the
//!   ADR-0090 typed-config round-trip;
//! - **`send_mail`**: a native-cap reply decodes + correlates;
//! - **`send_mail_traced`**: an atomic traced batch settles, yields its
//!   non-error ack root, and rides its correlated reply home.
//!
//! The recipient-path rows (issue 6570) prove the wire boundary itself: a
//! short `ActorPath` reaches its actor through the hub, and an absent path
//! comes back as `RpcError::NotPresent` rather than a flattened `Other`.

mod tests {
    use aether_data::{Kind, MailId};
    use aether_fs::{List, ListResult, NamespaceAddr};
    use aether_kinds::trace::DispatchTraced;
    use aether_kinds::{Advance, AdvanceResult};
    use aether_rpc::RpcError;
    use aether_test_fixtures_kinds::{ConfigEcho, ConfigQuery, ProbeConfig};

    use aether_harness_fleet::{FleetHarness, dist_component_available};

    /// Ping-pong (verify-first, the #1451 deferral): load
    /// `ProbeWithConfig` from the `probe` bundle with a seeded `ProbeConfig`, send it a
    /// `ConfigQuery`, and assert the single `ConfigEcho` decodes back
    /// to the same `{ seed, label }`. This is the first end-to-end
    /// proof that a wasm guest reply correlates home over the real
    /// RPC stack (the server tags the injected Call with
    /// `reply_to = Component(rpc_server)`, so the guest's
    /// `ctx.reply_target()` is `Some` and the echo fires; the
    /// server's reply interception forwards it home as a
    /// `ReplyEvent`), and it round-trips the ADR-0090 typed-config
    /// path. Also asserts the recorded `CallRecord` captured the
    /// round-trip, keeping the benchmark-ready trace exercised by the
    /// mail rows.
    #[test]
    fn fleetharness_pingpong_echoes_typed_config() {
        if !dist_component_available("aether_test_fixtures_bundle") {
            return;
        }
        let mut harness = FleetHarness::start();
        let engine = harness.spawn_headless();
        let config = ProbeConfig { seed: 0x00C0_FFEE, label: "fleetharness".to_owned() };
        let addr =
            harness.load_with_config_export(engine, "aether_test_fixtures_bundle", &config, "test.probe_with_config");

        let replies = harness.send(engine, &addr, &ConfigQuery);
        let reply = match replies.as_slice() {
            [one] => one,
            other => panic!("ping-pong expected exactly one reply event, got {}", other.len()),
        };
        assert_eq!(reply.kind, ConfigEcho::ID, "the component reply should be a ConfigEcho");
        let echo = ConfigEcho::decode_from_bytes(&reply.payload).expect("the reply payload decodes as ConfigEcho");
        assert_eq!(
            echo,
            ConfigEcho { seed: config.seed, label: config.label.clone() },
            "the echoed config should match the seeded ProbeConfig",
        );

        let query_record = harness
            .calls()
            .iter()
            .find(|record| record.request_kind == ConfigQuery::ID)
            .expect("the ConfigQuery round-trip is recorded as a CallRecord");
        assert_eq!(query_record.engine, Some(engine), "the ConfigQuery is routed to the forked engine");
        assert_eq!(query_record.reply_kinds, vec![ConfigEcho::ID], "the ConfigQuery drew exactly one ConfigEcho reply");
    }

    /// `send_mail` row: route an `fs::List` to the forked engine's
    /// `aether.fs` cap and assert the single reply decodes as a
    /// `ListResult` echoing the requested namespace. Both arms echo
    /// `namespace`, so the assertion is deterministic regardless of
    /// the save dir's contents — it proves schema-encode → route to a
    /// forked-engine native cap → reply decode + correlate (the
    /// non-component reply case; the component case is ping-pong).
    #[test]
    fn fleetharness_send_mail_decodes_fs_reply() {
        let mut harness = FleetHarness::start();
        let engine = harness.spawn_headless();

        let replies = harness.send(engine, "aether.fs", &List { addr: NamespaceAddr::new("save", String::new()) });
        let reply = match replies.as_slice() {
            [one] => one,
            other => panic!("send_mail expected exactly one reply event, got {}", other.len()),
        };
        assert_eq!(reply.kind, ListResult::ID, "the fs reply should be a ListResult");
        assert_eq!(fs_reply_namespace(&reply.payload), "save", "the ListResult should echo the requested namespace");

        let list_record = harness
            .calls()
            .iter()
            .find(|record| record.request_kind == List::ID)
            .expect("the fs List round-trip is recorded as a CallRecord");
        assert_eq!(list_record.engine, Some(engine), "the List call is routed to the forked engine");
        assert_eq!(list_record.reply_kinds, vec![ListResult::ID], "the List call drew exactly one ListResult reply");
    }

    /// Issue 6419: a headless engine composes the fail-fast
    /// `aether.substrate_harness` stub, so an `advance` sent over the wire
    /// must draw exactly one `AdvanceResult::Err` before its `ReplyEnd`. The
    /// stub used to reply through the hub outbound, which drops a
    /// `Component` sender — and an rpc `Call` always names the rpc server's
    /// mailbox as its reply target — so the wire caller got zero replies
    /// instead of the failure the stub exists to deliver.
    #[test]
    fn fleetharness_substrate_harness_advance_fails_fast_on_headless() {
        let mut harness = FleetHarness::start();
        let engine = harness.spawn_headless();

        let replies = harness.send(engine, "aether.substrate_harness", &Advance { ticks: 1, delta_micros: 16_667 });
        let reply = match replies.as_slice() {
            [one] => one,
            other => panic!("advance expected exactly one reply event, got {}", other.len()),
        };
        assert_eq!(reply.kind, AdvanceResult::ID, "the stub reply should be an AdvanceResult");
        assert!(
            matches!(AdvanceResult::decode_from_bytes(&reply.payload), Some(AdvanceResult::Err { .. })),
            "a headless engine should refuse advance with AdvanceResult::Err",
        );
    }

    /// `send_mail_traced` row: dispatch a one-entry traced batch
    /// (`fs::List`) and assert the path settles, returns a non-error
    /// ack root, and collects the `ListResult` reply. The single
    /// settlement-bracketed wire `Call` yields all three — `call`'s
    /// read-until-`ReplyEnd` spans the settlement window (the server
    /// holds the `Call` open via its `SettlementHold`), so no new
    /// wire read loop is needed.
    #[test]
    fn fleetharness_send_traced_settles_and_collects_reply() {
        let mut harness = FleetHarness::start();
        let engine = harness.spawn_headless();

        let (root, replies) =
            harness.send_traced(engine, "aether.fs", &List { addr: NamespaceAddr::new("save", String::new()) });
        assert_ne!(root, MailId::NONE, "the traced batch ack carries a non-sentinel chassis root");

        let echoed = replies
            .iter()
            .find(|envelope| envelope.kind == ListResult::ID)
            .expect("the traced fs List drew a ListResult reply");
        assert_eq!(
            fs_reply_namespace(&echoed.payload),
            "save",
            "the traced ListResult should echo the requested namespace",
        );

        let traced_record = harness
            .calls()
            .iter()
            .find(|record| record.request_kind == DispatchTraced::ID)
            .expect("the traced dispatch is recorded as a CallRecord");
        assert_eq!(traced_record.engine, Some(engine), "the traced batch is routed to the forked engine");
        assert!(
            traced_record.reply_kinds.contains(&ListResult::ID),
            "the traced call's reply stream includes the ListResult",
        );
    }

    /// Issue 6570: a wire `Call` names its recipient by `ActorPath`, and the
    /// engine that hosts it expands an ADR-0166 short path on arrival. Load
    /// the probe, then address it only as `aether.component/:NAME` and expect
    /// its one `ConfigEcho`. Fails if any hop — the harness, the hub, or the
    /// proxy — hashes or folds the recipient text instead of carrying the
    /// path to the engine, because a folded short path names no mailbox.
    #[test]
    fn fleetharness_short_path_reaches_the_component() {
        if !dist_component_available("aether_test_fixtures_bundle") {
            return;
        }
        let mut harness = FleetHarness::start();
        let engine = harness.spawn_headless();
        let config = ProbeConfig { seed: 7, label: "short-path".to_owned() };
        harness.load_with_config_export(engine, "aether_test_fixtures_bundle", &config, "test.probe_with_config");

        let replies = harness
            .try_send(engine, "aether.component/:test.probe_with_config", &ConfigQuery)
            .expect("the short path reaches the loaded probe");
        assert!(matches!(replies.as_slice(), [one] if one.kind == ConfigEcho::ID), "one ConfigEcho: {replies:?}");
    }

    /// Issue 6570: a path that resolves to no actor in the engine closes the
    /// call with `RpcError::NotPresent` naming the path, and the hub relays
    /// the engine's refusal unchanged. Fails if the proxy or the hub flattens
    /// the refusal into `RpcError::Other`, or if the engine reports some
    /// other variant or path.
    #[test]
    fn fleetharness_absent_path_is_not_present_through_the_hub() {
        let mut harness = FleetHarness::start();
        let engine = harness.spawn_headless();
        let absent = "aether.component/aether.embedded:never-loaded";

        let error = harness.try_send(engine, absent, &ConfigQuery).expect_err("an absent path is refused");
        assert!(
            matches!(&error, RpcError::NotPresent { path, .. } if path.to_string() == absent),
            "the engine's NotPresent reaches the caller through the hub: {error:?}",
        );
    }

    /// Decode an `fs::List` reply and return the echoed namespace,
    /// matching either arm — both `Ok` and `Err` echo `addr`,
    /// so the row's assertion is deterministic regardless of the save
    /// dir's contents.
    fn fs_reply_namespace(payload: &[u8]) -> String {
        match ListResult::decode_from_bytes(payload) {
            Some(ListResult::Ok { addr, .. } | ListResult::Err { addr, .. }) => addr.namespace,
            None => panic!("undecodable ListResult"),
        }
    }
}
