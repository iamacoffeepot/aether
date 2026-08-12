//! The ADR-0149 migration step 1 exit gate, cross-process: boot the `bloomery`
//! bin — the control core is a native capability mounted into the chassis at
//! boot, so it comes up already live — seal a synthetic single-workpiece bloom
//! by admitting a `Fact::Seal` event through the `aether.bloomery.admit`
//! ingress, `kill -9` the process, restart it against the same database (the
//! control core's `wire` replays the journal on boot), and prove convergence
//! **through the reducer**: `aether.bloomery.query` returns the rebuilt bloom,
//! and a second overlapping seal loses cleanly.
//!
//! Unlike `recovery.rs` (which drives the raw `aether.store.*` primitives), this
//! drives the admit/query mail and reply outcomes only — never the store
//! directly — so it exercises the whole control loop: admit → reduce → combined
//! commit → snapshot apply, and boot replay → rebuild.
//!
//! The one exception is `aether.store.record_config`, which the configuration
//! test below sends deliberately: it is standing in for the api cap, which
//! authors a configuration straight to the store without the control core
//! hearing about it (ADR-0174). Reaching the store is the *point* of that test —
//! it is how the core's resolved set is made stale, which is the condition the
//! deferred re-read exists to survive.

#![allow(clippy::unwrap_used)]
// The test addresses the native control cap by its lineage path
// (`mailbox_id_from_path`), disallowed-by-default outside the id/routing API and
// permitted here per the clippy.toml test carve-out.
#![allow(clippy::disallowed_methods)]

mod common;

use std::net::TcpStream;
use std::thread;
use std::time::{Duration, Instant};

use aether_bloomery::{
    Admit, AdmitResult, BloomDraft, CONTROL_CORE_NAMESPACE, ConfigKind, ConfigRegistry, Digest, Event, Evidence,
    EvidenceKind, Fact, IdempotencyKey, Membership, ModelOverride, Outcome, Query, QueryResult, SealError,
    Unproducible, ViewDocument, WorkpieceId,
};
use aether_chassis_bloomery::store::{RecordConfig, RecordConfigResult};
use aether_codec::frame::{read_frame, write_frame};
use aether_data::wire::{from_bytes, to_vec};
use aether_data::{Kind, MailboxId, mailbox_id_from_path};
use aether_rpc::WireFrame;
use common::client::{call, call_frame, connect, handshake};
use common::{Coordinator, free_port};
use serde::Serialize;

/// Fork the `bloomery` bin against `db` on `port`, reaped when the returned
/// guard drops.
fn spawn(port: u16, db: &str) -> Coordinator {
    Coordinator::spawn(port, &[("AETHER_STORE_PATH", db)])
}

/// Pipeline two typed `Call`s to `mailbox` — write **both** frames before reading
/// either reply — so the second request sits in the actor's mailbox while the
/// first's store round-trip is still outstanding, forcing the in-flight
/// same-key admit interleaving. Returns each cid's reply of the expected kind.
fn call_pair<Req, Reply>(
    stream: &mut TcpStream,
    cids: (u64, u64),
    mailbox: MailboxId,
    requests: (&Req, &Req),
) -> (Reply, Reply)
where
    Req: Kind + Serialize,
    Reply: Kind,
{
    for (cid, request) in [(cids.0, requests.0), (cids.1, requests.1)] {
        write_frame(stream, &call_frame(cid, mailbox, request)).unwrap();
    }

    let mut first: Option<Reply> = None;
    let mut second: Option<Reply> = None;
    let mut ended = 0;
    while ended < 2 {
        match read_frame(stream).unwrap() {
            WireFrame::ReplyEvent { cid, envelope } => {
                if envelope.kind == Reply::ID
                    && let Some(reply) = Reply::decode_from_bytes(&envelope.payload)
                {
                    if cid == cids.0 {
                        first = Some(reply);
                    } else if cid == cids.1 {
                        second = Some(reply);
                    } else {
                        panic!("ReplyEvent for an unexpected cid {cid}");
                    }
                }
            }
            WireFrame::ReplyEnd { cid, result } => {
                result.unwrap();
                assert!(cid == cids.0 || cid == cids.1, "ReplyEnd for an unexpected cid {cid}");
                ended += 1;
            }
            other => panic!("unexpected frame during pipelined pair: {other:?}"),
        }
    }
    (
        first.expect("the first cid produced a reply of the expected kind"),
        second.expect("the second cid produced a reply of the expected kind"),
    )
}

/// The native control-core capability's mailbox — mounted into the chassis at
/// boot under `aether.bloomery.control`, addressed by its lineage path.
fn control_mailbox() -> MailboxId {
    mailbox_id_from_path(CONTROL_CORE_NAMESPACE)
}

/// A valid `Fact::Seal` event for a single-workpiece bloom on `base`, its member
/// approved (the approval evidence binds the member's own scope revision, which
/// the reducer's admission requires). Distinct `base` seeds yield distinct bloom
/// ids over the same workpiece.
fn seal_event(key: &str, base: u8, workpiece: &str) -> Event {
    seal_event_configured(key, base, workpiece, ConfigRegistry::default())
}

/// [`seal_event`], with the member sealing `configs` (ADR-0174).
fn seal_event_configured(key: &str, base: u8, workpiece: &str, configs: ConfigRegistry) -> Event {
    let scope_revision = Digest::from_bytes([1; 32]);
    let mut member = Membership {
        workpiece: WorkpieceId(workpiece.to_owned()),
        scope_revision,
        configs,
        approval: Evidence {
            subject: Digest::default(),
            kind: EvidenceKind::Approval,
            detail: Digest::from_bytes([200; 32]),
        },
    };
    // The approval binds the member's subject (ADR-0174).
    member.approval.subject = member.subject();
    // An empty registry selects the compiled stage line. A configured seal uses
    // the catalog content the caller resolved before reducing.
    let spec =
        BloomDraft { proposals: vec![member], base: Digest::from_bytes([base; 32]), ..BloomDraft::default() }.seal();
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

#[test]
fn control_loop_converges_through_the_reducer_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("bloomery.db");
    let db = db.to_str().unwrap();

    // First boot: load the control core and seal a synthetic bloom through the
    // admit ingress — the reducer decides, the combined commit persists.
    let port = free_port();
    let coordinator = spawn(port, db);
    let mut stream = connect(port);
    handshake(&mut stream, "control-loop-test");

    let control = control_mailbox();
    let sealed = admit(&mut stream, 2, control, &seal_event("seal-1", 0, "wp"));
    assert!(matches!(sealed, Outcome::Sealed(_)), "the first seal admits and seals: {sealed:?}");

    // Crash: SIGKILL after the committed admit.
    drop(stream);
    coordinator.kill9();

    // Restart against the same database and reload the control core; its `wire`
    // replays the journal to rebuild the snapshot.
    let port = free_port();
    let _coordinator = spawn(port, db);
    let mut stream = connect(port);
    handshake(&mut stream, "control-loop-test");
    let control = control_mailbox();

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
}

#[test]
fn concurrent_same_key_admits_each_get_a_coherent_ok() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("bloomery.db");
    let db = db.to_str().unwrap();

    let port = free_port();
    let _coordinator = spawn(port, db);
    let mut stream = connect(port);
    handshake(&mut stream, "control-loop-test");
    let control = control_mailbox();

    // Two admits sharing one idempotency key, pipelined before either reply is
    // read, so the second sits in the mailbox while the first's store round-trip
    // is still outstanding — the in-flight interleaving. Tripwire: the pre-fix
    // code displaced the first pending entry on the second admit and answered the
    // first admitter `Err "superseded by a concurrent admit…"` even though its
    // commit landed; the fan-out fix opens one commit per key and hands every
    // same-key admitter the one resolved outcome.
    let admit = Admit { event: to_vec(&seal_event("dup-key", 0, "wp")).unwrap() };
    let (first, second): (AdmitResult, AdmitResult) = call_pair(&mut stream, (2, 3), control, (&admit, &admit));
    for (label, result) in [("first", &first), ("second", &second)] {
        match result {
            AdmitResult::Ok { outcome } => {
                let outcome: Outcome = from_bytes(outcome).expect("outcome decodes");
                assert!(
                    matches!(outcome, Outcome::Sealed(_) | Outcome::Duplicate),
                    "the {label} same-key admit gets a coherent outcome, never a rejection or stray error: {outcome:?}"
                );
            }
            AdmitResult::Err { error } => panic!("the {label} same-key admit got a spurious error: {error}"),
        }
    }

    // Exactly one bloom: one journal row, one applied decision — the deduped
    // second admit opened no second commit and forced no double-apply.
    let document = query_until_blooms(&mut stream, 10, control, 1);
    assert_eq!(document.blooms.len(), 1, "one bloom from the deduped same-key admit pair: {:?}", document.blooms);
}

/// Author a configuration straight to the store, exactly as the api cap's
/// `POST /configs` does — behind the control core's back.
fn author_config<K: ConfigKind>(stream: &mut TcpStream, cid: u64, value: &K) -> Digest {
    let address = value.address();
    let record =
        RecordConfig { digest: address.as_bytes().to_vec(), kind: K::NAME.to_owned(), bytes: to_vec(value).unwrap() };
    let store = mailbox_id_from_path("aether.store");
    match call::<_, RecordConfigResult>(stream, cid, store, &record) {
        // Tripwire: `POST /configs` renders its whole response from this echo
        // rather than from a held correlation entry (#3694), so an echo that
        // does not reproduce the request's address hands a caller a digest that
        // resolves to nothing — the ADR-0174 divergence the write exists to close.
        RecordConfigResult::Ok { digest, kind } => {
            assert_eq!(digest, address.as_bytes(), "the write echoes the address it stored under");
            assert_eq!(kind, K::NAME, "the write echoes the kind it stored under");
            address
        }
        RecordConfigResult::Err { error } => panic!("config write failed: {error}"),
    }
}

// Tripwire: a seal naming a configuration authored *after* the control core
// booted still admits (ADR-0174). The api cap writes an authored config straight
// to the store, so the core's resolved set is stale by construction the first
// time any new configuration is sealed — without the deferred re-read the core
// would refuse a perfectly good seal, and every operator would have to restart
// the coordinator between authoring a config and using it.
//
// The second half is the other side of the same gate: an address nothing ever
// authored is refused, and named. A re-read cannot conjure it, so the refusal has
// to arrive rather than the core re-reading forever.
#[test]
fn a_seal_naming_a_configuration_authored_after_boot_still_admits() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("bloomery.db");
    let db = db.to_str().unwrap();

    let port = free_port();
    let _coordinator = spawn(port, db);
    let mut stream = connect(port);
    handshake(&mut stream, "control-loop-test");
    let control = control_mailbox();

    let override_config = ModelOverride::default();
    let address = author_config(&mut stream, 2, &override_config);
    let mut configs = ConfigRegistry::default();
    configs.insert::<ModelOverride>(address);

    let sealed = admit(&mut stream, 3, control, &seal_event_configured("cfg-seal", 0, "wp-a", configs));
    assert!(
        matches!(sealed, Outcome::Sealed(_)),
        "a seal naming a config authored after boot admits on the re-read: {sealed:?}",
    );

    // An address nothing authored: the re-read runs, finds nothing, and the
    // reducer's own refusal answers — naming the kind that went missing.
    let mut dangling = ConfigRegistry::default();
    dangling.insert::<ModelOverride>(Digest::from_bytes([0xAB; 32]));
    let refused = admit(&mut stream, 4, control, &seal_event_configured("cfg-dangling", 1, "wp-b", dangling));
    assert!(
        matches!(
            refused,
            Outcome::SealRejected(SealError::UnproducibleConfig { ref kind, reason: Unproducible::Absent, .. })
                if kind == ModelOverride::NAME
        ),
        "an address nothing authored is refused, naming its kind: {refused:?}",
    );
}
