//! The REST control API end-to-end (ADR-0149 §Packaging, issue #3498): boot the
//! `bloomery` bin with the HTTP ingress + control core autoloaded, and drive a
//! bloom lifecycle over raw HTTP the way an operator's `curl` would — stage
//! workpieces, shape and seal a draft, read the sealed bloom / view document /
//! journal, and 404 a missing artifact. No typed-mail RPC vocabulary is used;
//! every request is plain HTTP against the `aether.bloomery.api` router.
//!
//! Like `control_loop.rs`, the write / live-read half needs the control-core
//! wasm pre-built (CI's "Pre-build component wasm" step); a dev box that hasn't
//! built it skips cleanly unless `AETHER_REQUIRE_RUNTIME` is set.

#![allow(clippy::unwrap_used)]
// The wasm-availability skip prints a diagnostic and reads `AETHER_REQUIRE_RUNTIME`
// — test-harness controls, not cap config (the `control_loop.rs` precedent).
#![allow(clippy::print_stderr)]
#![allow(clippy::disallowed_methods)]
// Test-harness ergonomics: fully-qualified std paths in a one-off client, a
// request head assembled with `format!`, and an explicit skip-panic.
#![allow(clippy::absolute_paths)]
#![allow(clippy::format_push_string)]
#![allow(clippy::format_collect)]
#![allow(clippy::manual_assert)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use std::{env, thread};

use aether_bloomery::{
    BloomDraft, Digest, Evidence, EvidenceKind, KeyId, Membership, Provenance, SignatureEnvelope, StageCatalog,
    Statement, Workpiece, WorkpieceId,
};
use aether_bloomery_host::api::{MemberProjection, SealRequest};
use aether_bloomery_host::bloomery::{AdrTouch, Completeness};
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

/// A `SealRequest` carrying `projections`, serialized to a JSON value for the
/// seal route.
fn seal_body(projections: Vec<MemberProjection>) -> Value {
    serde_json::to_value(SealRequest { idempotency_key: None, projections }).unwrap()
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

/// Locate the control-core wasm artifact, preferring `release` over `debug`.
fn control_core_wasm() -> Option<PathBuf> {
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
/// claim (a small race the connect-retry loop tolerates).
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// Fork the `bloomery` bin with the HTTP ingress and control core autoloaded,
/// pointing the pre-seal approve gate at `policy_path` (#3583).
fn spawn(http_port: u16, rpc_port: u16, wasm: &PathBuf, policy_path: &str) -> Child {
    Command::new(env!("CARGO_BIN_EXE_bloomery"))
        .env("AETHER_HTTP_PORT", http_port.to_string())
        .env("AETHER_RPC_PORT", rpc_port.to_string())
        .env("AETHER_STORE_PATH", ":memory:")
        .env("AETHER_SIGNING_ALLOWLIST", owner_allowlist())
        .env("AETHER_APPROVAL_POLICY_FILE", policy_path)
        .env("AETHER_CONTROL_CORE_WASM", wasm)
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
    let member = Membership {
        workpiece: WorkpieceId(workpiece.to_owned()),
        scope_revision,
        approval: Evidence {
            subject: scope_revision,
            kind: EvidenceKind::Approval,
            detail: Digest::from_bytes([9; 32]),
        },
    };
    BloomDraft {
        proposals: vec![member],
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

    // Above-auto surface: `crates/aether-data/**` resolves `human`, which this
    // slice fails closed on (signed-statement enforcement is the follow-up child).
    let above_auto = seal_body(vec![projection(&["crates/aether-data/src/lib.rs"], complete())]);
    let (status, _) = send_json(http_port, "POST", seal_path, &above_auto);
    assert_eq!(status, 422, "above-auto fails closed");
}

#[test]
fn rest_api_drives_a_bloom_end_to_end() {
    let Some(wasm) = control_core_wasm() else {
        if env::var("AETHER_REQUIRE_RUNTIME").is_ok() {
            panic!("control-core wasm not built but AETHER_REQUIRE_RUNTIME is set");
        }
        eprintln!("skipping rest_api_drives_a_bloom_end_to_end: control-core wasm not built");
        return;
    };

    let http_port = free_port();
    let rpc_port = free_port();
    // The gate loads this policy at init; kept alive for the child's lifetime.
    let (_policy_dir, policy_path) = test_policy();
    let mut child = spawn(http_port, rpc_port, &wasm, &policy_path);

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
