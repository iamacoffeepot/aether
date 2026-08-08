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

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use aether_bloomery::{
    BloomDraft, ConfigRegistry, Digest, Evidence, EvidenceKind, KeyId, Membership, Provenance, SignatureEnvelope,
    StageCatalog, Statement, Workpiece, WorkpieceId,
};
use aether_chassis_bloomery::api::{MemberProjection, SealRequest};
use aether_chassis_bloomery::bloomery::{AdrTouch, Completeness};
use ed25519_dalek::{Signer, SigningKey};
use serde_json::Value;

/// The scope revision `valid_draft`'s single member pins — the seal projection
/// must key its projection to this exact revision to match the proposal.
fn member_revision() -> Digest {
    Digest::from_bytes([7; 32])
}

/// A `Completeness` with every gate check satisfied — the base a negative case
/// flips one field of.
fn complete() -> Completeness {
    Completeness {
        has_problem_statement: true,
        has_design_notes: true,
        has_implementation_plan: true,
        referenced_adr_prs_merged: true,
        model_routing_count: 1,
        blocked: false,
        declared_surface_fresh: true,
        dependencies_all_closed: true,
        umbrella_integrity: true,
    }
}

/// A member projection for `wp-1` over `surface` with a complete, non-ADR,
/// non-pre-approved revision — the seal-time input the pre-seal gate decides.
fn projection(surface: &[&str], completeness: Completeness) -> MemberProjection {
    MemberProjection {
        workpiece: WorkpieceId("wp-1".to_owned()),
        scope_revision: member_revision(),
        declared_surface: surface.iter().map(|glob| (*glob).to_owned()).collect(),
        completeness,
        adr_touch: AdrTouch::None,
        pre_approved: false,
        signed_statement: None,
    }
}

/// A `wp` projection over `surface` at `revision`, complete and non-pre-approved,
/// optionally carrying the above-auto `signed_statement` — the seal-time input
/// the deferred-verify enforcement (#3599) decides.
fn projection_at(
    wp: &str,
    revision: Digest,
    surface: &[&str],
    signed_statement: Option<Statement>,
) -> MemberProjection {
    MemberProjection {
        workpiece: WorkpieceId(wp.to_owned()),
        scope_revision: revision,
        declared_surface: surface.iter().map(|glob| (*glob).to_owned()).collect(),
        completeness: complete(),
        adr_touch: AdrTouch::None,
        pre_approved: false,
        signed_statement,
    }
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

/// An author-signed statement over `subject`'s bytes by the allowlisted `owner`
/// — the above-auto member's owner approval the seal path verifies through the
/// `aether.signing` capability.
fn owner_signed_statement(subject: Digest) -> Statement {
    let words = subject.as_bytes().to_vec();
    Statement {
        words: words.clone(),
        provenance: Provenance::AuthorSignature(SignatureEnvelope {
            signer: KeyId("owner".to_owned()),
            signature: owner_signing_key().sign(&words).to_bytes().to_vec(),
        }),
        parents: vec![],
    }
}

/// A `SealRequest` carrying `projections`, serialized to a JSON value for the
/// seal route.
fn seal_body(projections: Vec<MemberProjection>) -> Value {
    serde_json::to_value(SealRequest { idempotency_key: None, projections, ..Default::default() }).unwrap()
}

/// Write a self-contained tier policy to a temp dir and return it plus the
/// policy path. `docs/guide/**` resolves `auto`; `crates/aether-data/**` resolves
/// `human` (above-auto); the default is `judge`. Kept independent of the evolving
/// repo policy so the gate cases are deterministic.
fn test_policy() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("approval-policy.yml");
    std::fs::write(
        &path,
        "default: judge\nrules:\n  - glob: \"docs/guide/**\"\n    tier: auto\n  - glob: \"crates/aether-data/**\"\n    tier: human\n",
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
/// public half (`key-id:hex-public-key`).
fn owner_allowlist() -> String {
    let hex: String = owner_signing_key().verifying_key().to_bytes().iter().map(|b| format!("{b:02x}")).collect();
    format!("owner:{hex}")
}

/// Reserve a free localhost port by binding `:0`, then release it for the bin to
/// claim (a small race the connect-retry loop tolerates).
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// Fork the `bloomery` bin with the HTTP ingress and control core autoloaded,
/// pointing the pre-seal approve gate at `policy_path` (#3583).
fn spawn(http_port: u16, rpc_port: u16, policy_path: &str) -> Child {
    Command::new(env!("CARGO_BIN_EXE_bloomery"))
        .env("AETHER_HTTP_PORT", http_port.to_string())
        .env("AETHER_RPC_PORT", rpc_port.to_string())
        .env("AETHER_STORE_PATH", ":memory:")
        .env("AETHER_SIGNING_ALLOWLIST", owner_allowlist())
        .env("AETHER_APPROVAL_POLICY_FILE", policy_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// One HTTP request over a fresh `Connection: close` socket; returns the status
/// code and the response body bytes. `Err` when the socket cannot be reached
/// yet (the bin is still binding).
fn try_http(port: u16, method: &str, path: &str, body: Option<&[u8]>) -> Result<(u16, Vec<u8>), std::io::Error> {
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
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

/// Split an HTTP response into its status code and body bytes.
fn parse_response(response: &[u8]) -> (u16, Vec<u8>) {
    let separator = b"\r\n\r\n";
    let head_end = response.windows(separator.len()).position(|w| w == separator).expect("response has a head");
    let head = &response[..head_end];
    let body = response[head_end + separator.len()..].to_vec();
    let status_line = head.split(|&b| b == b'\r').next().unwrap();
    let status = std::str::from_utf8(status_line)
        .unwrap()
        .split_whitespace()
        .nth(1)
        .expect("status line has a code")
        .parse()
        .expect("status code parses");
    (status, body)
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

/// Hex-encode a `BloomId` rendered as its serde byte-array JSON, for the
/// `/blooms/{id}` path (which addresses blooms by hex digest).
fn bloom_hex(bloom_id: &Value) -> String {
    bloom_id
        .as_array()
        .expect("bloom id is a byte array")
        .iter()
        .map(|byte| format!("{:02x}", byte.as_u64().expect("byte is a number")))
        .collect()
}

/// A valid single-workpiece draft the reducer admits: the member's approval
/// evidence binds its own scope revision, and the stage catalog is the one line
/// the reducer requires (`StageCatalog::line_digest`, not the zero default).
fn valid_draft(workpiece: &str) -> BloomDraft {
    let scope_revision = Digest::from_bytes([7; 32]);
    BloomDraft {
        proposals: vec![member(workpiece, scope_revision, Digest::from_bytes([9; 32]))],
        base: Digest::from_bytes([1; 32]),
        stage_catalog: StageCatalog::line_digest(),
        ..BloomDraft::default()
    }
}

/// A valid two-workpiece draft: `wp-1` at `member_revision()` and `wp-2` at a
/// distinct revision, each approval binding its own scope revision. The seal
/// gate re-forms both approvals from the request projections; the placeholder
/// details here only have to be reducer-admissible before the gate replaces them.
fn two_member_draft() -> BloomDraft {
    let detail = Digest::from_bytes([9; 32]);
    BloomDraft {
        proposals: vec![member("wp-1", member_revision(), detail), member("wp-2", Digest::from_bytes([8; 32]), detail)],
        base: Digest::from_bytes([1; 32]),
        stage_catalog: StageCatalog::line_digest(),
        ..BloomDraft::default()
    }
}

/// Assert the pre-seal approve gate (#3583) refuses `seal_path` at every
/// fail-closed branch: a missing projection, an incomplete one, and an
/// above-auto surface each return `422` before admit, leaving the draft
/// unchanged for the caller's subsequent successful seal.
fn assert_gate_fails_closed(http_port: u16, seal_path: &str) {
    // Missing projection: an empty seal body has no projection for the member —
    // never admitted on the operator's unchecked approval.
    let (status, _) = try_http(http_port, "POST", seal_path, None).unwrap();
    assert_eq!(status, 422, "no projection fails closed");

    // Incomplete projection: an otherwise-auto surface with a completeness check
    // flipped false is refused.
    let mut incomplete = complete();
    incomplete.has_problem_statement = false;
    let (status, _) =
        send_json(http_port, "POST", seal_path, &seal_body(vec![projection(&["docs/guide/x.md"], incomplete)]));
    assert_eq!(status, 422, "incomplete projection fails closed");

    // Above-auto surface with no signed statement: `crates/aether-data/**`
    // resolves `human`, and `projection` carries no `signed_statement`, so the
    // deferred-verify path (#3599) fails it closed before any signing dispatch.
    let above_auto = seal_body(vec![projection(&["crates/aether-data/src/lib.rs"], complete())]);
    let (status, _) = send_json(http_port, "POST", seal_path, &above_auto);
    assert_eq!(status, 422, "above-auto with no signed statement fails closed");
}

#[test]
fn rest_api_drives_a_bloom_end_to_end() {
    let http_port = free_port();
    let rpc_port = free_port();
    // The gate loads this policy at init; kept alive for the child's lifetime.
    let (_policy_dir, policy_path) = test_policy();
    let mut child = spawn(http_port, rpc_port, &policy_path);

    // Run the whole flow in a closure so a panic still reaps the child.
    let result = std::panic::catch_unwind(|| {
        // Routes register asynchronously; `/drafts` (an in-memory route) is the
        // http-readiness signal.
        wait_for_200(http_port, "/drafts");

        // In-memory shaping: stage a workpiece and read it back.
        let staged = Workpiece {
            id: WorkpieceId("wp-1".to_owned()),
            intent: Digest::from_bytes([2; 32]),
            scope_revision: Digest::from_bytes([3; 32]),
        };
        let (status, workpiece) = send_json(http_port, "POST", "/workpieces", &serde_json::to_value(&staged).unwrap());
        assert_eq!(status, 201, "stage workpiece");
        assert_eq!(workpiece["id"], "wp-1");
        let (_, listed) = get_json(http_port, "/workpieces");
        assert_eq!(listed["workpieces"].as_array().unwrap().len(), 1);

        // Open a draft, shape it into an admissible bloom, and read it back.
        let (status, opened) = send_json(http_port, "POST", "/drafts", &Value::Null);
        assert_eq!(status, 201, "open draft");
        let draft_id = opened["draft_id"].as_str().unwrap().to_owned();
        let patch = serde_json::to_value(valid_draft("wp-1")).unwrap();
        let (status, patched) = send_json(http_port, "PATCH", &format!("/drafts/{draft_id}"), &patch);
        assert_eq!(status, 200, "patch draft");
        assert_eq!(patched["draft"]["proposals"].as_array().unwrap().len(), 1);
        let (status, _) = get_json(http_port, &format!("/drafts/{draft_id}"));
        assert_eq!(status, 200, "read draft");

        // The control core answers queries once loaded + replayed — the write /
        // live-read readiness signal.
        wait_for_200(http_port, "/view");

        // The pre-seal approve gate (#3583) fails closed at every branch — a
        // missing projection, an incomplete one, and an above-auto surface each
        // refuse the seal (422) before admit, so the draft is unchanged.
        let seal_path = format!("/drafts/{draft_id}/seal");
        assert_gate_fails_closed(http_port, &seal_path);

        // Auto surface + complete projection: the gate forms the approval and the
        // seal admits. The outcome names the sealed bloom id (rendered as the
        // BloomId digest's serde byte array; hex-encode it for the `{id}` route).
        let (status, body) = try_http(
            http_port,
            "POST",
            &seal_path,
            Some(&serde_json::to_vec(&seal_body(vec![projection(&["docs/guide/x.md"], complete())])).unwrap()),
        )
        .unwrap();
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

        // The answer route (ADR-0151). A malformed statement body is a clean 400,
        // not a panic; a malformed bloom id is a 400.
        let (status, _) =
            try_http(http_port, "POST", &format!("/blooms/{bloom_id}/answer"), Some(b"not a statement")).unwrap();
        assert_eq!(status, 400, "malformed answer body");
        let (status, _) = try_http(http_port, "POST", "/blooms/xyz/answer", Some(b"{}")).unwrap();
        assert_eq!(status, 400, "malformed bloom id");

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
            parents: vec![Digest::from_bytes([222; 32])],
        };
        let (status, _) = send_json(
            http_port,
            "POST",
            &format!("/blooms/{bloom_id}/answer"),
            &serde_json::to_value(&unsigned).unwrap(),
        );
        assert_eq!(status, 400, "an answer with an unverifiable signature is rejected at the gate");

        // A genuine author signature by the allowlisted `owner` verifies and
        // admits; adopting no held question reduces to a clean rejection outcome
        // — the sealed bloom carries no parked hold — 200 carrying the reducer's
        // refusal, mirroring how seal rejections surface. This is the real
        // custody path: the answer verified against the host-local allowlist, not
        // the retired always-valid stub.
        let answer = Statement {
            words: words.clone(),
            provenance: Provenance::AuthorSignature(SignatureEnvelope {
                signer: KeyId("owner".to_owned()),
                signature: owner_signing_key().sign(&words).to_bytes().to_vec(),
            }),
            parents: vec![Digest::from_bytes([222; 32])],
        };
        let (status, answered) = send_json(
            http_port,
            "POST",
            &format!("/blooms/{bloom_id}/answer"),
            &serde_json::to_value(&answer).unwrap(),
        );
        assert_eq!(status, 200, "answer admitted: {answered:?}");
        assert_eq!(answered["outcome"]["AdoptAnswerRejected"], "NoMatchingHold", "no parked hold to adopt");
    });

    let _ = child.kill();
    let _ = child.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
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
    run_with_bloomery("mixed_auto_and_above_auto_seal_admits_when_verified", |http_port| {
        assert_mixed_above_auto_seal(http_port, member_revision());
    });
}

/// Boot the `bloomery` bin, wait for both readiness signals (`/drafts` for the
/// HTTP router, `/view` for the live control core), run `body` against its HTTP
/// port inside a panic guard, then reap the child.
fn run_with_bloomery(_label: &str, body: impl FnOnce(u16) + std::panic::UnwindSafe) {
    let http_port = free_port();
    let rpc_port = free_port();
    let (_policy_dir, policy_path) = test_policy();
    let mut child = spawn(http_port, rpc_port, &policy_path);

    let result = std::panic::catch_unwind(|| {
        wait_for_200(http_port, "/drafts");
        wait_for_200(http_port, "/view");
        body(http_port);
    });

    let _ = child.kill();
    let _ = child.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// Cases (a/b/c) of the deferred-verify seal on a single above-auto member
/// (`wp-1` over `crates/aether-data`, which the test policy resolves `human`): a
/// missing statement, a wrong-subject statement (synchronous pre-check), and an
/// unverifiable signature (after the `aether.signing` round trip) each fail
/// closed (422); a valid owner-signed statement admits with a gate-formed
/// approval the reducer accepts.
fn assert_single_above_auto_seal(http_port: u16) {
    let (_, opened) = send_json(http_port, "POST", "/drafts", &Value::Null);
    let draft_id = opened["draft_id"].as_str().unwrap().to_owned();
    let (status, _) = send_json(
        http_port,
        "PATCH",
        &format!("/drafts/{draft_id}"),
        &serde_json::to_value(valid_draft("wp-1")).unwrap(),
    );
    assert_eq!(status, 200, "patch above-auto draft");
    let seal_path = format!("/drafts/{draft_id}/seal");
    let surface = ["crates/aether-data/src/lib.rs"];
    let revision = member_revision();
    let subject = member("wp-1", revision, Digest::from_bytes([9; 32])).subject();
    let seal = |statement| seal_body(vec![projection_at("wp-1", revision, &surface, statement)]);

    // (b) No signed statement → fail closed before any signing dispatch.
    let (status, _) = send_json(http_port, "POST", &seal_path, &seal(None));
    assert_eq!(status, 422, "above-auto with no signed statement fails closed");

    // (c-subject) Signed over another revision → rejected at the synchronous
    // pre-check, before any signing dispatch.
    let wrong_subject = owner_signed_statement(Digest::from_bytes([1; 32]));
    let (status, _) = send_json(http_port, "POST", &seal_path, &seal(Some(wrong_subject)));
    assert_eq!(status, 422, "a wrong-subject statement fails closed at the pre-check");

    // (c-signature) Correct subject + author signature that does NOT verify (an
    // empty signature) → passes the pre-check, dispatched to `aether.signing`,
    // refused there, failing the seal closed after the round trip.
    let mis_signed = Statement {
        words: subject.as_bytes().to_vec(),
        provenance: Provenance::AuthorSignature(SignatureEnvelope {
            signer: KeyId("owner".to_owned()),
            signature: vec![],
        }),
        parents: vec![],
    };
    let (status, _) = send_json(http_port, "POST", &seal_path, &seal(Some(mis_signed)));
    assert_eq!(status, 422, "a mis-signed statement fails closed after the signing round trip");

    // (a) A valid owner-signed statement verifies and the seal admits with a
    // gate-formed approval the reducer accepts.
    let body = serde_json::to_vec(&seal(Some(owner_signed_statement(subject)))).unwrap();
    let (status, body) = try_http(http_port, "POST", &seal_path, Some(&body)).unwrap();
    let sealed: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, 200, "above-auto seal admits: {sealed:?}");
    let bloom_id = bloom_hex(&sealed["outcome"]["Sealed"]);
    assert_eq!(bloom_id.len(), 64, "sealed bloom id is a 32-byte digest");

    // The sealed above-auto member is live, its approval a gate-formed `Approval`
    // bound to the revision (not the operator's placeholder).
    let (status, view) = get_json(http_port, &format!("/blooms/{bloom_id}"));
    assert_eq!(status, 200, "the sealed above-auto bloom is live");
    let members = view["members"].as_array().unwrap();
    assert_eq!(members.len(), 1, "one sealed member");
    assert_eq!(members[0]["workpiece"], "wp-1");
    assert_eq!(members[0]["approval"]["kind"], "Approval", "the above-auto member carries a gate-formed approval");
}

/// Case (d) of the deferred-verify seal: a mixed draft with an auto member
/// (`wp-1`, `docs/guide`) and an above-auto member (`wp-2`, `crates/aether-data`)
/// fails closed when the above-auto member is unsigned, and admits both members
/// once its signature verifies.
fn assert_mixed_above_auto_seal(http_port: u16, wp1_revision: Digest) {
    let (_, opened) = send_json(http_port, "POST", "/drafts", &Value::Null);
    let mixed_id = opened["draft_id"].as_str().unwrap().to_owned();
    let (status, _) = send_json(
        http_port,
        "PATCH",
        &format!("/drafts/{mixed_id}"),
        &serde_json::to_value(two_member_draft()).unwrap(),
    );
    assert_eq!(status, 200, "patch mixed draft");
    let mixed_seal = format!("/drafts/{mixed_id}/seal");
    let wp2_revision = Digest::from_bytes([8; 32]);
    let wp1_auto = projection_at("wp-1", wp1_revision, &["docs/guide/x.md"], None);
    let wp2_above = |statement| projection_at("wp-2", wp2_revision, &["crates/aether-data/src/lib.rs"], statement);

    // wp-2's statement omitted → the whole mixed seal fails closed even though
    // wp-1 resolves auto.
    let (status, _) = send_json(http_port, "POST", &mixed_seal, &seal_body(vec![wp1_auto.clone(), wp2_above(None)]));
    assert_eq!(status, 422, "a mixed draft with an unsigned above-auto member fails closed");

    // wp-2 validly signed → the mixed seal admits once the verification passes.
    let (status, body) = try_http(
        http_port,
        "POST",
        &mixed_seal,
        Some(
            &serde_json::to_vec(&seal_body(vec![
                wp1_auto,
                wp2_above(Some(owner_signed_statement(
                    member("wp-2", wp2_revision, Digest::from_bytes([9; 32])).subject(),
                ))),
            ]))
            .unwrap(),
        ),
    )
    .unwrap();
    let sealed: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, 200, "mixed auto + above-auto seal admits when the above-auto member verifies: {sealed:?}");
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
    let http_port = free_port();
    let rpc_port = free_port();
    let (_policy_dir, policy_path) = test_policy();
    let mut child = spawn(http_port, rpc_port, &policy_path);

    let result = std::panic::catch_unwind(|| {
        wait_for_200(http_port, "/drafts");

        let request = |value: Value| serde_json::json!({ "kind": "aether.store.drain_outbox", "value": value });

        let (status, authored) =
            send_json(http_port, "POST", "/configs", &request(serde_json::json!({ "topic": "construct" })));
        assert_eq!(status, 200, "authoring answers only once the store write lands");
        assert!(
            authored["digest"].as_array().is_some_and(|bytes| bytes.len() == 32),
            "the reply names a 32-byte address"
        );
        assert_eq!(authored["kind"], "aether.store.drain_outbox", "the reply names the key a registry seals under");
        let digest = authored["digest"].clone();

        // Idempotent by content addressing: re-authoring rewrites the same row
        // rather than creating a second address a seal would have to choose
        // between.
        let (status, again) =
            send_json(http_port, "POST", "/configs", &request(serde_json::json!({ "topic": "construct" })));
        assert_eq!(status, 200);
        assert_eq!(again["digest"], digest, "the same content under the same kind keeps its address");

        let (status, other) =
            send_json(http_port, "POST", "/configs", &request(serde_json::json!({ "topic": "refine" })));
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
    });

    let _ = child.kill();
    let _ = child.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
