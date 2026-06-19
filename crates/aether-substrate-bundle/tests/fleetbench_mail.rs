//! `FleetBench` mail + reply proofs (issue 1460, Tier-A): the rows that
//! share the settlement-aware reply-collection machinery over the real
//! hub → RPC → forked-headless stack. Three rows land as a unit —
//!
//! - **ping-pong** (the load-bearing one, deferred from #1451): a wasm
//!   component reply correlates home over the routed RPC path, plus the
//!   ADR-0090 typed-config round-trip;
//! - **`send_mail`**: a native-cap reply decodes + correlates;
//! - **`send_mail_traced`**: an atomic traced batch settles, yields its
//!   non-error ack root, and rides its correlated reply home.

mod fleetbench;

mod tests {
    use aether_data::{Kind, MailId};
    use aether_kinds::trace::DispatchTraced;
    use aether_kinds::{List, ListResult};
    use aether_test_fixtures_kinds::{ConfigEcho, ConfigQuery, ProbeConfig};

    use crate::fleetbench::{FleetBench, dist_manifest_present};

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
    fn fleetbench_pingpong_echoes_typed_config() {
        if !dist_manifest_present() {
            return;
        }
        let mut bench = FleetBench::start();
        let engine = bench.spawn_headless();
        let config = ProbeConfig {
            seed: 0x00C0_FFEE,
            label: "fleetbench".to_owned(),
        };
        let addr = bench.load_with_config_export(
            engine,
            "aether_test_fixtures_bundle",
            &config,
            "test_fixtures_probe_with_config",
        );

        let replies = bench.send(engine, &addr, &ConfigQuery);
        let reply = match replies.as_slice() {
            [one] => one,
            other => panic!(
                "ping-pong expected exactly one reply event, got {}",
                other.len(),
            ),
        };
        assert_eq!(
            reply.kind,
            ConfigEcho::ID,
            "the component reply should be a ConfigEcho",
        );
        let echo = ConfigEcho::decode_from_bytes(&reply.payload)
            .expect("the reply payload decodes as ConfigEcho");
        assert_eq!(
            echo,
            ConfigEcho {
                seed: config.seed,
                label: config.label.clone(),
            },
            "the echoed config should match the seeded ProbeConfig",
        );

        let query_record = bench
            .calls()
            .iter()
            .find(|record| record.request_kind == ConfigQuery::ID)
            .expect("the ConfigQuery round-trip is recorded as a CallRecord");
        assert_eq!(
            query_record.engine,
            Some(engine),
            "the ConfigQuery is routed to the forked engine",
        );
        assert_eq!(
            query_record.reply_kinds,
            vec![ConfigEcho::ID],
            "the ConfigQuery drew exactly one ConfigEcho reply",
        );
    }

    /// `send_mail` row: route an `fs::List` to the forked engine's
    /// `aether.fs` cap and assert the single reply decodes as a
    /// `ListResult` echoing the requested namespace. Both arms echo
    /// `namespace`, so the assertion is deterministic regardless of
    /// the save dir's contents — it proves schema-encode → route to a
    /// forked-engine native cap → reply decode + correlate (the
    /// non-component reply case; the component case is ping-pong).
    #[test]
    fn fleetbench_send_mail_decodes_fs_reply() {
        let mut bench = FleetBench::start();
        let engine = bench.spawn_headless();

        let replies = bench.send(
            engine,
            "aether.fs",
            &List {
                namespace: "save".to_owned(),
                prefix: String::new(),
            },
        );
        let reply = match replies.as_slice() {
            [one] => one,
            other => panic!(
                "send_mail expected exactly one reply event, got {}",
                other.len(),
            ),
        };
        assert_eq!(
            reply.kind,
            ListResult::ID,
            "the fs reply should be a ListResult",
        );
        assert_eq!(
            fs_reply_namespace(&reply.payload),
            "save",
            "the ListResult should echo the requested namespace",
        );

        let list_record = bench
            .calls()
            .iter()
            .find(|record| record.request_kind == List::ID)
            .expect("the fs List round-trip is recorded as a CallRecord");
        assert_eq!(
            list_record.engine,
            Some(engine),
            "the List call is routed to the forked engine",
        );
        assert_eq!(
            list_record.reply_kinds,
            vec![ListResult::ID],
            "the List call drew exactly one ListResult reply",
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
    fn fleetbench_send_traced_settles_and_collects_reply() {
        let mut bench = FleetBench::start();
        let engine = bench.spawn_headless();

        let (root, replies) = bench.send_traced(
            engine,
            "aether.fs",
            &List {
                namespace: "save".to_owned(),
                prefix: String::new(),
            },
        );
        assert_ne!(
            root,
            MailId::NONE,
            "the traced batch ack carries a non-sentinel chassis root",
        );

        let echoed = replies
            .iter()
            .find(|envelope| envelope.kind == ListResult::ID)
            .expect("the traced fs List drew a ListResult reply");
        assert_eq!(
            fs_reply_namespace(&echoed.payload),
            "save",
            "the traced ListResult should echo the requested namespace",
        );

        let traced_record = bench
            .calls()
            .iter()
            .find(|record| record.request_kind == DispatchTraced::ID)
            .expect("the traced dispatch is recorded as a CallRecord");
        assert_eq!(
            traced_record.engine,
            Some(engine),
            "the traced batch is routed to the forked engine",
        );
        assert!(
            traced_record.reply_kinds.contains(&ListResult::ID),
            "the traced call's reply stream includes the ListResult",
        );
    }

    /// Decode an `fs::List` reply and return the echoed namespace,
    /// matching either arm — both `Ok` and `Err` echo `namespace`,
    /// so the row's assertion is deterministic regardless of the save
    /// dir's contents.
    fn fs_reply_namespace(payload: &[u8]) -> String {
        match ListResult::decode_from_bytes(payload) {
            Some(ListResult::Ok { namespace, .. } | ListResult::Err { namespace, .. }) => namespace,
            None => panic!("undecodable ListResult"),
        }
    }
}
