//! The ADR-0149 migration step 1 exit gate, cross-process: boot the `bloomery`
//! bin, load the control-core wasm actor, seal a synthetic single-workpiece
//! bloom by admitting a `Fact::Seal` event through the `aether.bloomery.admit`
//! ingress, `kill -9` the process, restart it against the same database, reload
//! the control core (its `wire` replays the journal), and prove convergence
//! **through the reducer**: `aether.bloomery.query` returns the rebuilt bloom,
//! and a second overlapping seal loses cleanly.
//!
//! Unlike `recovery.rs` (which drives the raw `aether.store.*` primitives), this
//! drives the admit/query mail and reply outcomes only — never the store
//! directly — so it exercises the whole control loop: admit → reduce → combined
//! commit → snapshot apply, and boot replay → rebuild.
//!
//! The test needs the control-core wasm pre-built for `wasm32-unknown-unknown`
//! (CI's "Pre-build component wasm" step / `cargo xtask dist`). A dev box that
//! hasn't built it skips cleanly, unless `AETHER_REQUIRE_RUNTIME` is set.

#![allow(clippy::unwrap_used)]
// The wasm-availability skip prints a diagnostic to stderr and reads the
// `AETHER_REQUIRE_RUNTIME` opt-out — legitimate test harness controls, not cap
// config (the `headless_autoload.rs` precedent).
#![allow(clippy::print_stderr)]
#![allow(clippy::disallowed_methods)]

use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use aether_bloomery::{
    Admit, AdmitResult, BloomDraft, Digest, Event, Evidence, EvidenceKind, Fact, IdempotencyKey, Membership, Outcome,
    Query, QueryResult, StageCatalog, ViewDocument, WorkpieceId,
};
use aether_capabilities::rpc::{Hello, HelloAck, MailEnvelope, MailboxAddress, PeerKind, WIRE_VERSION, WireFrame};
use aether_codec::frame::{read_frame, write_frame};
use aether_data::wire::{from_bytes, to_vec};
use aether_data::{Kind, MailboxId, mailbox_id_from_path};
use aether_kinds::{LoadComponent, LoadResult};
use serde::Serialize;

/// Locate the control-core wasm artifact under the workspace target dir,
/// preferring `release` over `debug`. `None` when neither exists (an unbuilt
/// dev box).
fn control_core_wasm() -> Option<PathBuf> {
    // `CARGO_BIN_EXE_bloomery` sits at `<target>/<profile>/bloomery`, so its
    // grandparent is `<target>`.
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_bloomery"));
    let target = bin.parent()?.parent()?;
    for profile in ["release", "debug"] {
        let candidate = target.join("wasm32-unknown-unknown").join(profile).join("aether_bloomery.wasm");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Reserve a free localhost port by binding `:0`, then release it for the bin to
/// claim. A small race window, tolerated by the connect retry loop.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// Fork the `bloomery` bin against `db` on `port`. The bin now also binds a
/// default REST control-API port (#3498), so hand it a free HTTP port too — a
/// fixed default would collide across the suite's concurrently-spawned bins.
fn spawn(port: u16, db: &str) -> Child {
    Command::new(env!("CARGO_BIN_EXE_bloomery"))
        .env("AETHER_RPC_PORT", port.to_string())
        .env("AETHER_HTTP_PORT", free_port().to_string())
        .env("AETHER_STORE_PATH", db)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// Connect to the bin, retrying until it has bound its RPC port.
fn connect(port: u16) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(stream) => {
                stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
                return stream;
            }
            Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(100)),
            Err(error) => panic!("could not reach the bloomery bin on port {port}: {error}"),
        }
    }
}

/// Handshake as a client peer.
fn handshake(stream: &mut TcpStream) {
    let hello = WireFrame::Hello(Hello {
        wire_version: WIRE_VERSION,
        peer: PeerKind::Client { client_name: "control-loop-test".into(), client_version: "0.0.1".into() },
    });
    write_frame(stream, &hello).unwrap();
    match read_frame(stream).unwrap() {
        WireFrame::HelloAck(HelloAck { wire_version, .. }) => assert_eq!(wire_version, WIRE_VERSION),
        other => panic!("expected HelloAck, got {other:?}"),
    }
}

/// Issue one typed `Call` to `mailbox` and decode the reply of the expected kind
/// (collected from the `ReplyEvent` stream, closed by `ReplyEnd`).
fn call<Req, Reply>(stream: &mut TcpStream, cid: u64, mailbox: MailboxId, request: &Req) -> Reply
where
    Req: Kind + Serialize,
    Reply: Kind,
{
    let frame = WireFrame::Call {
        cid: Some(cid),
        envelope: MailEnvelope {
            to: MailboxAddress { engine: None, mailbox },
            from: None,
            kind: Req::ID,
            correlation_id: None,
            payload: request.encode_into_bytes(),
        },
    };
    write_frame(stream, &frame).unwrap();

    let mut reply: Option<Reply> = None;
    loop {
        match read_frame(stream).unwrap() {
            WireFrame::ReplyEvent { cid: got, envelope } => {
                assert_eq!(got, cid, "ReplyEvent cid mismatch");
                if envelope.kind == Reply::ID {
                    reply = Reply::decode_from_bytes(&envelope.payload);
                }
            }
            WireFrame::ReplyEnd { cid: got, result } => {
                assert_eq!(got, cid, "ReplyEnd cid mismatch");
                result.unwrap();
                return reply.expect("a reply of the expected kind arrived before ReplyEnd");
            }
            other => panic!("unexpected frame for call {cid}: {other:?}"),
        }
    }
}

/// Load the control-core component and return its assigned mailbox id.
fn load_control_core(stream: &mut TcpStream, cid: u64, wasm: &[u8]) -> MailboxId {
    let load = LoadComponent { wasm: wasm.to_vec(), name: None, config: Vec::new(), export: None };
    match call::<_, LoadResult>(stream, cid, mailbox_id_from_path("aether.component"), &load) {
        LoadResult::Ok { mailbox_id, .. } => mailbox_id,
        LoadResult::Err { error } => panic!("control-core load failed: {error}"),
    }
}

/// A valid `Fact::Seal` event for a single-workpiece bloom on `base`, its member
/// approved (the approval evidence binds the member's own scope revision, which
/// the reducer's admission requires). Distinct `base` seeds yield distinct bloom
/// ids over the same workpiece.
fn seal_event(key: &str, base: u8, workpiece: &str) -> Event {
    let scope_revision = Digest::from_bytes([1; 32]);
    let member = Membership {
        workpiece: WorkpieceId(workpiece.to_owned()),
        scope_revision,
        approval: Evidence {
            subject: scope_revision,
            kind: EvidenceKind::Approval,
            detail: Digest::from_bytes([200; 32]),
        },
    };
    // The seal must freeze the one line the pipeline runs (#3506): the reducer
    // rejects a bloom whose stage-catalog digest is not `StageCatalog::line_digest`
    // (the zero default is inadmissible).
    let spec = BloomDraft {
        proposals: vec![member],
        base: Digest::from_bytes([base; 32]),
        stage_catalog: StageCatalog::line_digest(),
        ..BloomDraft::default()
    }
    .seal();
    Event { idempotency_key: IdempotencyKey(key.to_owned()), fact: Fact::Seal(spec) }
}

/// Admit an event's wire bytes and decode the reducer outcome from the reply.
fn admit(stream: &mut TcpStream, cid: u64, control: MailboxId, event: &Event) -> Outcome {
    let admit = Admit { event: to_vec(event).unwrap() };
    match call::<_, AdmitResult>(stream, cid, control, &admit) {
        AdmitResult::Ok { outcome } => from_bytes(&outcome).expect("outcome decodes"),
        AdmitResult::Err { error } => panic!("admit failed: {error}"),
    }
}

/// Query the whole projection, retrying until the expected bloom count appears —
/// boot replay runs asynchronously after a component load, so a query issued
/// immediately can race the rebuild.
fn query_until_blooms(stream: &mut TcpStream, cid_base: u64, control: MailboxId, want: usize) -> ViewDocument {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut cid = cid_base;
    loop {
        let document = match call::<_, QueryResult>(stream, cid, control, &Query { bloom: None }) {
            QueryResult::Document { document } => from_bytes::<ViewDocument>(&document).expect("document decodes"),
            other => panic!("expected a document reply, got {other:?}"),
        };
        if document.blooms.len() == want || Instant::now() >= deadline {
            return document;
        }
        cid += 1;
        thread::sleep(Duration::from_millis(100));
    }
}

/// Terminate a child with SIGKILL (`Child::kill` is SIGKILL on unix) and reap it.
fn kill9(mut child: Child) {
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn control_loop_converges_through_the_reducer_across_a_restart() {
    let Some(wasm_path) = control_core_wasm() else {
        assert!(
            env::var_os("AETHER_REQUIRE_RUNTIME").is_none(),
            "AETHER_REQUIRE_RUNTIME set but the control-core wasm is not pre-built"
        );
        eprintln!(
            "skipping: control-core wasm not built; run \
             `cargo build -p aether-bloomery --target wasm32-unknown-unknown`"
        );
        return;
    };
    let wasm = fs::read(&wasm_path).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("bloomery.db");
    let db = db.to_str().unwrap();

    // First boot: load the control core and seal a synthetic bloom through the
    // admit ingress — the reducer decides, the combined commit persists.
    let port = free_port();
    let child = spawn(port, db);
    let mut stream = connect(port);
    handshake(&mut stream);

    let control = load_control_core(&mut stream, 1, &wasm);
    let sealed = admit(&mut stream, 2, control, &seal_event("seal-1", 0, "wp"));
    assert!(matches!(sealed, Outcome::Sealed(_)), "the first seal admits and seals: {sealed:?}");

    // Crash: SIGKILL after the committed admit.
    drop(stream);
    kill9(child);

    // Restart against the same database and reload the control core; its `wire`
    // replays the journal to rebuild the snapshot.
    let port = free_port();
    let child = spawn(port, db);
    let mut stream = connect(port);
    handshake(&mut stream);
    let control = load_control_core(&mut stream, 1, &wasm);

    // Convergence through the reducer: the rebuilt snapshot names the bloom.
    let document = query_until_blooms(&mut stream, 10, control, 1);
    assert_eq!(document.blooms.len(), 1, "journal replay rebuilt exactly the sealed bloom");
    let bloom = &document.blooms[0];
    assert_eq!(bloom.members.len(), 1);
    assert_eq!(bloom.members[0].workpiece.0, "wp");

    // A second overlapping seal (a different bloom over the same workpiece) loses
    // cleanly against the rebuilt membership — proof the snapshot converged, not
    // just the raw store.
    let second = admit(&mut stream, 20, control, &seal_event("seal-2", 5, "wp"));
    assert!(matches!(second, Outcome::SealRejected(_)), "the overlapping seal is refused: {second:?}");

    drop(stream);
    kill9(child);
}
