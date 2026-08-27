//! The REST control API end-to-end (ADR-0149 §Packaging, issue #3498): boot the
//! `bloomery` bin with the HTTP ingress + control core autoloaded, and drive a
//! bloom lifecycle over raw HTTP the way an operator's `curl` would — stage
//! workpieces, shape and seal a draft, read the sealed bloom / view document /
//! journal, and 404 a missing artifact. No typed-mail RPC vocabulary is used;
//! every request is plain HTTP against the `aether.bloomery.api` router. The
//! control core is a native capability mounted into the chassis at boot, so the
//! bin comes up with it already live and the write / live-read half always runs.

#![allow(clippy::unwrap_used)]
#![allow(clippy::disallowed_methods)]
// Test-harness ergonomics: fully-qualified std paths in a one-off client and a
// request head assembled with `format!`.
#![allow(clippy::absolute_paths)]
#![allow(clippy::format_push_string)]
#![allow(clippy::format_collect)]
#![allow(clippy::manual_assert)]

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use aether_bloomery::testing::{digest, event};
use aether_bloomery::{
    AgentProfile, ApprovalPolicy, ApprovalRule, AuthorityDoor, BloomDraft, BloomId, CapabilityLedger, ClaimRefKind,
    ConfigKind, ConfigRegistry, ContentAddressed, Digest, DispatchPayload, Evidence, EvidenceKind, Harness, KeyId,
    Membership, MetricsSeat, NamedPath, ORPHAN_CLAIM_RELEASE_WORDS, Observation, OrphanClaimRelease, PathOrigin,
    Provenance, ReasoningEffort, SCOPE_REVISION_SCHEMA, SCOPE_VERIFY_SCHEMA, ScopeRevision, ScopeRouting,
    ScopeVerifyInput, SignatureEnvelope, StageCatalog, StageId, Statement, Tier, ToolPolicy, Topic, WorkpieceId,
    authorization_message, digest_of, verify_scope,
};
use aether_chassis_bloomery::bloomery::TopicOutbox;
use aether_chassis_bloomery::commission;
use aether_chassis_bloomery::store::{CommissionBackend, SqliteStore, StoreBackend};
use aether_data::wire::from_bytes;
use common::{Coordinator, Ingress};
use ed25519_dalek::{Signer, SigningKey};
use serde_json::Value;

/// Bearer the commission authoring routes require in this process.
const CONTROL_TOKEN: &str = "test-control-token";

/// How long a forked coordinator has to bind an ingress and say so. A child
/// that dies instead reports at once, so this is the silent-child ceiling
/// rather than a cost any passing run pays.
const BIND_BUDGET: Duration = Duration::from_mins(1);

/// An empty seal / supersede body. Projections and descriptions are store-backed.
fn seal_body() -> Value {
    Value::Object(serde_json::Map::new())
}

/// A member built the way the draft fixtures build one, so its
/// [`subject`](Membership::subject) — what an above-auto author signs and what
/// the seal validates against (ADR-0174) — is computed in exactly one place.
fn member(workpiece: &str, revision: Digest, detail: Digest) -> Membership {
    let mut member = Membership {
        workpiece: WorkpieceId(workpiece.to_owned()),
        scope_revision: revision,
        configs: ConfigRegistry::default(),
        approval: Evidence { subject: Digest::default(), kind: EvidenceKind::Approval, detail },
    };
    member.approval.subject = member.subject();
    member
}

/// An author-signed statement by the allowlisted `owner` over `words`, signed as
/// authority for `door` bound to `binding`, naming `parents` (ADR-0182).
fn owner_signed_at(door: AuthorityDoor, binding: Digest, words: Vec<u8>, parents: Vec<Digest>) -> Statement {
    let message = authorization_message(door, binding, &words);
    Statement {
        words,
        provenance: Provenance::AuthorSignature(SignatureEnvelope {
            signer: KeyId("owner".to_owned()),
            signature: owner_signing_key().sign(message.as_bytes()).to_bytes().to_vec(),
        }),
        parents,
    }
}

/// Persist an open commission with a complete revision and a signed approval.
fn seed_commission(port: u16, id: &str, surface: &[&str]) -> Digest {
    seed_commission_with(port, id, surface, "problem", true)
}

/// Persist a commission; `approve` submits the owner-signed approval.
fn seed_commission_with(port: u16, id: &str, surface: &[&str], problem: &str, approve: bool) -> Digest {
    seed_commission_described(port, id, surface, problem, &format!("task for {id}"), approve)
}

/// Persist a commission whose revision carries an explicit description.
fn seed_commission_described(
    port: u16,
    id: &str,
    surface: &[&str],
    problem: &str,
    description: &str,
    approve: bool,
) -> Digest {
    seed_commission_revision(port, id, surface, problem, description, approve, &[])
}

/// Persist a complete approved commission whose frozen revision depends on `depends_on`.
fn seed_depending(port: u16, id: &str, depends_on: &[&str]) -> Digest {
    seed_commission_revision(port, id, &["docs/guide/**"], "problem", &format!("task for {id}"), true, depends_on)
}

fn seed_commission_revision(
    port: u16,
    id: &str,
    surface: &[&str],
    problem: &str,
    description: &str,
    approve: bool,
    dependencies: &[&str],
) -> Digest {
    let intent = Statement {
        words: format!("intent {id}").into_bytes(),
        provenance: Provenance::ObservationAttestation(Observation { source: "rest-api".to_owned() }),
        parents: Vec::new(),
    };
    let (status, created) = send_auth(port, "POST", "/commissions", &serde_json::json!({ "id": id, "intent": intent }));
    assert_eq!(status, 201, "create commission {id}: {created:?}");

    let revision = ScopeRevision {
        schema: SCOPE_REVISION_SCHEMA,
        workpiece: WorkpieceId(id.to_owned()),
        predecessor: None,
        problem: problem.to_owned(),
        design: "design".to_owned(),
        plan: "plan".to_owned(),
        declared_surface: surface.iter().map(|glob| (*glob).to_owned()).collect(),
        dogfood_brief: "dogfood".to_owned(),
        routing: ScopeRouting { size: "M".to_owned(), model: "construct: test".to_owned() },
        dependencies: dependencies.iter().map(|dep| WorkpieceId((*dep).to_owned())).collect(),
        description: description.to_owned(),
        implements: Vec::new(),
        declared_crates: Vec::new(),
        declared_reads: Vec::new(),
    };
    let (status, written) =
        send_auth(port, "POST", &format!("/commissions/{id}/revisions"), &serde_json::json!({ "revision": revision }));
    assert_eq!(status, 201, "write revision {id}: {written:?}");
    let digest = digest_at(&written["digest"]);

    if approve {
        let statement = owner_signed_at(AuthorityDoor::Approve, digest, digest.as_bytes().to_vec(), Vec::new());
        let (status, approved) = send_auth(
            port,
            "POST",
            &format!("/commissions/{id}/approvals"),
            &serde_json::to_value(&statement).unwrap(),
        );
        assert_eq!(status, 201, "approve {id}: {approved:?}");
    }
    digest
}

/// Authenticated JSON write used by the commission authoring routes.
fn send_auth(port: u16, method: &str, path: &str, body: &Value) -> (u16, Value) {
    let bytes = serde_json::to_vec(body).unwrap();
    let (status, response) = try_http_auth(port, method, path, Some(&bytes), Some(CONTROL_TOKEN)).unwrap();
    (status, serde_json::from_slice(&response).unwrap_or(Value::Null))
}

/// Authenticated JSON GET used by the commission show route.
fn get_auth(port: u16, path: &str) -> (u16, Value) {
    let (status, response) = try_http_auth(port, "GET", path, None, Some(CONTROL_TOKEN)).unwrap();
    (status, serde_json::from_slice(&response).unwrap_or(Value::Null))
}

/// Write a self-contained tier policy to a temp dir and return it plus the
/// policy path. `docs/guide/**` resolves `auto`; `crates/aether-data/**` resolves
/// `human` (above-auto); the default is `judge`. Kept independent of the evolving
/// repo policy so the gate cases are deterministic.
fn test_policy() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("approval-policy.toml");
    std::fs::write(
        &path,
        "default = \"judge\"\n[[rules]]\nglob = \"docs/guide/**\"\ntier = \"auto\"\n[[rules]]\nglob = \"crates/aether-data/**\"\ntier = \"human\"\n",
    )
    .unwrap();
    let path = path.to_str().unwrap().to_owned();
    (dir, path)
}

/// The deterministic authorized signer the test configures the `aether.signing`
/// allowlist with and signs the answer statement with — a fixed seed, so the
/// public key in the allowlist matches the private key that signs.
fn owner_signing_key() -> SigningKey {
    SigningKey::from_bytes(&[42u8; 32])
}

/// The `AETHER_SIGNING_ALLOWLIST` value trusting `owner` at [`owner_signing_key`]'s
/// public half, at the `human` ceiling — this key stands in for the owner, so
/// it signs at every tier (`key-id:hex-public-key:tier`, #5324).
fn owner_allowlist() -> String {
    format!("owner:{}:human", aether_bloomery::encode_hex(&owner_signing_key().verifying_key().to_bytes()))
}

/// Fork the `bloomery` bin with the HTTP ingress and control core autoloaded,
/// pointing the pre-seal approve gate at `policy_path` (#3583). Reaped when the
/// returned guard drops.
///
/// Both ingresses bind `0`, so the child holds each port from the moment it
/// binds and no concurrently booting sibling can take one. A REST port reserved
/// and released here used to be stolen exactly that way, and the loser died on
/// the HTTP bind before the RPC ingress existed — leaving the fixture to burn
/// its readiness deadline against a socket nobody owned (#5000, #5475).
fn spawn(policy_path: &str) -> Coordinator {
    spawn_with_store(policy_path, ":memory:")
}

/// [`spawn`], with a caller-provided durable store for a test that reads the
/// dispatch outbox through a second store connection.
fn spawn_with_store(policy_path: &str, store_path: &str) -> Coordinator {
    Coordinator::spawn(
        0,
        &[
            ("AETHER_STORE_PATH", store_path),
            ("AETHER_SIGNING_ALLOWLIST", &owner_allowlist()),
            ("AETHER_APPROVAL_POLICY_FILE", policy_path),
            ("AETHER_HTTP_CONTROL_TOKEN", CONTROL_TOKEN),
        ],
    )
}

/// The port `coordinator` announced for `ingress` as it bound.
///
/// # Panics
/// The child died before it bound that ingress, or stayed silent past the
/// budget — either way the refusal carries what it last logged.
fn announced_port(coordinator: &Coordinator, ingress: Ingress) -> u16 {
    coordinator
        .await_port(ingress, Instant::now() + BIND_BUDGET)
        .unwrap_or_else(|why| panic!("the coordinator never announced a bound ingress: {why}"))
}

/// Record a green whole-workspace receipt for `base` so a following HTTP seal
/// dispatches Construct rather than waiting on `verify.base`.
///
/// `rpc_port` is the one the child announced, so the handshake here reaches the
/// coordinator this test forked and no other.
fn prove_green_base(rpc_port: u16, base: Digest) {
    use aether_bloomery::{
        Admit, AdmitResult, CONTROL_CORE_NAMESPACE, Event, Fact, IdempotencyKey, Outcome, VerifyFailureSet,
    };
    use aether_data::mailbox_id_from_path;
    use aether_data::wire::to_vec;
    use common::client::{call, connect_and_handshake};

    let mut stream = connect_and_handshake(rpc_port, "prove-base");
    let control = mailbox_id_from_path(CONTROL_CORE_NAMESPACE);
    let event = Event {
        idempotency_key: IdempotencyKey("fixture-base-verify".to_owned()),
        fact: Fact::BaseVerifyCompleted {
            base,
            tree: base,
            passed: true,
            evidence: Evidence {
                subject: base,
                kind: EvidenceKind::VerificationResult,
                detail: Digest::from_bytes([9; 32]),
            },
            failed: VerifyFailureSet::EMPTY,
        },
    };
    let admit = Admit { event: to_vec(&event).unwrap() };
    match call::<_, AdmitResult>(&mut stream, 1, control, &admit) {
        AdmitResult::Ok { outcome } => {
            let outcome: Outcome = from_bytes(&outcome).expect("outcome decodes");
            assert!(matches!(outcome, Outcome::BaseProven { .. }), "the fixture base proves green: {outcome:?}");
        }
        AdmitResult::Err { error } => panic!("fixture base prove failed: {error}"),
    }
}

/// One HTTP request over a fresh `Connection: close` socket; returns the status
/// code and the response body bytes. `Err` when the socket cannot be reached
/// yet (the bin is still binding).
fn try_http(port: u16, method: &str, path: &str, body: Option<&[u8]>) -> Result<(u16, Vec<u8>), std::io::Error> {
    try_http_auth(port, method, path, body, None)
}

fn try_http_auth(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    token: Option<&str>,
) -> Result<(u16, Vec<u8>), std::io::Error> {
    let (status, _, body) = request(port, method, path, body, token)?;
    Ok((status, body))
}

type HttpExchange = (u16, Vec<(String, String)>, Vec<u8>);

fn request(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    token: Option<&str>,
) -> Result<HttpExchange, std::io::Error> {
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    if let Some(token) = token {
        head.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    if let Some(bytes) = body {
        head.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", bytes.len()));
    }
    head.push_str("\r\n");
    let mut request = head.into_bytes();
    if let Some(bytes) = body {
        request.extend_from_slice(bytes);
    }

    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    stream.write_all(&request)?;
    stream.flush()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    Ok(parse_response(&response))
}

/// Split an HTTP response into its status code, headers, and body bytes.
fn parse_response(response: &[u8]) -> HttpExchange {
    let separator = b"\r\n\r\n";
    let head_end = response.windows(separator.len()).position(|w| w == separator).expect("response has a head");
    let head = &response[..head_end];
    let body = response[head_end + separator.len()..].to_vec();
    let mut lines = head.split(|&b| b == b'\r');
    let status_line = lines.next().unwrap();
    let status = std::str::from_utf8(status_line)
        .unwrap()
        .split_whitespace()
        .nth(1)
        .expect("status line has a code")
        .parse()
        .expect("status code parses");
    let mut headers = Vec::new();
    for line in lines {
        let line = line.strip_prefix(b"\n").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let Ok(line) = std::str::from_utf8(line) else {
            continue;
        };
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    (status, headers, body)
}

/// Poll `path` until it answers `200` (routes register asynchronously in `wire`,
/// and the control core loads + replays asynchronously at boot).
fn wait_for_200(port: u16, path: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok((200, _)) = try_http(port, "GET", path, None) {
            return;
        }
        if Instant::now() >= deadline {
            panic!("{path} never reached 200 within the deadline");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// A GET that must succeed, returning the parsed JSON body.
fn get_json(port: u16, path: &str) -> (u16, Value) {
    let (status, body) = try_http(port, "GET", path, None).unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// A POST/PATCH with a JSON body, returning the status and parsed JSON body.
fn send_json(port: u16, method: &str, path: &str, body: &Value) -> (u16, Value) {
    let bytes = serde_json::to_vec(body).unwrap();
    let (status, response) = try_http(port, method, path, Some(&bytes)).unwrap();
    (status, serde_json::from_slice(&response).unwrap_or(Value::Null))
}

/// The `BloomId` an outcome names, for the `/blooms/{id}` path. Body and path
/// speak the same 64 hex characters, so the id is taken as rendered.
fn bloom_hex(bloom_id: &Value) -> String {
    bloom_id.as_str().expect("a rendered bloom id is a hex string").to_owned()
}

/// Read a rendered digest back into a [`Digest`] — the inverse of [`hex_of`].
fn digest_at(rendered: &Value) -> Digest {
    let hex = rendered.as_str().expect("a rendered digest is a hex string");
    Digest::from_hex(hex).expect("a rendered digest is 32 lowercase hex characters")
}

/// Hex-encode a [`Digest`] for a path segment that names one — the answer
/// route's `{question}` and the release route's `{digest}`.
fn hex_of(digest: &Digest) -> String {
    digest.to_hex()
}

/// A valid single-workpiece draft the reducer admits: the member's approval
/// evidence binds its own scope revision, and the stage catalog is the one line
/// the reducer requires (`StageCatalog::line_digest`, not the zero default).
fn valid_draft(workpiece: &str, scope_revision: Digest) -> BloomDraft {
    BloomDraft {
        proposals: vec![member(workpiece, scope_revision, Digest::from_bytes([9; 32]))],
        base: Digest::from_bytes([1; 32]),
        ..BloomDraft::default()
    }
}

/// A valid two-workpiece draft. The seal gate re-forms both approvals from the
/// store; the placeholder details only have to be reducer-admissible before
/// the gate replaces them.
fn two_member_draft(wp1: Digest, wp2: Digest) -> BloomDraft {
    let detail = Digest::from_bytes([9; 32]);
    BloomDraft {
        proposals: vec![member("wp-1", wp1, detail), member("wp-2", wp2, detail)],
        base: Digest::from_bytes([1; 32]),
        ..BloomDraft::default()
    }
}

/// Assert the store-backed door refuses a seal that names a workpiece with
/// no commission, an incomplete revision, and a current revision with no
/// approval. Each message is a different closed door.
fn assert_store_door_fails_closed(http_port: u16) {
    let missing =
        patch_draft(http_port, &serde_json::to_value(valid_draft("wp-missing", Digest::from_bytes([1; 32]))).unwrap());
    let (status, body) = send_json(http_port, "POST", &format!("/drafts/{missing}/seal"), &seal_body());
    assert_eq!(status, 422, "missing commission fails closed: {body:?}");
    assert!(
        body["error"].as_str().unwrap_or("").contains("no commission in the store"),
        "missing commission: {body:?}"
    );

    let incomplete = seed_commission_with(http_port, "wp-incomplete", &["docs/guide/**"], "", true);
    let incomplete_id =
        patch_draft(http_port, &serde_json::to_value(valid_draft("wp-incomplete", incomplete)).unwrap());
    let (status, body) = send_json(http_port, "POST", &format!("/drafts/{incomplete_id}/seal"), &seal_body());
    assert_eq!(status, 422, "incomplete revision fails closed: {body:?}");
    assert!(
        body["error"].as_str().unwrap_or("").contains("incomplete"),
        "incomplete must name incompleteness: {body:?}"
    );

    let unsigned = seed_commission_with(http_port, "wp-unsigned", &["docs/guide/**"], "problem", false);
    let unsigned_id = patch_draft(http_port, &serde_json::to_value(valid_draft("wp-unsigned", unsigned)).unwrap());
    let (status, body) = send_json(http_port, "POST", &format!("/drafts/{unsigned_id}/seal"), &seal_body());
    assert_eq!(status, 422, "absent approval fails closed: {body:?}");
    assert!(body["error"].as_str().unwrap_or("").contains("no stored approval"), "absent approval: {body:?}");
}

#[test]
fn rest_api_drives_a_bloom_end_to_end() {
    // The gate loads this policy at init; kept alive for the coordinator's lifetime.
    let (_policy_dir, policy_path) = test_policy();
    let coordinator = spawn(&policy_path);
    let http_port = announced_port(&coordinator, Ingress::Http);

    // Routes register asynchronously; `/drafts` (an in-memory route) is the
    // http-readiness signal.
    wait_for_200(http_port, "/drafts");

    wait_for_200(http_port, "/view");
    assert_store_door_fails_closed(http_port);

    // Durable open commissions are the workpiece list, not the in-memory staged map.
    let revision = seed_commission(http_port, "wp-1", &["docs/guide/**"]);
    let (_, listed) = get_json(http_port, "/workpieces");
    let listed = listed["workpieces"].as_array().expect("workpieces list");
    let wp1 = listed.iter().find(|row| row["id"] == "wp-1").expect("wp-1 is listed");
    assert_eq!(wp1["scope_revision"], hex_of(&revision));

    // Open a draft, shape it into an admissible bloom, and read it back.
    let (status, opened) = send_json(http_port, "POST", "/drafts", &Value::Null);
    assert_eq!(status, 201, "open draft");
    let draft_id = opened["draft_id"].as_str().unwrap().to_owned();
    // The body spells `base` the way the paths do — 64 hex characters — while
    // the memberships beside it keep the canonical byte arrays, so one request
    // exercises both accepted forms. The echo renders every digest as hex.
    let mut patch = serde_json::to_value(valid_draft("wp-1", revision)).unwrap();
    let base = hex_of(&Digest::from_bytes([1; 32]));
    patch["base"] = Value::String(base.clone());
    let (status, patched) = send_json(http_port, "PATCH", &format!("/drafts/{draft_id}"), &patch);
    assert_eq!(status, 200, "patch draft");
    assert_eq!(patched["draft"]["base"], base, "a hex base is taken and rendered back as hex");
    assert_eq!(
        patched["draft"]["proposals"][0]["scope_revision"],
        hex_of(&revision),
        "a digest sent as a byte array renders back as hex too"
    );
    assert_eq!(patched["draft"]["proposals"].as_array().unwrap().len(), 1);
    let (status, _) = get_json(http_port, &format!("/drafts/{draft_id}"));
    assert_eq!(status, 200, "read draft");

    // Malformed hex is a refusal that names its field, never a truncated read.
    let mut malformed = patch.clone();
    malformed["base"] = Value::String("nothexatall".to_owned());
    let (status, refused) = send_json(http_port, "PATCH", &format!("/drafts/{draft_id}"), &malformed);
    assert_eq!(status, 400, "a malformed digest is refused: {refused:?}");
    assert!(refused["error"].as_str().unwrap().contains("base"), "the refusal names the field: {refused:?}");

    // Auto surface + stored complete revision: the gate forms the approval and
    // the seal admits. A caller-supplied projection is not on the body.
    let seal_path = format!("/drafts/{draft_id}/seal");
    let (status, body) =
        try_http(http_port, "POST", &seal_path, Some(&serde_json::to_vec(&seal_body()).unwrap())).unwrap();
    let sealed: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, 200, "auto-tier seal admits: {sealed:?}");
    let bloom_id = bloom_hex(&sealed["outcome"]["Sealed"]);
    assert_eq!(bloom_id.len(), 64, "sealed bloom id is a 32-byte digest");

    // The sealed bloom is live in the view document and its own view.
    let (status, view) = get_json(http_port, "/view");
    assert_eq!(status, 200, "view document");
    assert_eq!(view["blooms"].as_array().unwrap().len(), 1, "one sealed bloom");
    let (status, _) = get_json(http_port, &format!("/blooms/{bloom_id}"));
    assert_eq!(status, 200, "single bloom view");

    // The seal was journaled.
    let (status, journal) = get_json(http_port, "/journal");
    assert_eq!(status, 200, "journal");
    assert_eq!(journal["records"].as_array().unwrap().len(), 1, "one journaled event");

    // A missing artifact is a clean 404, not a hang.
    let (status, _) = try_http(http_port, "GET", &format!("/artifacts/{}", "0".repeat(64)), None).unwrap();
    assert_eq!(status, 404, "missing artifact");

    assert_answer_door_binds_its_question(http_port, &bloom_id);
}

#[test]
fn a_seal_reads_commission_task_text_without_a_github_issue() {
    // Acceptance: the door seals a commission that never had a GitHub home,
    // and construct's task is the signed work order — not a gh issue view,
    // and not an empty advisory field.
    let (_policy_dir, policy_path) = test_policy();
    let store_dir = tempfile::tempdir().unwrap();
    let store_path = store_dir.path().join("bloomery.db");
    let store_path = store_path.to_str().unwrap();
    let coordinator = spawn_with_store(&policy_path, store_path);
    let http_port = announced_port(&coordinator, Ingress::Http);

    wait_for_200(http_port, "/drafts");
    wait_for_200(http_port, "/view");

    // The scope verb persists this rendered order; an empty description is refused.
    let order = "\
## Problem statement

Need a CLI.

## Design notes

design

## Implementation plan

plan
";
    let revision = seed_commission_described(http_port, "wp-local", &["docs/guide/**"], "Need a CLI.", order, true);
    let draft_id = patch_draft(http_port, &serde_json::to_value(valid_draft("wp-local", revision)).unwrap());
    let (status, sealed) = send_json(http_port, "POST", &format!("/drafts/{draft_id}/seal"), &seal_body());
    assert_eq!(status, 200, "a commission with no GitHub issue seals: {sealed:?}");
    let bloom = digest_at(&sealed["outcome"]["Sealed"]);

    let task = wait_for_dispatch_description(store_path, bloom.as_bytes(), "wp-local");
    assert!(task.contains("Need a CLI."), "seal persisted the commission problem: {task}");
    assert!(task.contains("## Design notes"), "seal rendered the signed headings: {task}");
    assert!(task.contains("plan"), "seal rendered the signed plan: {task}");
}

/// Persist is fire-and-forget beside the seal admit, so a second connection
/// has to wait for the row rather than assume the HTTP 200 implies it.
fn wait_for_dispatch_description(store_path: &str, bloom: &[u8], workpiece: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut store = SqliteStore::open(store_path).unwrap();
        if let Ok(Some(text)) = store.lookup_dispatch_description(bloom, workpiece)
            && !text.trim().is_empty()
        {
            return text;
        }
        if Instant::now() >= deadline {
            panic!("dispatch description for {workpiece} never persisted");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Pre-fix, `dependencies_all_closed` was the literal `true`, so this seal was
/// refused later by `resolve_seal_graph` as an unknown workpiece, never as
/// `OpenDependency`.
#[test]
fn an_open_non_member_dependency_is_refused_as_open_dependency() {
    run_with_bloomery("an_open_non_member_dependency_is_refused_as_open_dependency", |http_port| {
        seed_commission(http_port, "wp-dep", &["docs/guide/**"]);
        let revision = seed_depending(http_port, "wp-a", &["wp-dep"]);
        let draft_id = patch_draft(http_port, &serde_json::to_value(valid_draft("wp-a", revision)).unwrap());
        let (status, body) = send_json(http_port, "POST", &format!("/drafts/{draft_id}/seal"), &seal_body());
        assert_eq!(status, 422, "open non-member dependency fails closed: {body:?}");
        let error = body["error"].as_str().unwrap_or("");
        assert!(error.contains("OpenDependency"), "gate names OpenDependency: {body:?}");
        assert!(
            !error.contains("which is not a member of this bloom"),
            "must not be the graph-resolver unknown-workpiece door: {body:?}"
        );
    });
}

/// A landed prerequisite is not a bloom member. Pre-fix, the graph resolver
/// refused it as an unknown workpiece; the gate must admit it.
#[test]
fn a_member_depending_on_a_landed_commission_seals() {
    let (_policy_dir, policy_path) = test_policy();
    let store_dir = tempfile::tempdir().unwrap();
    let store_path = store_dir.path().join("bloomery.db");
    let store_path = store_path.to_str().unwrap();
    let coordinator = spawn_with_store(&policy_path, store_path);
    let http_port = announced_port(&coordinator, Ingress::Http);

    wait_for_200(http_port, "/drafts");
    wait_for_200(http_port, "/view");

    seed_commission(http_port, "wp-dep", &["docs/guide/**"]);
    let mut store = SqliteStore::open(store_path).unwrap();
    store.mark_landed(&WorkpieceId("wp-dep".to_owned())).expect("land the prerequisite");
    drop(store);

    let revision = seed_depending(http_port, "wp-a", &["wp-dep"]);
    let draft_id = patch_draft(http_port, &serde_json::to_value(valid_draft("wp-a", revision)).unwrap());
    let (status, sealed) = send_json(http_port, "POST", &format!("/drafts/{draft_id}/seal"), &seal_body());
    assert_eq!(status, 200, "a landed prerequisite must not refuse the seal: {sealed:?}");
}

/// Two co-sealed members, one depending on the other, still seal and still
/// journal the ordering edge. The naive "not open" reading of completeness
/// would refuse the dependent member, because a co-sealed sibling is Open.
#[test]
fn co_sealed_members_still_journal_the_declared_ordering_edge() {
    run_with_bloomery("co_sealed_members_still_journal_the_declared_ordering_edge", |http_port| {
        let wp_a = seed_commission(http_port, "wp-a", &["docs/guide/**"]);
        let wp_b = seed_depending(http_port, "wp-b", &["wp-a"]);
        let detail = Digest::from_bytes([9; 32]);
        let draft = BloomDraft {
            proposals: vec![member("wp-a", wp_a, detail), member("wp-b", wp_b, detail)],
            base: Digest::from_bytes([1; 32]),
            ..BloomDraft::default()
        };
        let draft_id = patch_draft(http_port, &serde_json::to_value(&draft).unwrap());
        let (status, sealed) = send_json(http_port, "POST", &format!("/drafts/{draft_id}/seal"), &seal_body());
        assert_eq!(status, 200, "co-sealed dependency must still seal: {sealed:?}");

        let (status, journal) = get_json(http_port, "/journal");
        assert_eq!(status, 200, "journal");
        let graph = journal["records"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|record| record["event"]["fact"]["GraphSeal"].as_object())
            .expect("a declared in-bloom edge journals GraphSeal");
        let edges = graph["edges"].as_array().expect("GraphSeal carries edges");
        assert!(
            edges.iter().any(|edge| edge["member"] == "wp-b" && edge["depends_on"] == "wp-a"),
            "the co-sealed ordering edge is journaled: {graph:?}"
        );
    });
}

/// The above-auto deferred-verify seal path (#3599): an above-auto member whose
/// projection carries a valid owner-signed statement admits with a gate-formed
/// approval verified through the `aether.signing` capability; a missing,
/// wrong-subject, or unverifiable statement refuses the whole seal (422, fail
/// closed).
#[test]
fn above_auto_deferred_verify_gates_the_seal() {
    run_with_bloomery("above_auto_deferred_verify_gates_the_seal", assert_single_above_auto_seal);
}

/// The mixed half of the deferred-verify seal: a draft carrying one auto and one
/// above-auto member admits only when the above-auto member's signature
/// verifies. A separate process (own `:memory:` store, own mainline) from the
/// single-member cases, since V1 permits only one active bloom per mainline and
/// each case here seals its own.
#[test]
fn mixed_auto_and_above_auto_seal_admits_when_verified() {
    run_with_bloomery("mixed_auto_and_above_auto_seal_admits_when_verified", assert_mixed_above_auto_seal);
}

/// Boot the `bloomery` bin, wait for both readiness signals (`/drafts` for the
/// HTTP router, `/view` for the live control core), then run `body` against its
/// HTTP port. The coordinator is reaped when this returns, panic or not.
fn run_with_bloomery(_label: &str, body: impl FnOnce(u16)) {
    let (_policy_dir, policy_path) = test_policy();
    let coordinator = spawn(&policy_path);
    let http_port = announced_port(&coordinator, Ingress::Http);

    wait_for_200(http_port, "/drafts");
    wait_for_200(http_port, "/view");
    body(http_port);
}

/// Cases (a/b/c) of the deferred-verify seal on a single above-auto member
/// (`wp-1` over `crates/aether-data`, which the test policy resolves `human`): a
/// missing statement, a wrong-subject statement (synchronous pre-check), and an
/// unverifiable signature (after the `aether.signing` round trip) each fail
/// closed (422); a valid owner-signed statement admits with a gate-formed
/// approval the reducer accepts.
fn assert_single_above_auto_seal(http_port: u16) {
    let unsigned = seed_commission_with(http_port, "wp-1", &["crates/aether-data/**"], "problem", false);
    let unsigned_id = patch_draft(http_port, &serde_json::to_value(valid_draft("wp-1", unsigned)).unwrap());
    let (status, body) = send_json(http_port, "POST", &format!("/drafts/{unsigned_id}/seal"), &seal_body());
    assert_eq!(status, 422, "above-auto with no stored approval fails closed: {body:?}");
    assert!(body["error"].as_str().unwrap_or("").contains("no stored approval"), "{body:?}");

    let revision = seed_commission(http_port, "wp-signed", &["crates/aether-data/**"]);
    let draft_id = patch_draft(http_port, &serde_json::to_value(valid_draft("wp-signed", revision)).unwrap());
    let (status, sealed) = send_json(http_port, "POST", &format!("/drafts/{draft_id}/seal"), &seal_body());
    assert_eq!(status, 200, "above-auto seal admits from the stored statement: {sealed:?}");
    let bloom_id = bloom_hex(&sealed["outcome"]["Sealed"]);
    assert_eq!(bloom_id.len(), 64, "sealed bloom id is a 32-byte digest");

    let (status, view) = get_json(http_port, &format!("/blooms/{bloom_id}"));
    assert_eq!(status, 200, "the sealed above-auto bloom is live");
    let members = view["members"].as_array().unwrap();
    assert_eq!(members.len(), 1, "one sealed member");
    assert_eq!(members[0]["workpiece"], "wp-signed");
    assert_eq!(members[0]["approval"]["kind"], "Approval", "the above-auto member carries a gate-formed approval");
}

/// Case (d) of the deferred-verify seal: a mixed draft with an auto member
/// (`wp-1`, `docs/guide`) and an above-auto member (`wp-2`, `crates/aether-data`)
/// fails closed when the above-auto member is unsigned, and admits both members
/// once its signature verifies.
fn assert_mixed_above_auto_seal(http_port: u16) {
    let wp1 = seed_commission(http_port, "wp-1", &["docs/guide/**"]);
    let wp2_unsigned = seed_commission_with(http_port, "wp-2", &["crates/aether-data/**"], "problem", false);
    let mixed_id = patch_draft(http_port, &serde_json::to_value(two_member_draft(wp1, wp2_unsigned)).unwrap());
    let (status, body) = send_json(http_port, "POST", &format!("/drafts/{mixed_id}/seal"), &seal_body());
    assert_eq!(status, 422, "a mixed draft with an unsigned above-auto member fails closed: {body:?}");

    let wp2 = seed_commission(http_port, "wp-2b", &["crates/aether-data/**"]);
    let mut mixed = two_member_draft(wp1, wp2);
    mixed.proposals[1].workpiece = WorkpieceId("wp-2b".to_owned());
    let mixed_ok = patch_draft(http_port, &serde_json::to_value(&mixed).unwrap());
    let (status, sealed) = send_json(http_port, "POST", &format!("/drafts/{mixed_ok}/seal"), &seal_body());
    assert_eq!(status, 200, "mixed auto + above-auto seal admits when the above-auto member is stored: {sealed:?}");
    let mixed_bloom = bloom_hex(&sealed["outcome"]["Sealed"]);
    let (status, view) = get_json(http_port, &format!("/blooms/{mixed_bloom}"));
    assert_eq!(status, 200, "the mixed bloom is live");
    assert_eq!(view["members"].as_array().unwrap().len(), 2, "both members sealed");
}

// ADR-0174 — `POST /configs` is one authoring route for every configuration
// kind, replacing a route per kind. The contract it has to hold is the same one
// #4588 established for the scope revision, generalized: the address it hands
// back is a pure function of (kind, content), and a `200` means that address
// resolves to a durable row rather than to nothing.
//
// The kind used here is incidental — the route resolves whatever the descriptor
// inventory carries, so a store mail kind exercises the same path a real
// configuration will. What the test pins is the route's own logic: schema
// resolution, encoding through that schema, addressing, and the deferral.
#[test]
fn authoring_a_config_stores_it_under_a_stable_content_address() {
    let (_policy_dir, policy_path) = test_policy();
    let coordinator = spawn(&policy_path);
    let http_port = announced_port(&coordinator, Ingress::Http);

    wait_for_200(http_port, "/drafts");

    let request = |value: Value| serde_json::json!({ "kind": "aether.store.drain_outbox", "value": value });

    let (status, authored) =
        send_json(http_port, "POST", "/configs", &request(serde_json::json!({ "topic": "construct" })));
    assert_eq!(status, 200, "authoring answers only once the store write lands");
    assert!(authored["digest"].as_str().is_some_and(|hex| hex.len() == 64), "the reply names a 32-byte address");
    assert_eq!(authored["kind"], "aether.store.drain_outbox", "the reply names the key a registry seals under");
    let digest = authored["digest"].clone();

    // Idempotent by content addressing: re-authoring rewrites the same row
    // rather than creating a second address a seal would have to choose
    // between.
    let (status, again) =
        send_json(http_port, "POST", "/configs", &request(serde_json::json!({ "topic": "construct" })));
    assert_eq!(status, 200);
    assert_eq!(again["digest"], digest, "the same content under the same kind keeps its address");

    let (status, other) = send_json(http_port, "POST", "/configs", &request(serde_json::json!({ "topic": "refine" })));
    assert_eq!(status, 200);
    assert_ne!(other["digest"], digest, "changed content changes the address");

    // A kind this binary does not carry is a client error, not a server one:
    // the vocabulary is fixed at build time, so no retry helps.
    let (status, _) = send_json(
        http_port,
        "POST",
        "/configs",
        &serde_json::json!({ "kind": "aether.bloomery.no_such_config", "value": {} }),
    );
    assert_eq!(status, 400, "an unknown kind is refused before any write");

    // A value that does not fit the kind's schema is refused inline too, so a
    // typo cannot reach the store as bytes that will not decode at dispatch.
    let (status, _) = send_json(http_port, "POST", "/configs", &request(serde_json::json!({ "topic": 7 })));
    assert_eq!(status, 400, "a value that does not match the schema is refused before any write");
}

// #4616 — the pre-seal approve gate resolves its tier policy from the draft's
// own registry rather than from the file the host booted with, so the tier a
// member is admitted at is a property of the bloom.
//
// The A/B is the whole point and both halves run in one process against one
// file policy: the same declared surface, the same projection with no signed
// statement, refused under the file policy (`crates/aether-data/**` is `human`)
// and admitted under a sealed policy that resolves it `auto`. Nothing else about
// the request changes, so a pass cannot come from anywhere but the sealed value.
//
// The refusals pinned alongside it are the security decisions, not error
// plumbing: a member sealing its own policy would choose the tier admitting that
// member, and a bloom-wide address with no content must refuse rather than fall
// back to the file — falling back would admit the bloom at a tier its own
// receipt contradicts.
#[test]
fn a_sealed_approval_policy_decides_the_tier_the_file_policy_would_refuse() {
    let (_policy_dir, policy_path) = test_policy();
    let coordinator = spawn(&policy_path);
    let http_port = announced_port(&coordinator, Ingress::Http);

    wait_for_200(http_port, "/drafts");
    wait_for_200(http_port, "/view");

    // The surface the file policy resolves `human`. An auto-only stored
    // approval (no signed statement) is signer-policy under that file, and
    // auto under a sealed policy that names the same surface.
    let revision = seed_commission_with(http_port, "wp-1", &["crates/aether-data/**"], "problem", false);
    let seal = || seal_body();

    let (status, body) =
        send_json(http_port, "POST", &format!("/drafts/{}/seal", draft_with(http_port, revision, None)), &seal());
    assert_eq!(status, 422, "under the file policy the surface is human and the seal fails closed: {body:?}");

    // A member that seals the policy deciding its own admission is refused
    // outright — neither resolved (self-authorization) nor ignored (a sealed
    // configuration nothing reads).
    let mut member_scoped = valid_draft("wp-1", revision);
    member_scoped.proposals[0].configs.insert::<ApprovalPolicy>(Digest::from_bytes([5; 32]));
    let (status, _) = send_json(
        http_port,
        "POST",
        &format!("/drafts/{}/seal", patch_draft(http_port, &serde_json::to_value(&member_scoped).unwrap())),
        &seal(),
    );
    assert_eq!(status, 422, "a member-scoped approval policy fails the seal closed");

    // A bloom-wide entry whose content was never authored refuses rather than
    // silently falling through to the file policy.
    let dangling = ApprovalPolicy { default: Tier::Auto, rules: vec![] };
    let (status, _) = send_json(
        http_port,
        "POST",
        &format!("/drafts/{}/seal", draft_with(http_port, revision, Some(registry_naming(dangling.address())))),
        &seal(),
    );
    assert_eq!(status, 422, "a sealed policy address with no stored content fails the seal closed");

    // The same surface, admitted `auto` by a policy the draft seals.
    let authored = ApprovalPolicy {
        default: Tier::Judge,
        rules: vec![ApprovalRule { glob: "crates/aether-data/**".to_owned(), tier: Tier::Auto }],
    };
    let (status, stored) = send_json(
        http_port,
        "POST",
        "/configs",
        &serde_json::json!({ "kind": "aether.bloomery.approval_policy", "value": authored }),
    );
    assert_eq!(status, 200, "the authored policy is durable before its address is returned: {stored:?}");
    let address = digest_at(&stored["digest"]);
    assert_eq!(address, authored.address(), "the route addresses the policy exactly as a typed seal would");

    let statement = owner_signed_at(AuthorityDoor::Approve, revision, revision.as_bytes().to_vec(), Vec::new());
    let (status, approved) =
        send_auth(http_port, "POST", "/commissions/wp-1/approvals", &serde_json::to_value(&statement).unwrap());
    assert_eq!(status, 201, "store an approval so the auto path is not an absent-approval refuse: {approved:?}");

    let sealed_draft = draft_with(http_port, revision, Some(registry_naming(address)));
    let (status, sealed) = send_json(http_port, "POST", &format!("/drafts/{sealed_draft}/seal"), &seal());
    assert_eq!(status, 200, "the sealed policy admits the surface the file policy refused: {sealed:?}");
    let bloom_id = bloom_hex(&sealed["outcome"]["Sealed"]);
    let (status, view) = get_json(http_port, &format!("/blooms/{bloom_id}"));
    assert_eq!(status, 200, "the bloom sealed under its own policy is live");
    assert_eq!(view["members"][0]["approval"]["kind"], "Approval", "the member carries a gate-formed approval");
}

/// A bloom-wide registry naming `address` as the approval policy.
fn registry_naming(address: Digest) -> ConfigRegistry {
    let mut configs = ConfigRegistry::default();
    configs.insert::<ApprovalPolicy>(address);
    configs
}

/// Open a draft shaped like [`valid_draft`] with `configs` as its bloom-wide
/// registry, and return its handle.
fn draft_with(http_port: u16, revision: Digest, configs: Option<ConfigRegistry>) -> String {
    let mut patch = serde_json::to_value(valid_draft("wp-1", revision)).unwrap();
    if let Some(configs) = configs {
        patch["configs"] = serde_json::to_value(&configs).unwrap();
    }
    patch_draft(http_port, &patch)
}

/// Open a fresh draft, PATCH `patch` into it, and return its handle.
fn patch_draft(http_port: u16, patch: &Value) -> String {
    let (status, opened) = send_json(http_port, "POST", "/drafts", &Value::Null);
    assert_eq!(status, 201, "open draft");
    let draft_id = opened["draft_id"].as_str().unwrap().to_owned();
    let (status, _) = send_json(http_port, "PATCH", &format!("/drafts/{draft_id}"), patch);
    assert_eq!(status, 200, "patch draft {draft_id}");
    draft_id
}

// ADR-0174 — the generic authoring route and the draft registry are only useful
// if the reducer dispatches the catalog they name. This drives that whole path:
// author a catalog over HTTP, PATCH and GET its registry, seal it, then decode
// the durable dispatch record. The exact profile assertion distinguishes the
// authored construct binding from the compiled fallback.
#[test]
fn authored_stage_catalog_reaches_the_dispatch_profile() {
    let (_policy_dir, policy_path) = test_policy();
    let store_dir = tempfile::tempdir().unwrap();
    let store_path = store_dir.path().join("bloomery.db");
    let store_path = store_path.to_str().unwrap();
    let coordinator = spawn_with_store(&policy_path, store_path);
    let http_port = announced_port(&coordinator, Ingress::Http);

    wait_for_200(http_port, "/drafts");

    let authored_profile = AgentProfile {
        harness: Harness::Grok,
        model: "grok-4.6-authored".to_owned(),
        effort: ReasoningEffort::Max,
        tools: ToolPolicy::ReadOnly,
    };
    let mut catalog = StageCatalog::line();
    catalog
        .bindings
        .iter_mut()
        .find(|binding| binding.stage == StageId::Construct)
        .expect("the compiled catalog binds Construct")
        .profile = authored_profile.clone();

    let (status, authored) = send_json(
        http_port,
        "POST",
        "/configs",
        &serde_json::json!({ "kind": "aether.bloomery.stage_catalog", "value": catalog }),
    );
    assert_eq!(status, 200, "the authored catalog is durable before its address is returned");
    let catalog_address = digest_at(&authored["digest"]);

    let mut configs = ConfigRegistry::default();
    configs.insert_named("aether.bloomery.stage_catalog", catalog_address);
    let (status, opened) = send_json(http_port, "POST", "/drafts", &Value::Null);
    assert_eq!(status, 201, "open draft");
    let draft_id = opened["draft_id"].as_str().unwrap();
    let revision = seed_commission(http_port, "wp-1", &["docs/guide/**"]);
    let mut patch = serde_json::to_value(valid_draft("wp-1", revision)).unwrap();
    patch["configs"] = serde_json::to_value(&configs).unwrap();
    // The registry goes up in canonical form and comes back with its address
    // spelled the way the `/artifacts/{digest}` path would spell it.
    let rendered_registry =
        serde_json::json!({ "entries": { "aether.bloomery.stage_catalog": hex_of(&catalog_address) } });
    let (status, patched) = send_json(http_port, "PATCH", &format!("/drafts/{draft_id}"), &patch);
    assert_eq!(status, 200, "patch the authored catalog address into the draft registry");
    assert_eq!(patched["draft"]["configs"], rendered_registry);
    let (status, fetched) = get_json(http_port, &format!("/drafts/{draft_id}"));
    assert_eq!(status, 200, "read patched draft");
    assert_eq!(fetched["draft"]["configs"], rendered_registry);

    wait_for_200(http_port, "/view");
    prove_green_base(announced_port(&coordinator, Ingress::Rpc), Digest::from_bytes([1; 32]));
    let (status, sealed) = send_json(http_port, "POST", &format!("/drafts/{draft_id}/seal"), &seal_body());
    assert_eq!(status, 200, "seal the draft carrying the authored catalog: {sealed:?}");

    let (status, days) = get_json(http_port, "/metrics/days");
    assert_eq!(status, 200, "days after a construct dispatch: {days:?}");
    assert!(days.is_array(), "a fattened days document must render as an array, not a summary object: {days:?}");
    let row = days.as_array().and_then(|rows| rows.last()).expect("a construct dispatch fills a dated day");
    assert!(
        row.get("label").and_then(Value::as_str).is_some_and(|label| label.starts_with("bloomery/daily/")),
        "the newest row is a civil day: {row:?}"
    );
    assert!(row.get("spend_micro_usd").is_some(), "days columns survived the summary-first renderer: {row:?}");

    let mut store = SqliteStore::open(store_path).unwrap();
    let entries = store.drain_topic(Topic::Dispatch).unwrap();
    assert_eq!(entries.len(), 1, "sealing dispatches the member's Construct attempt");
    let payload: DispatchPayload = from_bytes(&entries[0].payload).unwrap();
    assert_eq!(payload.stage, StageId::Construct);
    assert_eq!(payload.profile, authored_profile, "the dispatch carries the catalog profile the operator authored");
}

/// The orphan-claim release door over its own route (ADR-0179/ADR-0182).
///
/// The door and the binding `request_claim_release` hands to `aether.signing`
/// are otherwise untested from outside: no other case in this file reaches
/// `POST /claims/releases`, so a route dialling the wrong door — or binding to a
/// digest read out of the submitted envelope instead of recomputed from the
/// request body — would ship green. Each case here is genuinely signed by the
/// allowlisted owner and differs from the admitting one in exactly one of those
/// two axes.
#[test]
fn the_release_route_binds_its_own_door_and_request_digest() {
    run_with_bloomery("the_release_route_binds_its_own_door_and_request_digest", |http_port| {
        wait_for_200(http_port, "/view");

        // A holder this journal has never seen, so the reducer's orphanhood gate
        // passes and the only thing left to decide the outcome is the signature.
        let target = OrphanClaimRelease {
            ref_kind: ClaimRefKind::MainlineAdmission,
            expected_holder: BloomId(Digest::from_bytes([201; 32])),
        };
        let binding = target.request();
        let words = ORPHAN_CLAIM_RELEASE_WORDS.as_bytes().to_vec();
        let body = |authorization: &Statement| {
            serde_json::json!({
                "ref_kind": serde_json::to_value(&target.ref_kind).unwrap(),
                "expected_holder": serde_json::to_value(target.expected_holder).unwrap(),
                "authorization": serde_json::to_value(authorization).unwrap(),
            })
        };

        // Right door, wrong request. The words this door signs are a fixed
        // constant, so this envelope is byte-identical to the admitting one
        // except for the request it was minted for — which is precisely the
        // universal-release-token shape ADR-0182 closes.
        let other_request = Digest::from_bytes([202; 32]);
        let wrong_binding =
            owner_signed_at(AuthorityDoor::OrphanClaimRelease, other_request, words.clone(), vec![binding]);
        let (status, _) = send_json(http_port, "POST", "/claims/releases", &body(&wrong_binding));
        assert_eq!(status, 400, "a release signed for another request does not verify against this one");

        // Right request, wrong door — an answer-door envelope over the same
        // binding. Pins that the route dials `OrphanClaimRelease` specifically.
        let wrong_door = owner_signed_at(AuthorityDoor::Answer, binding, words.clone(), vec![binding]);
        let (status, _) = send_json(http_port, "POST", "/claims/releases", &body(&wrong_door));
        assert_eq!(status, 400, "a release signed at another door does not verify at this one");

        // Both right: verifies against the custodied allowlist and admits, and
        // the `202` hands back the request digest the coordinator recomputed
        // from the body. This is the case that would break if the route bound to
        // anything other than `OrphanClaimRelease` + `target.request()`.
        let authorized = owner_signed_at(AuthorityDoor::OrphanClaimRelease, binding, words, vec![binding]);
        let (status, accepted) = send_json(http_port, "POST", "/claims/releases", &body(&authorized));
        assert_eq!(status, 202, "an authorized release is accepted: {accepted:?}");
        assert_eq!(accepted["request"], hex_of(&binding), "the 202 hands back the recomputed request digest");
    });
}

/// The answer door over its route (ADR-0151, ADR-0182): malformed segments, an
/// unverifiable signature, and the two ways an envelope can be aimed at a hold
/// its signature never named — re-parented onto the path question, and genuinely
/// signed for the path question but pointed elsewhere.
fn assert_answer_door_binds_its_question(http_port: u16, bloom_id: &str) {
    // The answer route (ADR-0151, ADR-0182). The question rides the path, so
    // every malformed segment is a clean 400 rather than a panic.
    let question = Digest::from_bytes([222; 32]);
    let question_hex = hex_of(&question);
    let (status, _) =
        try_http(http_port, "POST", &format!("/blooms/{bloom_id}/answer/{question_hex}"), Some(b"not a statement"))
            .unwrap();
    assert_eq!(status, 400, "malformed answer body");
    let (status, _) = try_http(http_port, "POST", &format!("/blooms/xyz/answer/{question_hex}"), Some(b"{}")).unwrap();
    assert_eq!(status, 400, "malformed bloom id");
    let (status, _) = try_http(http_port, "POST", &format!("/blooms/{bloom_id}/answer/xyz"), Some(b"{}")).unwrap();
    assert_eq!(status, 400, "malformed question digest");

    // An author-signed answer whose signature does NOT verify against the
    // configured allowlist is rejected at the gate — a 400, never admitted.
    // The signature is empty (not a 64-byte ed25519 signature), so the real
    // provider refuses it where the fake always-valid provider would have
    // admitted it.
    let words = b"answer: choose A".to_vec();
    let unsigned = Statement {
        words: words.clone(),
        provenance: Provenance::AuthorSignature(SignatureEnvelope {
            signer: KeyId("owner".to_owned()),
            signature: vec![],
        }),
        parents: vec![question],
    };
    let (status, _) = send_json(
        http_port,
        "POST",
        &format!("/blooms/{bloom_id}/answer/{question_hex}"),
        &serde_json::to_value(&unsigned).unwrap(),
    );
    assert_eq!(status, 400, "an answer with an unverifiable signature is rejected at the gate");

    // The replay ADR-0182 closes. This answer is genuinely signed by the
    // allowlisted owner and carries the same words as the one below — but it was
    // signed for a *different* question, and re-parenting it onto this one does
    // not move the signature. Before the binding was signed this admitted,
    // because the only thing tying an envelope to a question was `parents`.
    let other_question = Digest::from_bytes([223; 32]);
    let mut replayed = owner_signed_at(AuthorityDoor::Answer, other_question, words.clone(), vec![other_question]);
    replayed.parents = vec![question];
    let (status, _) = send_json(
        http_port,
        "POST",
        &format!("/blooms/{bloom_id}/answer/{question_hex}"),
        &serde_json::to_value(&replayed).unwrap(),
    );
    assert_eq!(status, 400, "an answer signed for another question does not verify when re-parented onto this one");

    // The other direction, and the one a signature check alone cannot catch
    // (ADR-0182). This envelope is genuine *for the question the path names* —
    // it verifies — but its unsigned `parents` was rewritten to point at a
    // sibling hold. `Fact::AdoptAnswer` carries no question, so the reducer
    // takes its target from `parents`: verifying alone would admit this and
    // release `other_question`, a hold nobody signed for. The route refuses it
    // before the signature is even dialled, so the binding the path names and
    // the hold the reducer releases cannot come apart.
    let mut misparented = owner_signed_at(AuthorityDoor::Answer, question, words.clone(), vec![question]);
    misparented.parents = vec![other_question];
    let (status, _) = send_json(
        http_port,
        "POST",
        &format!("/blooms/{bloom_id}/answer/{question_hex}"),
        &serde_json::to_value(&misparented).unwrap(),
    );
    assert_eq!(status, 400, "an answer whose parents name a different question than the path is refused");

    // Membership is not enough, which is why the route requires equality with a
    // single-element list. The reducer releases the first parent that is an open
    // hold in the submitter's order (pinned in `aether-bloomery`'s
    // `the_released_hold_is_the_first_parent_in_submitter_order`), so this list
    // *contains* the path question and would still have released
    // `other_question` on a two-hold bloom.
    let mut both_parents = owner_signed_at(AuthorityDoor::Answer, question, words.clone(), vec![question]);
    both_parents.parents = vec![other_question, question];
    let (status, _) = send_json(
        http_port,
        "POST",
        &format!("/blooms/{bloom_id}/answer/{question_hex}"),
        &serde_json::to_value(&both_parents).unwrap(),
    );
    assert_eq!(status, 400, "parents merely containing the path question is refused, not just parents missing it");

    // A genuine author signature by the allowlisted `owner`, bound to the
    // question the path names, verifies and admits; adopting no held question
    // reduces to a clean rejection outcome — the sealed bloom carries no parked
    // hold — 200 carrying the reducer's refusal, mirroring how seal rejections
    // surface. This is the real custody path: the answer verified against the
    // host-local allowlist, not the retired always-valid stub.
    let answer = owner_signed_at(AuthorityDoor::Answer, question, words, vec![question]);
    let (status, answered) = send_json(
        http_port,
        "POST",
        &format!("/blooms/{bloom_id}/answer/{question_hex}"),
        &serde_json::to_value(&answer).unwrap(),
    );
    assert_eq!(status, 200, "answer admitted: {answered:?}");
    assert_eq!(answered["outcome"]["AdoptAnswerRejected"], "NoMatchingHold", "no parked hold to adopt");
}

// #5090 — the existing grant request is the operator door for a selected
// batch. The plausible bugs: the route dropping `attempts` so the reducer
// never sees the chosen count, swallowing a reducer refusal as a silent 2xx
// with no body, treating a repeated POST as a second grant, or letting a
// fresh key redispatch a member that already has a worker.
#[test]
fn grant_route_carries_the_selected_count_and_refuses_concurrent_work() {
    run_with_bloomery("grant_route_carries_the_selected_count_and_refuses_concurrent_work", |http_port| {
        let revision = seed_commission(http_port, "wp-1", &["docs/guide/**"]);
        let draft_id = draft_with(http_port, revision, None);
        let (status, sealed) = send_json(http_port, "POST", &format!("/drafts/{draft_id}/seal"), &seal_body());
        assert_eq!(status, 200, "seal admits: {sealed:?}");
        let bloom_id = bloom_hex(&sealed["outcome"]["Sealed"]);
        let path = format!("/blooms/{bloom_id}/grant");

        let grant = |attempts: u32, key: Option<&str>| {
            let mut body = serde_json::json!({
                "workpiece": "wp-1",
                "stage": "Construct",
                "attempts": attempts,
                "reason": "sandbox recovered",
                "operator": "eve",
            });
            if let Some(key) = key {
                body["idempotency_key"] = Value::String(key.to_owned());
            }
            body
        };

        // A just-sealed member is mid-flight. The grant is admitted so the
        // reducer can refuse it; the route does not predict NotWedged.
        let (status, refused) = send_json(http_port, "POST", &path, &grant(2, None));
        assert_eq!(status, 422, "a reducer refusal is 422 like the other operator doors: {refused:?}");
        assert_eq!(
            refused["outcome"]["GrantAttemptsRejected"]["NotWedged"], "wp-1",
            "the refusal stays visible: {refused:?}"
        );

        // The selected count is on the journaled fact, and it is part of the
        // default idempotency key: the same body is a duplicate, a different
        // count is a new admission.
        let (status, journal) = get_json(http_port, "/journal");
        assert_eq!(status, 200, "journal");
        let granted = journal["records"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find_map(|record| record["event"]["fact"]["GrantAttempts"].as_object())
            .expect("the grant was journaled");
        assert_eq!(granted["attempts"], 2, "the request's selected count reached the fact: {granted:?}");

        let (status, again) = send_json(http_port, "POST", &path, &grant(2, None));
        assert_eq!(status, 200, "{again:?}");
        assert_eq!(again["outcome"], "Duplicate", "a repeated POST of the same key is a no-op: {again:?}");

        let (status, other_count) = send_json(http_port, "POST", &path, &grant(1, None));
        assert_eq!(status, 422, "{other_count:?}");
        assert_eq!(
            other_count["outcome"]["GrantAttemptsRejected"]["NotWedged"], "wp-1",
            "a different count is a new admission, not a duplicate: {other_count:?}"
        );

        let (status, fresh) = send_json(http_port, "POST", &path, &grant(2, Some("grant-fresh")));
        assert_eq!(status, 422, "{fresh:?}");
        assert_eq!(
            fresh["outcome"]["GrantAttemptsRejected"]["NotWedged"], "wp-1",
            "a fresh key still cannot put two workers on one member: {fresh:?}"
        );
    });
}

#[test]
fn an_amend_shaped_revision_files_a_scope_verify_report() {
    // An operator freeze that files no report reads exactly like a hand-authored
    // one. The xtask client path now carries the sidecar so the report lands.
    let (_policy_dir, policy_path) = test_policy();
    let store_dir = tempfile::tempdir().unwrap();
    let store_path = store_dir.path().join("bloomery.db");
    let store_path = store_path.to_str().unwrap();
    let coordinator = spawn_with_store(&policy_path, store_path);
    let http_port = announced_port(&coordinator, Ingress::Http);

    wait_for_200(http_port, "/view");

    let intent = Statement {
        words: b"intent wp-amend".to_vec(),
        provenance: Provenance::ObservationAttestation(Observation { source: "rest-api".to_owned() }),
        parents: Vec::new(),
    };
    let (status, created) =
        send_auth(http_port, "POST", "/commissions", &serde_json::json!({ "id": "wp-amend", "intent": intent }));
    assert_eq!(status, 201, "create commission: {created:?}");

    let surface = vec!["docs/guide/**".to_owned()];
    let revision = ScopeRevision {
        schema: SCOPE_REVISION_SCHEMA,
        workpiece: WorkpieceId("wp-amend".to_owned()),
        predecessor: None,
        problem: "problem".to_owned(),
        design: "design".to_owned(),
        plan: "plan".to_owned(),
        declared_surface: surface.clone(),
        dogfood_brief: "dogfood".to_owned(),
        routing: ScopeRouting { size: "M".to_owned(), model: "construct: test".to_owned() },
        dependencies: Vec::new(),
        description: "task for wp-amend".to_owned(),
        implements: Vec::new(),
        declared_crates: Vec::new(),
        declared_reads: Vec::new(),
    };
    let input = ScopeVerifyInput {
        schema: SCOPE_VERIFY_SCHEMA,
        named_paths: vec![NamedPath {
            path: "docs/guide/SUMMARY.md".to_owned(),
            origin: PathOrigin::PlanStep { step: 1 },
        }],
        named_symbols: Vec::new(),
        declared_surface: surface,
    };
    let (status, written) = send_auth(
        http_port,
        "POST",
        "/commissions/wp-amend/revisions",
        &serde_json::json!({
            "revision": revision,
            "evidence": { "scope_verify": input },
        }),
    );
    assert_eq!(status, 201, "write revision: {written:?}");
    let digest = digest_at(&written["digest"]);

    let store = SqliteStore::open(store_path).unwrap();
    let report = store.load_scope_verify_report(digest).expect("load report").expect("the sidecar filed a report");
    assert_eq!(report, verify_scope(&input));
}

#[test]
fn posting_a_scope_run_enqueues_the_host_minted_topic() {
    // The trigger: POST /commissions/{id}/scope-runs writes the enqueued row
    // and the Topic::ScopeDispatch outbox row in one transaction, so the
    // executor can drain a pre-bloom run with no bloom in the store.
    let (_policy_dir, policy_path) = test_policy();
    let store_dir = tempfile::tempdir().unwrap();
    let store_path = store_dir.path().join("bloomery.db");
    let store_path = store_path.to_str().unwrap();
    let coordinator = spawn_with_store(&policy_path, store_path);
    let http_port = announced_port(&coordinator, Ingress::Http);

    wait_for_200(http_port, "/view");

    let intent = Statement {
        words: b"scope this workpiece".to_vec(),
        provenance: Provenance::ObservationAttestation(Observation { source: "rest-api".to_owned() }),
        parents: Vec::new(),
    };
    let (status, created) =
        send_auth(http_port, "POST", "/commissions", &serde_json::json!({ "id": "wp-scope", "intent": intent }));
    assert_eq!(status, 201, "create commission: {created:?}");

    let (status, view) = get_json(http_port, "/view");
    assert_eq!(status, 200, "view");
    let (status, opened) = send_auth(
        http_port,
        "POST",
        "/commissions/wp-scope/scope-runs",
        &serde_json::json!({ "base": view["mainline"] }),
    );
    assert_eq!(status, 201, "open scope run: {opened:?}");
    assert_eq!(opened["ordinal"], 1, "the first run is ordinal 1: {opened:?}");
    assert_eq!(opened["id"], "wp-scope");

    let mut store = SqliteStore::open(store_path).unwrap();
    let entries = store.drain_topic(Topic::ScopeDispatch).expect("drain");
    assert_eq!(entries.len(), 1, "one POST, one outbox row");
    assert_eq!(entries[0].sequence, opened["sequence"].as_u64().expect("sequence is a u64"));
}

#[test]
fn a_commission_whose_tip_predates_a_field_append_can_be_shown_and_healed() {
    // Before the fix, GET /commissions/{id} over a short version-1 row is the
    // 500 an operator cannot act on. After it, show returns the tip digest and
    // a successor that names that digest as predecessor brings the id back.
    let (_policy_dir, policy_path) = test_policy();
    let store_dir = tempfile::tempdir().unwrap();
    let store_path = store_dir.path().join("bloomery.db");
    let store_path = store_path.to_str().unwrap();

    let intent = Statement {
        words: b"intent wp-old".to_vec(),
        provenance: Provenance::ObservationAttestation(Observation { source: "rest-api".to_owned() }),
        parents: Vec::new(),
    };
    let mut old_bytes = ScopeRevision {
        schema: SCOPE_REVISION_SCHEMA,
        workpiece: WorkpieceId("wp-old".to_owned()),
        predecessor: None,
        problem: "problem".to_owned(),
        design: "design".to_owned(),
        plan: "plan".to_owned(),
        declared_surface: vec!["docs/guide/**".to_owned()],
        dogfood_brief: "dogfood".to_owned(),
        routing: ScopeRouting { size: "M".to_owned(), model: "construct: test".to_owned() },
        dependencies: Vec::new(),
        description: "task for wp-old".to_owned(),
        implements: Vec::new(),
        declared_crates: Vec::new(),
        declared_reads: Vec::new(),
    }
    .to_canonical();
    old_bytes.truncate(old_bytes.len() - 8);
    let old_digest = Digest::of_domain_tagged(ScopeRevision::DOMAIN, &old_bytes);
    {
        let mut store = SqliteStore::open(store_path).unwrap();
        store.create(&WorkpieceId("wp-old".to_owned()), &intent).expect("create commission");
    }
    {
        let conn = rusqlite::Connection::open(store_path).unwrap();
        conn.execute(
            "INSERT INTO scope_revisions (digest, commission, predecessor, ordinal, canonical)
             VALUES (?1, ?2, NULL, 1, ?3)",
            rusqlite::params![old_digest.as_bytes().as_slice(), "wp-old", old_bytes],
        )
        .expect("plant a pre-append revision");
        conn.execute(
            "UPDATE commissions SET current_revision = ?1, current_ordinal = 1 WHERE id = ?2",
            rusqlite::params![old_digest.as_bytes().as_slice(), "wp-old"],
        )
        .expect("point the tip at the planted row");
    }

    let coordinator = spawn_with_store(&policy_path, store_path);
    let http_port = announced_port(&coordinator, Ingress::Http);
    wait_for_200(http_port, "/view");

    let (status, shown) = get_auth(http_port, "/commissions/wp-old");
    assert_eq!(status, 200, "an older tip must not 500: {shown:?}");
    assert_eq!(digest_at(&shown["current_revision"]), old_digest, "show names the tip an operator can chain from");
    assert!(
        shown["current_unreadable"].as_str().is_some_and(|reason| reason.contains("malformed")),
        "the body is named unreadable, not silently absent: {shown:?}"
    );

    let successor = ScopeRevision {
        schema: SCOPE_REVISION_SCHEMA,
        workpiece: WorkpieceId("wp-old".to_owned()),
        predecessor: Some(old_digest),
        problem: "healed".to_owned(),
        design: "design".to_owned(),
        plan: "plan".to_owned(),
        declared_surface: vec!["docs/guide/**".to_owned()],
        dogfood_brief: "dogfood".to_owned(),
        routing: ScopeRouting { size: "M".to_owned(), model: "construct: test".to_owned() },
        dependencies: Vec::new(),
        description: "task for wp-old".to_owned(),
        implements: Vec::new(),
        declared_crates: Vec::new(),
        declared_reads: Vec::new(),
    };
    let (status, written) =
        send_auth(http_port, "POST", "/commissions/wp-old/revisions", &serde_json::json!({ "revision": successor }));
    assert_eq!(status, 201, "writing forward heals the id: {written:?}");
    let next = digest_at(&written["digest"]);

    let (status, shown) = get_auth(http_port, "/commissions/wp-old");
    assert_eq!(status, 200, "{shown:?}");
    assert_eq!(digest_at(&shown["current_revision"]), next);
    assert_eq!(shown["current"]["problem"], "healed");
}

/// Endpoint laws the REST surface documents: a limit above the clamp is applied
/// and named, percent-decoding is over the whole byte string, and a grant
/// carries audit fields whose reducer refusal is `422`.
#[test]
fn rest_endpoint_laws_clamp_decode_and_unify_refusals() {
    run_with_bloomery("rest_endpoint_laws_clamp_decode_and_unify_refusals", |http_port| {
        wait_for_200(http_port, "/view");

        // Percent-decode the query, then clamp: `1001` encoded as `%31%30%30%31`.
        // Per-byte decode would still parse as ASCII here; a missing decode is
        // a `400` because the raw escapes are not an integer.
        let (status, journal, _) = get_json_and_notice(http_port, "/journal?limit=%31%30%30%31");
        assert_eq!(status, 200, "a percent-encoded over-cap limit is applied, not refused: {journal:?}");
        assert!(
            journal["notice"].as_str().is_some_and(|text| text.contains("clamped") && text.contains("1001")),
            "the journal names the clamp: {journal:?}"
        );

        let (status, _, notice) = get_json_and_notice(http_port, "/metrics/blooms?limit=1001");
        assert_eq!(status, 200, "metrics pages clamp rather than refuse");
        assert!(
            notice.as_deref().is_some_and(|text| text.contains("clamped") && text.contains("1001")),
            "metrics names the clamp on x-aether-notice (the body is a bare array): {notice:?}"
        );

        let (status, days) = get_json(http_port, "/metrics/days");
        assert_eq!(status, 200, "empty days still 200: {days:?}");
        assert!(days.is_array(), "days is a bare array even when empty, not a summary object: {days:?}");

        // `%C3%A9` is UTF-8 `é`. Decoding each escaped byte as a char would
        // produce Latin-1 mojibake; either way the query parses, so a `400` is
        // the missing-decode failure. Unavailable journald is `501`.
        let (status, body) = try_http(http_port, "GET", "/logs/coordinator?contains=%C3%A9", None).unwrap();
        assert_ne!(status, 400, "a UTF-8 percent-encoded contains must parse: {status} {body:?}");
    });
}

/// Fold a Construct → mechanical-Verify → unpriced-study → integrate journal
/// through both seat ledgers. The test asserts the two tables agree.
fn fold_unpriced_construct_seats() -> (Vec<MetricsSeat>, CapabilityLedger) {
    use aether_bloomery::{
        AgentSelection, CalibrationLedger, CandidateRef, Decision, Fact, MetricsLedger, ModelOverride, ResolvedConfigs,
        Snapshot, SpendWindow, StudyCost, StudyRecord, reduce,
    };
    use aether_data::Kind;
    use aether_data::wire::to_vec;

    const MEMBER: &str = "wp-a";
    const REVISION: u8 = 10;
    const TREE: u8 = 100;

    let override_ = ModelOverride {
        agent: Some(AgentSelection { harness: Harness::Claude, model: "claude-opus-5".into() }),
        ..ModelOverride::default()
    };
    let mut member = member(MEMBER, digest(REVISION), digest(200));
    member.configs.insert::<ModelOverride>(override_.address());
    member.approval.subject = member.subject();

    let mut configs = ResolvedConfigs::default();
    configs.insert(override_.address(), ModelOverride::NAME, to_vec(&override_).expect("override encodes"), None);

    let spec = BloomDraft { proposals: vec![member], base: digest(1), ..BloomDraft::default() }.seal();
    let bloom = spec.id();

    let mut snapshot = Snapshot::new(digest(1)).with_green_base(digest(1));
    let mut metrics = MetricsLedger::default();
    let mut calibration = CalibrationLedger::default();
    let mut sequence = 0_u64;
    let mut admit = |event: &aether_bloomery::Event| {
        sequence = sequence.saturating_add(1);
        let decisions = reduce(&snapshot, event, &configs, &SpendWindow::default());
        metrics.observe(sequence, event, &decisions, &configs, Some(1_000));
        calibration.observe(event, &decisions, &configs);
        snapshot = snapshot.apply(event, &decisions, &configs);
        decisions
    };

    admit(&event("seal", Fact::Seal(spec)));
    let captured = CandidateRef { tree: digest(TREE), checkout: digest(TREE + 1) };
    let completed = admit(&event(
        "construct",
        Fact::AttemptCompleted {
            bloom,
            workpiece: WorkpieceId(MEMBER.to_owned()),
            stage: StageId::Construct,
            passed: true,
            evidence: Evidence { subject: digest(TREE), kind: EvidenceKind::VerificationResult, detail: digest(90) },
            candidate: Some(captured),
        },
    ));
    assert!(
        completed
            .effects
            .iter()
            .any(|effect| matches!(effect, Decision::DispatchAttempt { stage: StageId::Verify, .. })),
        "passing Construct dispatches the mechanical Verify fan-out: {completed:?}"
    );

    admit(&event(
        "study",
        Fact::AdmitEvidence {
            bloom,
            evidence: Evidence { subject: digest(REVISION), kind: EvidenceKind::StudyRecord, detail: digest(40) },
        },
    ));
    admit(&event(
        "integrate",
        Fact::Integrate {
            bloom,
            claim: aether_bloomery::ResolutionClaim {
                workpiece: WorkpieceId(MEMBER.to_owned()),
                scope_revision: digest(REVISION),
                candidate: digest(TREE),
                evidence: Evidence { subject: digest(TREE), kind: EvidenceKind::ResolutionClaim, detail: digest(201) },
            },
        },
    ));

    let records = std::collections::BTreeMap::from([(
        digest(40),
        StudyRecord {
            bloom,
            subject: digest(REVISION),
            cost: StudyCost { cost_micro_usd: 0, input_tokens: 10, output_tokens: 2, ..StudyCost::default() },
        },
    )]);
    let source = |asked: &Digest| records.get(asked).copied();
    (metrics.seats(source), calibration.report(source))
}

/// Both seat ledgers fold the same dispatch and price the same way: a mechanical
/// Verify does not mint a seat, and an unpriced study record is counted as
/// unpriced rather than averaged in as free.
#[test]
fn seat_ledgers_share_the_model_lane_gate_and_unpriced_cost() {
    let (seats, ledger) = fold_unpriced_construct_seats();
    assert!(
        seats.iter().all(|seat| seat.stage != StageId::Verify),
        "a mechanical Verify must not mint a metrics seat: {seats:?}"
    );
    let construct = seats.iter().find(|seat| seat.stage == StageId::Construct).expect("Construct is a model-lane seat");
    assert_eq!(construct.unpriced, 1, "the zero-priced record is unpriced, not a free sample");
    assert_eq!(construct.priced_samples, 0);
    assert_eq!(construct.mean_cost_micro_usd(), None, "unpriced must not flatten to a zero mean");

    assert!(
        ledger.cells.iter().all(|cell| cell.stage != StageId::Verify),
        "a mechanical Verify must not mint a calibration cell: {:?}",
        ledger.cells
    );
    let construct = ledger.cells.iter().find(|cell| cell.stage == StageId::Construct).expect("Construct is measured");
    assert_eq!(construct.unpriced, 1, "calibration surfaces the missing price row");
    assert_eq!(construct.samples, 0, "an unpriced record is not a priced sample");
    assert_eq!(
        construct.cost_per_resolved_member(),
        None,
        "cost-per-member is unmeasured when a price row is missing, never Some(0)"
    );
}

fn get_json_and_notice(port: u16, path: &str) -> (u16, Value, Option<String>) {
    let (status, headers, body) = request(port, "GET", path, None, None).unwrap();
    let notice = headers.iter().find(|(name, _)| name == "x-aether-notice").map(|(_, value)| value.clone());
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null), notice)
}

fn commission_cli(http_port: u16, rest: &[&str]) -> Result<String, anyhow::Error> {
    let port = http_port.to_string();
    let mut invocation = vec![
        "bloomery-commission".to_owned(),
        "--http-port".to_owned(),
        port,
        "--token".to_owned(),
        CONTROL_TOKEN.to_owned(),
    ];
    invocation.extend(rest.iter().map(|word| (*word).to_owned()));
    commission::run(invocation)
}

fn author_scope_markdown() -> &'static str {
    "\
## Problem statement\n\
\n\
Need a one-command author path.\n\
\n\
## Design notes\n\
\n\
Compose create, scope, and approve.\n\
\n\
## Implementation plan\n\
\n\
Ship bloomery-commission author.\n\
\n\
**Size:** m\n\
**Implementation model:** grok-4.6\n\
**Routing reason:** focused CLI\n\
\n\
## Declared surface\n\
\n\
```text\n\
crates/aether-chassis-bloomery/**\n\
```\n\
\n\
## Dogfood brief\n\
\n\
Author then show.\n"
}

fn write_owner_seed(path: &std::path::Path) {
    std::fs::write(path, [42u8; 32]).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

fn spawn_os_ports(policy_path: &str) -> Coordinator {
    let allowlist = owner_allowlist();
    Coordinator::spawn(
        0,
        &[
            ("AETHER_STORE_PATH", ":memory:"),
            ("AETHER_SIGNING_ALLOWLIST", &allowlist),
            ("AETHER_APPROVAL_POLICY_FILE", policy_path),
            ("AETHER_HTTP_CONTROL_TOKEN", CONTROL_TOKEN),
        ],
    )
}

fn spawn_author_ready(policy_path: &str) -> (u16, Coordinator) {
    let (coordinator, _stream) =
        common::client::spawn_and_connect("rest-author", Duration::from_mins(1), || spawn_os_ports(policy_path));
    (wait_author_http(&coordinator), coordinator)
}

/// The child's REST port, once `/drafts` answers on it.
///
/// Two gates, and both are needed: the child announces the port it bound, and
/// the first `200` is what says the routes behind it have registered.
fn wait_author_http(coordinator: &Coordinator) -> u16 {
    let deadline = Instant::now() + Duration::from_secs(30);
    let port = announced_port(coordinator, Ingress::Http);
    // Assigned on every path that reaches the assert, so it always names the
    // refusal the last attempt actually met.
    let mut last;
    loop {
        match try_http(port, "GET", "/drafts", None) {
            Ok((200, _)) => return port,
            Ok((status, _)) => last = format!("/drafts returned {status}"),
            Err(error) => last = error.to_string(),
        }
        assert!(Instant::now() < deadline, "coordinator HTTP never became ready for author: {last}");
        thread::sleep(Duration::from_millis(50));
    }
}

/// `bloomery-commission author` creates, scopes, and approves against a live
/// coordinator: the printed digest is the local `digest_of` of the revision,
/// the commission is open, and an approval is stored over that tip.
#[test]
fn author_opens_a_commission_with_an_approval_over_the_local_digest() {
    let (_policy_dir, policy_path) = test_policy();
    let (http_port, _coordinator) = spawn_author_ready(&policy_path);

    let dir = tempfile::tempdir().unwrap();
    let intent = dir.path().join("intent.txt");
    let scope = dir.path().join("scope.md");
    let seed = dir.path().join("seed");
    let ledger = dir.path().join("ledger");
    std::fs::write(&intent, b"ship the author verb").unwrap();
    std::fs::write(&scope, author_scope_markdown()).unwrap();
    write_owner_seed(&seed);

    let output = commission_cli(
        http_port,
        &[
            "author",
            "--id",
            "wp-author",
            "--intent-file",
            intent.to_str().unwrap(),
            "--scope-file",
            scope.to_str().unwrap(),
            "--seed-file",
            seed.to_str().unwrap(),
            "--signer",
            "owner",
            "--ledger",
            ledger.to_str().unwrap(),
            "--approval-policy",
            &policy_path,
        ],
    )
    .unwrap_or_else(|error| panic!("author: {error}"));

    let markdown = std::fs::read_to_string(&scope).unwrap();
    let expected = digest_of(&commission::parse_revision("wp-author", &markdown, None).unwrap());
    let line = format!("wp-author={}", expected.to_hex());
    assert_eq!(output, format!("{line}\n"), "author prints id=digest of the local revision");
    assert_eq!(std::fs::read_to_string(&ledger).unwrap(), format!("{line}\n"), "ledger records the same line");

    let (status, body) = try_http_auth(http_port, "GET", "/commissions/wp-author", None, Some(CONTROL_TOKEN)).unwrap();
    assert_eq!(status, 200, "show the authored commission: {body:?}");
    let shown: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(shown["status"], "open", "authored commission is open: {shown:?}");
    assert_eq!(
        shown["current_revision"].as_str().unwrap(),
        expected.to_hex(),
        "tip is the locally computed digest: {shown:?}"
    );
    let approvals = shown["approvals"].as_array().expect("approvals list");
    assert_eq!(approvals.len(), 1, "one approval over the tip: {shown:?}");
    let words = approvals[0]["words"].as_array().expect("approval words");
    let expected_words: Vec<Value> = expected.as_bytes().iter().copied().map(Value::from).collect();
    assert_eq!(words, &expected_words, "approval is stored over the tip digest");
}

/// `author` must refuse when the coordinator stores a different address than
/// `digest_of` of the revision just written — otherwise it would sign an
/// approval over unread bytes.
#[test]
fn author_refuses_when_the_stored_digest_differs() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let http_port = listener.local_addr().unwrap().port();
    let approved = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&approved);
    let stop = Arc::clone(&done);
    let server = thread::spawn(move || serve_mismatched_revision(&listener, &flag, &stop));

    let dir = tempfile::tempdir().unwrap();
    let intent = dir.path().join("intent.txt");
    let scope = dir.path().join("scope.md");
    let seed = dir.path().join("seed");
    let policy = dir.path().join("approval-policy.toml");
    std::fs::write(&intent, b"ship the author verb").unwrap();
    std::fs::write(&scope, author_scope_markdown()).unwrap();
    std::fs::write(&policy, b"default = \"judge\"\n").unwrap();
    write_owner_seed(&seed);

    let wrong = Digest::from_bytes([0x11; 32]).to_hex();
    match commission_cli(
        http_port,
        &[
            "author",
            "--id",
            "wp-mismatch",
            "--intent-file",
            intent.to_str().unwrap(),
            "--scope-file",
            scope.to_str().unwrap(),
            "--seed-file",
            seed.to_str().unwrap(),
            "--signer",
            "owner",
            "--approval-policy",
            policy.to_str().unwrap(),
        ],
    ) {
        Ok(output) => panic!("a mismatched stored digest must be refused, got {output}"),
        Err(error) => {
            let message = error.to_string();
            assert!(message.contains(&wrong), "refusal names the stored digest: {message}");
            assert!(message.contains("addressed"), "refusal names the local address: {message}");
        }
    }
    done.store(true, Ordering::SeqCst);
    let _ = server.join();
    assert!(!approved.load(Ordering::SeqCst), "a mismatched digest must not be signed over");
}

fn serve_mismatched_revision(listener: &TcpListener, approved: &AtomicBool, done: &AtomicBool) {
    let wrong = Digest::from_bytes([0x11; 32]).to_hex();
    let intent = Digest::from_bytes([0x22; 32]).to_hex();
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !done.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => handle_stub_conn(stream, approved, &wrong, &intent),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::Interrupted =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("stub accept: {error}"),
        }
    }
}

fn handle_stub_conn(mut stream: TcpStream, approved: &AtomicBool, wrong: &str, intent: &str) {
    stream.set_nonblocking(false).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let request = read_http_request(&mut stream);
    let head = std::str::from_utf8(&request).unwrap_or("");
    let line = head.lines().next().unwrap_or("");
    let (status, body) = if line.starts_with("POST /commissions/wp-mismatch/approvals") {
        approved.store(true, Ordering::SeqCst);
        (201, format!(r#"{{"digest":"{wrong}"}}"#))
    } else if line.starts_with("POST /commissions/wp-mismatch/revisions") {
        (201, format!(r#"{{"digest":"{wrong}"}}"#))
    } else if line.starts_with("POST /commissions") {
        (201, format!(r#"{{"id":"wp-mismatch","intent":"{intent}"}}"#))
    } else if line.starts_with("GET /commissions/wp-mismatch") {
        (
            200,
            format!(r#"{{"id":"wp-mismatch","intent":"{intent}","status":"open","approvals":[],"scope_verify":null}}"#),
        )
    } else {
        (404, r#"{"error":"unexpected"}"#.to_owned())
    };
    let response = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
}

fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buf = [0_u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => request.extend_from_slice(&buf[..n]),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut
                    || error.kind() == std::io::ErrorKind::Interrupted =>
            {
                break;
            }
            Err(error) => panic!("stub read: {error}"),
        }
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let head = std::str::from_utf8(&request[..header_end]).unwrap_or("");
        let content_length = head.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length").then_some(())?;
            value.trim().parse::<usize>().ok()
        });
        match content_length {
            Some(length) if request.len() >= header_end + 4 + length => break,
            None => break,
            Some(_) => {}
        }
    }
    request
}
