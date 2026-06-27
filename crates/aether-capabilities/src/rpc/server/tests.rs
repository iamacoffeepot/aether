// Test harness resolves echo/target actor mailboxes by their NAMESPACE to
// address Call frames — reference id derivation, not sibling-cap addressing.
#![allow(clippy::disallowed_methods)]
use super::*;
use crate::rpc::{Hello, HelloAck, PeerKind, WIRE_VERSION, WireFrame};
use crate::test_chassis::{TestChassis, fresh_substrate};
use aether_codec::frame::{read_frame, write_frame};
use aether_substrate::chassis::builder::Builder;
use aether_substrate::chassis::builder::PassiveChassis;
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

fn test_peer_kind() -> PeerKind {
    PeerKind::Substrate {
        engine_name: "test".into(),
        engine_version: "0.1.0".into(),
        kinds: vec![],
    }
}

/// Boot a chassis hosting only `RpcServerCapability`, connect a
/// client `TcpStream` to its OS-picked port, and apply
/// `read_timeout`. Tests that need additional caps (e.g.
/// `TestEchoActor`, `TraceDispatchCapability`) build their own
/// chassis and reach for [`connect_to_rpc_server`] for the
/// connect / timeout half. Returns `(chassis, stream)`; both must
/// stay alive for the listener to keep accepting.
fn boot_with_rpc_server_only(timeout: Duration) -> (PassiveChassis<TestChassis>, TcpStream) {
    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<RpcServerCapability>(RpcServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            peer_kind: test_peer_kind(),
        })
        .build_passive()
        .expect("rpc server boots");
    let stream = connect_to_rpc_server(&chassis, timeout);
    (chassis, stream)
}

/// Boot a chassis with the deferred-echo actor + trace dispatch
/// behind the RPC server, connect a client, and complete the
/// handshake. Shared by the deferred-reply settlement tests. Returns
/// `(chassis, stream)`; both must stay alive for the listener.
fn boot_with_deferred_echo(timeout: Duration) -> (PassiveChassis<TestChassis>, TcpStream) {
    use crate::rpc::test_echo::DeferredEchoActor;
    use crate::trace::TraceDispatchCapability;

    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<TraceDispatchCapability>(())
        .with_actor::<DeferredEchoActor>(())
        .with_actor::<RpcServerCapability>(RpcServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            peer_kind: test_peer_kind(),
        })
        .build_passive()
        .expect("caps boot");
    let mut stream = connect_to_rpc_server(&chassis, timeout);
    complete_handshake(&mut stream);
    (chassis, stream)
}

/// Lift the published `RpcServerHandle`'s `local_port`, open a
/// `TcpStream`, set `read_timeout`. Shared by every test whose
/// boot path is more elaborate than `boot_with_rpc_server_only`.
fn connect_to_rpc_server(chassis: &PassiveChassis<TestChassis>, timeout: Duration) -> TcpStream {
    let port = chassis
        .handle::<RpcServerHandle>()
        .expect("RpcServerHandle published")
        .local_port;
    let stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to rpc server");
    stream
        .set_read_timeout(Some(timeout))
        .expect("test: set_read_timeout on TcpStream");
    stream
}

/// Send a `Hello` carrying the current `WIRE_VERSION` and drain
/// the resulting `HelloAck` so subsequent test traffic sees a
/// clean stream. Tests that want to assert specifically against
/// the handshake reply (handshake_*_roundtrip,
/// `wire_version_mismatch_*`) write the `Hello` themselves so the
/// `HelloAck` / `Bye` can be matched on.
fn complete_handshake(stream: &mut TcpStream) {
    write_frame(
        stream,
        &WireFrame::Hello(Hello {
            wire_version: WIRE_VERSION,
            peer: PeerKind::Client {
                client_name: "test-client".into(),
                client_version: "0.0.1".into(),
            },
        }),
    )
    .expect("test: write_frame Hello to rpc server");
    let _: WireFrame = read_frame(stream).expect("test: read_frame after Hello returns HelloAck");
}

/// Boot a `RpcServerCapability` bound to OS-picked port, connect a
/// real TCP client, exchange `Hello` for `HelloAck`. Sanity-check
/// the wire's framing + handshake path end-to-end.
#[test]
fn handshake_hello_to_hello_ack_roundtrip() {
    // Specifically tests the handshake path end-to-end, so it
    // writes the `Hello` itself rather than using
    // `complete_handshake` (which would discard the `HelloAck`
    // before the asserts can inspect it).
    let (_chassis, mut stream) = boot_with_rpc_server_only(Duration::from_secs(2));
    write_frame(
        &mut stream,
        &WireFrame::Hello(Hello {
            wire_version: WIRE_VERSION,
            peer: PeerKind::Client {
                client_name: "test-client".into(),
                client_version: "0.0.1".into(),
            },
        }),
    )
    .expect("write Hello");

    let reply: WireFrame = read_frame(&mut stream).expect("read HelloAck");
    match reply {
        WireFrame::HelloAck(HelloAck {
            wire_version,
            server,
        }) => {
            assert_eq!(wire_version, WIRE_VERSION);
            match server {
                PeerKind::Substrate { engine_name, .. } => {
                    assert_eq!(engine_name, "test");
                }
                PeerKind::Client { .. } => panic!("expected Substrate peer kind"),
            }
        }
        other => panic!("expected HelloAck, got {other:?}"),
    }
}

/// `Ping(token)` round-trips as `Pong(token)`.
#[test]
fn ping_pong_roundtrip() {
    let (_chassis, mut stream) = boot_with_rpc_server_only(Duration::from_secs(2));
    complete_handshake(&mut stream);

    write_frame(&mut stream, &WireFrame::Ping(0x00c0_ffee)).expect("write Ping");
    let reply: WireFrame = read_frame(&mut stream).expect("read Pong");
    assert_eq!(reply, WireFrame::Pong(0x00c0_ffee));
}

/// End-to-end Call dispatch: connect, handshake, fire a `Call`
/// addressed at the test echo actor's `TestEchoRequest` kind,
/// observe a `ReplyEvent { TestEchoReply }` followed by a
/// `ReplyEnd { Ok(()) }` when the chain settles. Exercises the
/// full dispatch / settlement / reply-interception path from
/// phase 2.
#[test]
fn call_echo_round_trip_event_then_end() {
    use crate::rpc::test_echo::{TestEchoActor, TestEchoReply, TestEchoRequest};
    use crate::rpc::{MailEnvelope, MailboxAddress};
    use crate::trace::TraceDispatchCapability;
    use aether_actor::Addressable;
    use aether_data::{Kind, mailbox_id_from_name};

    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        // TraceObserver folds substrate-wide trace events into per-
        // root counters and fires `Settled { root }` mail at the
        // chassis-mailbox once a root drains. Without it,
        // RpcServer's settlement subscription never wakes and
        // the `Call` never produces a `ReplyEnd`.
        .with_actor::<TraceDispatchCapability>(())
        .with_actor::<TestEchoActor>(())
        .with_actor::<RpcServerCapability>(RpcServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            peer_kind: test_peer_kind(),
        })
        .build_passive()
        .expect("caps boot");

    let mut stream = connect_to_rpc_server(&chassis, Duration::from_secs(5));
    complete_handshake(&mut stream);

    // Fire a Call against the echo actor. cid = 0xabc; the cap
    // correlates and ends with ReplyEnd matching the same cid.
    let echo_payload = TestEchoRequest { value: 42 }.encode_into_bytes();
    let echo_mailbox = mailbox_id_from_name(<TestEchoActor as Addressable>::NAMESPACE);
    write_frame(
        &mut stream,
        &WireFrame::Call {
            cid: Some(0xabc),
            envelope: MailEnvelope {
                to: MailboxAddress::local(echo_mailbox),
                from: None,
                kind: <TestEchoRequest as Kind>::ID,
                correlation_id: None,
                payload: echo_payload,
            },
        },
    )
    .expect("test: write_frame Call to rpc server");

    // First frame back should be the ReplyEvent carrying the
    // TestEchoReply with the echoed value.
    let event: WireFrame = read_frame(&mut stream).expect("read ReplyEvent");
    let envelope = match event {
        WireFrame::ReplyEvent { cid, envelope } => {
            assert_eq!(cid, 0xabc);
            envelope
        }
        other => panic!("expected ReplyEvent, got {other:?}"),
    };
    assert_eq!(envelope.kind, <TestEchoReply as Kind>::ID);
    let decoded = TestEchoReply::decode_from_bytes(&envelope.payload).expect("decode reply");
    assert_eq!(decoded.value, 42);

    // Then the ReplyEnd closes the call.
    let end: WireFrame = read_frame(&mut stream).expect("read ReplyEnd");
    match end {
        WireFrame::ReplyEnd { cid, result } => {
            assert_eq!(cid, 0xabc);
            result.expect("ReplyEnd result Ok");
        }
        other => panic!("expected ReplyEnd, got {other:?}"),
    }
}

/// iamacoffeepot/aether#1321 regression: a `Call` routed through the
/// RPC server tags its reply `SourceAddr::Component(rpc_server)`, so
/// a capability that replies via `HubOutbound::send_reply` (which
/// only routes `Session` / `EngineMailbox`) drops the reply silently —
/// the same drop #1316/#1319 fixed for the desktop driver. The
/// `HeadlessWindowCapability` `Err`-replies on `set_window_mode`; with
/// the bug present this `Call` would yield a bare `ReplyEnd` and zero
/// `ReplyEvent`s. Routing through the `Mailer` (the complete router)
/// pushes the reply back locally to the server's `on_any`, so the
/// `Err` rides home as a `ReplyEvent` before the `ReplyEnd`.
#[test]
fn call_headless_window_set_mode_err_reaches_component_reply() {
    use crate::rpc::{MailEnvelope, MailboxAddress};
    use crate::trace::TraceDispatchCapability;
    use crate::window::HeadlessWindowCapability;
    use aether_actor::Addressable;
    use aether_data::{Kind, mailbox_id_from_name};
    use aether_kinds::{SetWindowMode, SetWindowModeResult, WindowMode};

    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<TraceDispatchCapability>(())
        .with_actor::<HeadlessWindowCapability>(())
        .with_actor::<RpcServerCapability>(RpcServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            peer_kind: test_peer_kind(),
        })
        .build_passive()
        .expect("caps boot");

    let mut stream = connect_to_rpc_server(&chassis, Duration::from_secs(5));
    complete_handshake(&mut stream);

    let payload = SetWindowMode {
        mode: WindowMode::Windowed,
        width: None,
        height: None,
    }
    .encode_into_bytes();
    let window_mailbox = mailbox_id_from_name(<HeadlessWindowCapability as Addressable>::NAMESPACE);
    write_frame(
        &mut stream,
        &WireFrame::Call {
            cid: Some(0xdef),
            envelope: MailEnvelope {
                to: MailboxAddress::local(window_mailbox),
                from: None,
                kind: <SetWindowMode as Kind>::ID,
                correlation_id: None,
                payload,
            },
        },
    )
    .expect("test: write_frame Call to rpc server");

    // The `Err` reply must arrive as a ReplyEvent — the drop this
    // test guards against would leave zero events before ReplyEnd.
    let event: WireFrame = read_frame(&mut stream).expect("read ReplyEvent");
    let envelope = match event {
        WireFrame::ReplyEvent { cid, envelope } => {
            assert_eq!(cid, 0xdef);
            envelope
        }
        other => panic!("expected ReplyEvent, got {other:?}"),
    };
    assert_eq!(envelope.kind, <SetWindowModeResult as Kind>::ID);
    let decoded = SetWindowModeResult::decode_from_bytes(&envelope.payload)
        .expect("decode SetWindowModeResult");
    assert!(
        matches!(decoded, SetWindowModeResult::Err { .. }),
        "headless window cap replies Err, got {decoded:?}"
    );

    let end: WireFrame = read_frame(&mut stream).expect("read ReplyEnd");
    match end {
        WireFrame::ReplyEnd { cid, result } => {
            assert_eq!(cid, 0xdef);
            result.expect("ReplyEnd result Ok");
        }
        other => panic!("expected ReplyEnd, got {other:?}"),
    }
}

/// iamacoffeepot/aether#1031 end-to-end: a `Call` against an actor
/// that replies through the ADR-0093 hold-until-resolve dispatch
/// (spawned worker -> completion wake -> re-reply) must still
/// produce a `ReplyEvent` followed by a `ReplyEnd`. The settlement
/// hold keeps the chain open across the spawn, so the RPC server's
/// settlement subscription wakes only *after* the deferred reply
/// arrives — not when the handler returns. Pre-fix the chain settled
/// the instant `on_deferred_echo` returned and the deferred reply
/// landed in an already-closed call (no `ReplyEvent`, only a bare
/// `ReplyEnd`, then the late reply dropped).
#[test]
fn call_deferred_echo_settles_after_reply() {
    use crate::rpc::test_echo::{DeferredEchoActor, DeferredEchoReply, DeferredEchoRequest};
    use crate::rpc::{MailEnvelope, MailboxAddress};
    use aether_actor::Addressable;
    use aether_data::{Kind, mailbox_id_from_name};

    let (_chassis, mut stream) = boot_with_deferred_echo(Duration::from_secs(5));

    let payload = DeferredEchoRequest { value: 99 }.encode_into_bytes();
    let mailbox = mailbox_id_from_name(<DeferredEchoActor as Addressable>::NAMESPACE);
    write_frame(
        &mut stream,
        &WireFrame::Call {
            cid: Some(0xdef),
            envelope: MailEnvelope {
                to: MailboxAddress::local(mailbox),
                from: None,
                kind: <DeferredEchoRequest as Kind>::ID,
                correlation_id: None,
                payload,
            },
        },
    )
    .expect("test: write_frame Call to rpc server");

    // The deferred reply arrives as a ReplyEvent — proving the chain
    // stayed open long enough for the spawned worker's reply to be
    // intercepted (not dropped into an already-settled call).
    let event: WireFrame = read_frame(&mut stream).expect("read ReplyEvent");
    let envelope = match event {
        WireFrame::ReplyEvent { cid, envelope } => {
            assert_eq!(cid, 0xdef);
            envelope
        }
        other => panic!("expected ReplyEvent for the deferred reply, got {other:?}"),
    };
    assert_eq!(envelope.kind, <DeferredEchoReply as Kind>::ID);
    let decoded =
        DeferredEchoReply::decode_from_bytes(&envelope.payload).expect("decode deferred reply");
    assert_eq!(decoded.value, 99);

    // ReplyEnd follows — settlement fired after the deferred reply,
    // not when the handler returned.
    let end: WireFrame = read_frame(&mut stream).expect("read ReplyEnd");
    match end {
        WireFrame::ReplyEnd { cid, result } => {
            assert_eq!(cid, 0xdef);
            result.expect("ReplyEnd result Ok");
        }
        other => panic!("expected ReplyEnd, got {other:?}"),
    }
}

/// A `Call` carrying a `DispatchTraced` batch with **two**
/// `DeferredEchoRequest` envelopes — the empirical `send_mail_traced`
/// failure shape: each child is itself a deferred-reply path
/// (spawn → loopback → re-reply), routed through the trace cap rather
/// than directly. Pre-fix the trace cap dispatched each child via
/// `ctx.send_envelope_traced` which stamps `reply_to` at the
/// dispatcher's own mailbox (the `push_envelope_buffered` default);
/// child deferred replies landed at the trace cap, which has no
/// handler for the reply kind and no `#[fallback]`, so they were
/// silently dropped. The wire call closed via the (still correct)
/// settlement signal with `replies: []`. The fix forwards each
/// child's `reply_to` to the trace cap's own inbound `reply_target`
/// (typically the RPC server holding the wire `cid`'s in-flight
/// entry), so child replies — sync or deferred — bubble through to
/// the wire as `ReplyEvent`s, and settlement still fires only after
/// each hold-until-resolve dispatch's hold drops.
///
/// Test asserts: TWO `ReplyEvent`s (one `DeferredEchoReply` per
/// request), then exactly ONE `ReplyEnd`. Order of the two events is
/// unspecified (the two deferred-echo handlers run in parallel
/// behind 50ms sleeps); the test pairs by `value`.
#[test]
fn dispatch_traced_with_deferred_replies_routes_each_event_then_settles() {
    use crate::rpc::test_echo::{DeferredEchoActor, DeferredEchoReply, DeferredEchoRequest};
    use crate::rpc::{MailEnvelope, MailboxAddress};
    use crate::trace::TraceDispatchCapability;
    use aether_actor::Addressable;
    use aether_data::{Kind, mailbox_id_from_name};
    use aether_kinds::NamedMail;
    use aether_kinds::trace::DispatchTraced;

    let (_chassis, mut stream) = boot_with_deferred_echo(Duration::from_secs(10));

    // Build a batched DispatchTraced with two DeferredEchoRequest
    // envelopes, addressed at the deferred-echo actor by name (the
    // trace cap resolves names through the registry).
    let batch = DispatchTraced {
        mails: vec![
            NamedMail {
                recipient_name: <DeferredEchoActor as Addressable>::NAMESPACE.into(),
                kind_name: <DeferredEchoRequest as Kind>::NAME.into(),
                payload: DeferredEchoRequest { value: 11 }.encode_into_bytes(),
                count: 1,
            },
            NamedMail {
                recipient_name: <DeferredEchoActor as Addressable>::NAMESPACE.into(),
                kind_name: <DeferredEchoRequest as Kind>::NAME.into(),
                payload: DeferredEchoRequest { value: 22 }.encode_into_bytes(),
                count: 1,
            },
        ],
    };
    let trace_mailbox = mailbox_id_from_name(<TraceDispatchCapability as Addressable>::NAMESPACE);
    let payload = batch.encode_into_bytes();
    write_frame(
        &mut stream,
        &WireFrame::Call {
            cid: Some(0xbeef),
            envelope: MailEnvelope {
                to: MailboxAddress::local(trace_mailbox),
                from: None,
                kind: <DispatchTraced as Kind>::ID,
                correlation_id: None,
                payload,
            },
        },
    )
    .expect("test: write_frame Call DispatchTraced to rpc server");

    // The trace cap's synchronous `DispatchTracedAck::Ok` reply
    // arrives as a ReplyEvent. Drain it before scanning for the two
    // deferred replies — its ordering is well-defined (the trace
    // handler replies before the children run), so we can read it
    // first without an unbound search.
    let mut deferred_values: Vec<u64> = Vec::new();
    let mut saw_ack = false;
    // Drain up to 4 ReplyEvent frames (ack + 2 deferred + safety
    // margin) before the ReplyEnd. Each iteration consumes one
    // frame; the ReplyEnd breaks.
    loop {
        let frame: WireFrame = read_frame(&mut stream).expect("read frame");
        match frame {
            WireFrame::ReplyEvent { cid, envelope } => {
                assert_eq!(cid, 0xbeef);
                if envelope.kind == <DeferredEchoReply as Kind>::ID {
                    let decoded = DeferredEchoReply::decode_from_bytes(&envelope.payload)
                        .expect("decode deferred reply");
                    deferred_values.push(decoded.value);
                } else {
                    // Otherwise this is the DispatchTracedAck::Ok
                    // reply; mark it observed but don't assert on
                    // its payload here (the ack carries the root
                    // MailId; the test's load-bearing assertions are
                    // on the deferred-reply payloads).
                    saw_ack = true;
                }
            }
            WireFrame::ReplyEnd { cid, result } => {
                assert_eq!(cid, 0xbeef);
                result.expect("ReplyEnd result Ok");
                break;
            }
            other => panic!("expected ReplyEvent / ReplyEnd, got {other:?}"),
        }
    }
    assert!(
        saw_ack,
        "expected DispatchTracedAck reply event before ReplyEnd",
    );
    deferred_values.sort_unstable();
    assert_eq!(
        deferred_values,
        vec![11, 22],
        "expected one DeferredEchoReply per request, sorted by value",
    );
}

/// Fire-and-forget `Call { cid: None }` skips reply correlation
/// entirely — no settlement subscription is created, no
/// `ReplyEnd` is written. Verify by sending a Call with cid None
/// at the test echo actor (whose reply would otherwise come back
/// as a `ReplyEvent` if correlation had leaked) and confirming a
/// subsequent `Ping(token)` round-trips immediately, which proves
/// no stale `ReplyEvent` / `ReplyEnd` frames are in the way.
#[test]
fn call_without_cid_is_fire_and_forget() {
    use crate::rpc::test_echo::{TestEchoActor, TestEchoRequest};
    use crate::rpc::{MailEnvelope, MailboxAddress};
    use aether_actor::Addressable;
    use aether_data::{Kind, mailbox_id_from_name};

    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<TestEchoActor>(())
        .with_actor::<RpcServerCapability>(RpcServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            peer_kind: test_peer_kind(),
        })
        .build_passive()
        .expect("caps boot");

    let mut stream = connect_to_rpc_server(&chassis, Duration::from_secs(2));
    complete_handshake(&mut stream);

    // Fire-and-forget Call (cid = None). The echo actor will
    // still reply, but with cid None there's no in-flight entry
    // so the reply has no matching correlation and gets dropped.
    let echo_payload = TestEchoRequest { value: 7 }.encode_into_bytes();
    let echo_mailbox = mailbox_id_from_name(<TestEchoActor as Addressable>::NAMESPACE);
    write_frame(
        &mut stream,
        &WireFrame::Call {
            cid: None,
            envelope: MailEnvelope {
                to: MailboxAddress::local(echo_mailbox),
                from: None,
                kind: <TestEchoRequest as Kind>::ID,
                correlation_id: None,
                payload: echo_payload,
            },
        },
    )
    .expect("test: write_frame fire-and-forget Call to rpc server");

    // Immediately Ping. If the fire-and-forget Call had leaked
    // reply correlation, a ReplyEvent / ReplyEnd would arrive
    // before the Pong. Asserting we see Pong first proves no leak.
    write_frame(&mut stream, &WireFrame::Ping(0x00c0_ffee))
        .expect("test: write_frame Ping to rpc server");
    let reply: WireFrame = read_frame(&mut stream).expect("read Pong");
    assert_eq!(reply, WireFrame::Pong(0x00c0_ffee));
}

/// A `Hello` carrying a mismatched `wire_version` triggers a `Bye`
/// and connection close on the server side.
#[test]
fn wire_version_mismatch_kicks_connection() {
    // Sends a deliberately wrong `wire_version` and asserts the
    // server responds with `Bye`, so it can't use
    // `complete_handshake` (which sends the current version).
    let (_chassis, mut stream) = boot_with_rpc_server_only(Duration::from_secs(2));
    write_frame(
        &mut stream,
        &WireFrame::Hello(Hello {
            wire_version: WIRE_VERSION + 1,
            peer: PeerKind::Client {
                client_name: "future-client".into(),
                client_version: "9.9.9".into(),
            },
        }),
    )
    .expect("test: write_frame future-version Hello to rpc server");

    let reply: WireFrame = read_frame(&mut stream).expect("read Bye");
    match reply {
        WireFrame::Bye { reason } => {
            assert!(
                reason.contains("wire_version"),
                "Bye reason should mention wire_version: {reason}",
            );
        }
        other => panic!("expected Bye, got {other:?}"),
    }
}

/// iamacoffeepot/aether#1271: an inbound frame whose announced
/// length exceeds the framing cap but is within the drain ceiling
/// (`size <= 2 * max`) is fail-soft. The server drains the body,
/// writes a `ReplyEnd { cid: 0, Err(RpcError::FrameTooLarge) }`,
/// and keeps the connection alive — a follow-up `Ping` round-trips
/// as `Pong`, proving the session survived.
#[test]
fn oversize_frame_replies_with_frame_too_large_and_session_survives() {
    use crate::rpc::RpcError;
    use aether_codec::frame::{MAX_FRAME_SIZE, max_frame_size};
    use std::io::Write;

    let (_chassis, mut stream) = boot_with_rpc_server_only(Duration::from_secs(5));
    complete_handshake(&mut stream);

    // Set the read timeout high — the server has to read the full
    // oversize body off the wire before it can write the error
    // reply, so the read for the ReplyEnd is gated on that drain.
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .expect("set_write_timeout");

    // Announce a body just over the cap, then push that many zero
    // bytes. The cap defaults to 64 MiB (MAX_FRAME_SIZE), and the
    // process-wide accessor caches on first read — so the drain
    // ceiling is exactly `2 * max_frame_size()`. Pick the smallest
    // legal oversize: max + 1.
    let max = max_frame_size();
    assert!(max >= MAX_FRAME_SIZE, "cap accessor lifted below default");
    let oversize: usize = max + 1;
    assert!(
        oversize <= max.saturating_mul(2),
        "test size must be inside the drain ceiling",
    );
    #[allow(clippy::cast_possible_truncation)]
    let prefix = (oversize as u32).to_le_bytes();
    stream
        .write_all(&prefix)
        .expect("write oversize length prefix");
    // Write the body in chunks so a 64 MiB+ payload doesn't single-
    // syscall through.
    let chunk = vec![0u8; 1024 * 1024];
    let mut remaining = oversize;
    while remaining > 0 {
        let n = remaining.min(chunk.len());
        stream
            .write_all(&chunk[..n])
            .expect("write oversize body chunk");
        remaining -= n;
    }

    // The server replies with a structured ReplyEnd carrying
    // FrameTooLarge. cid is 0 (the sentinel for "wire-level error,
    // no in-flight cid to bind to").
    let reply: WireFrame = read_frame(&mut stream).expect("read fail-soft ReplyEnd");
    match reply {
        WireFrame::ReplyEnd { cid, result } => {
            assert_eq!(cid, 0, "fail-soft uses cid=0 sentinel");
            match result {
                Err(RpcError::FrameTooLarge { size, max: cap }) => {
                    assert_eq!(size, oversize as u64);
                    assert_eq!(cap, max as u64);
                }
                other => panic!("expected FrameTooLarge, got {other:?}"),
            }
        }
        other => panic!("expected ReplyEnd, got {other:?}"),
    }

    // Ping/Pong round-trips — the session is still alive.
    write_frame(&mut stream, &WireFrame::Ping(0xfeed_face)).expect("write Ping after fail-soft");
    let pong: WireFrame = read_frame(&mut stream).expect("read Pong after fail-soft");
    assert_eq!(pong, WireFrame::Pong(0xfeed_face));
}

fn client_peer_kind() -> PeerKind {
    PeerKind::Client {
        client_name: "rpc-client-test".into(),
        client_version: "0.0.1".into(),
    }
}

/// Full socket round-trip: boot `RpcServerCapability` + the echo
/// actor + `TraceDispatchCapability`, connect a real
/// [`RpcClient`](aether_rpc::rpc::RpcClient), fire a `Call` carrying a
/// `TestEchoRequest`, and drain the inbound channel — expect
/// `ReplyEvent { TestEchoReply }` then `ReplyEnd { Ok }`. This is the
/// only test exercising the actual TCP client↔server path end to end
/// (the `RpcClient` half moved to `aether-rpc` per ADR-0102; this
/// integration test stays here, where the server lives).
#[test]
fn call_echo_round_trips_over_the_socket() {
    use crate::rpc::test_echo::{TestEchoActor, TestEchoReply, TestEchoRequest};
    use crate::rpc::{MailEnvelope, MailboxAddress, RpcClient};
    use crate::trace::TraceDispatchCapability;
    use aether_actor::Addressable;
    use aether_data::{Kind, mailbox_id_from_name};

    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        // TraceObserver fires `Settled { root }` once a dispatched
        // chain drains; without it RpcServer's settlement
        // subscription never wakes and no `ReplyEnd` is written.
        .with_actor::<TraceDispatchCapability>(())
        .with_actor::<TestEchoActor>(())
        .with_actor::<RpcServerCapability>(RpcServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            peer_kind: test_peer_kind(),
        })
        .build_passive()
        .expect("caps boot");

    let port = chassis
        .handle::<RpcServerHandle>()
        .expect("RpcServerHandle published")
        .local_port;

    // No on_frame work needed — `recv_timeout` returning is the
    // observable signal we care about. iamacoffeepot/aether#835:
    // a prior version asserted `frames_seen >= 2` against an
    // AtomicUsize bumped inside the hook, but the hook is a
    // post-enqueue scheduling kick by design — the test thread can
    // wake from `recv_timeout` before the reader thread reaches
    // `on_frame()`, racing the assertion. End-to-end correctness
    // here is the two `recv_timeout` returns below: ReplyEvent then
    // ReplyEnd.
    let mut conn = RpcClient::connect(&format!("127.0.0.1:{port}"), client_peer_kind(), || {})
        .expect("client connects + handshakes");

    // The handshake handed back the server's identity.
    match &conn.server {
        PeerKind::Substrate { engine_name, .. } => assert_eq!(engine_name, "test"),
        PeerKind::Client { .. } => panic!("expected Substrate peer kind from server"),
    }

    let echo_payload = TestEchoRequest { value: 42 }.encode_into_bytes();
    let echo_mailbox = mailbox_id_from_name(<TestEchoActor as Addressable>::NAMESPACE);
    let cid = conn
        .client
        .call(MailEnvelope {
            to: MailboxAddress::local(echo_mailbox),
            from: None,
            kind: <TestEchoRequest as Kind>::ID,
            correlation_id: None,
            payload: echo_payload,
        })
        .expect("call writes");

    // First frame back: ReplyEvent carrying the echoed reply.
    // recv_timeout so a hung settlement fails the test instead of
    // blocking forever.
    let event = conn
        .inbound
        .recv_timeout(Duration::from_secs(5))
        .expect("ReplyEvent within 5s");
    let envelope = match event {
        WireFrame::ReplyEvent {
            cid: ev_cid,
            envelope,
        } => {
            assert_eq!(ev_cid, cid);
            envelope
        }
        other => panic!("expected ReplyEvent, got {other:?}"),
    };
    assert_eq!(envelope.kind, <TestEchoReply as Kind>::ID);
    let decoded = TestEchoReply::decode_from_bytes(&envelope.payload).expect("decode reply");
    assert_eq!(decoded.value, 42);

    // Then ReplyEnd closes the call.
    let end = conn
        .inbound
        .recv_timeout(Duration::from_secs(5))
        .expect("ReplyEnd within 5s");
    match end {
        WireFrame::ReplyEnd {
            cid: end_cid,
            result,
        } => {
            assert_eq!(end_cid, cid);
            result.expect("ReplyEnd result Ok");
        }
        other => panic!("expected ReplyEnd, got {other:?}"),
    }
}
