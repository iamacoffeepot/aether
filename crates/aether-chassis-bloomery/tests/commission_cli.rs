//! Commission CLI against a running coordinator (#5047): create, scope,
//! approve, show, list over the control API, a malformed envelope refused
//! with an operator-readable reason, and a bare `bloomery` still the daemon.

mod common;

use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use aether_bloomery::{AuthorityDoor, KeyId, SignatureEnvelope, authorization_message};
use aether_chassis_bloomery::commission;
use common::{Coordinator, free_port};
use ed25519_dalek::{Signer, SigningKey};

const TOKEN: &str = "secret";

fn owner_key() -> SigningKey {
    SigningKey::from_bytes(&[42u8; 32])
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    out
}

fn owner_allowlist() -> String {
    format!("owner:{}", hex_bytes(&owner_key().verifying_key().to_bytes()))
}

fn write_file(path: &Path, bytes: &[u8]) {
    if let Err(error) = fs::write(path, bytes) {
        panic!("write {}: {error}", path.display());
    }
}

fn spawn(http_port: u16, policy_path: &str) -> Coordinator {
    let http = http_port.to_string();
    Coordinator::spawn(
        free_port(),
        &[
            ("AETHER_HTTP_PORT", &http),
            ("AETHER_STORE_PATH", ":memory:"),
            ("AETHER_HTTP_CONTROL_TOKEN", TOKEN),
            ("AETHER_SIGNING_ALLOWLIST", &owner_allowlist()),
            ("AETHER_APPROVAL_POLICY_FILE", policy_path),
        ],
    )
}

fn cli(port: u16, args: &[&str]) -> Result<String, anyhow::Error> {
    let port = port.to_string();
    let mut argv =
        vec!["bloomery-commission".to_owned(), "--http-port".to_owned(), port, "--token".to_owned(), TOKEN.to_owned()];
    argv.extend(args.iter().map(|arg| (*arg).to_owned()));
    commission::run(argv)
}

fn wait_ready(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match cli(port, &["list"]) {
            Ok(_) => return,
            Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(100)),
            Err(error) => panic!("coordinator never became ready for commission list: {error}"),
        }
    }
}

fn envelope_for(door: AuthorityDoor, digest_hex: &str) -> Vec<u8> {
    let bytes = match hex_to_32(digest_hex) {
        Some(bytes) => bytes,
        None => panic!("test digest must be 32 bytes hex, got {digest_hex}"),
    };
    let digest = aether_bloomery::Digest::from_bytes(bytes);
    let message = authorization_message(door, digest, digest.as_bytes());
    let envelope = SignatureEnvelope {
        signer: KeyId("owner".to_owned()),
        signature: owner_key().sign(message.as_bytes()).to_bytes().to_vec(),
    };
    match serde_json::to_vec(&envelope) {
        Ok(bytes) => bytes,
        Err(error) => panic!("encode envelope: {error}"),
    }
}

fn hex_to_32(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (index, slot) in bytes.iter_mut().enumerate() {
        let high = u8::from_str_radix(&hex[(index * 2)..=(index * 2)], 16).ok()?;
        let low = u8::from_str_radix(&hex[index * 2 + 1..index * 2 + 2], 16).ok()?;
        *slot = (high << 4) | low;
    }
    Some(bytes)
}

fn first_token(output: &str) -> &str {
    match output.split_whitespace().next() {
        Some(token) => token,
        None => panic!("expected output, got {output:?}"),
    }
}

#[test]
fn the_commission_cli_is_a_sibling_binary() {
    assert!(
        Path::new(env!("CARGO_BIN_EXE_bloomery-commission")).is_file(),
        "bloomery-commission must be a cargo binary"
    );
}

#[test]
fn a_bare_bloomery_invocation_still_starts_the_daemon() {
    let http_port = free_port();
    let http = http_port.to_string();
    let dir = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(error) => panic!("temp dir: {error}"),
    };
    let policy = dir.path().join("policy.toml");
    write_file(&policy, b"default = \"judge\"\n");
    let policy_path = match policy.to_str() {
        Some(path) => path,
        None => panic!("policy path is UTF-8"),
    };
    let mut coordinator = Coordinator::spawn(
        free_port(),
        &[
            ("AETHER_HTTP_PORT", &http),
            ("AETHER_STORE_PATH", ":memory:"),
            ("AETHER_HTTP_CONTROL_TOKEN", TOKEN),
            ("AETHER_APPROVAL_POLICY_FILE", policy_path),
        ],
    );
    wait_ready(http_port);
    assert!(coordinator.is_alive(), "a bare bloomery must still be the daemon process");
}

#[test]
fn create_scope_approve_show_and_list_round_trip() {
    let http_port = free_port();
    let dir = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(error) => panic!("temp dir: {error}"),
    };
    let policy = dir.path().join("policy.toml");
    write_file(&policy, b"default = \"judge\"\n");
    let policy_path = match policy.to_str() {
        Some(path) => path,
        None => panic!("policy path is UTF-8"),
    };
    let _coordinator = spawn(http_port, policy_path);
    wait_ready(http_port);

    let intent = dir.path().join("intent.txt");
    write_file(&intent, b"ship the commission CLI");
    let intent_path = match intent.to_str() {
        Some(path) => path,
        None => panic!("intent path is UTF-8"),
    };
    let created = match cli(http_port, &["create", "--id", "issue-5047", "--intent-file", intent_path]) {
        Ok(output) => output,
        Err(error) => panic!("create: {error}"),
    };
    assert!(created.contains("issue-5047"), "create names the workpiece: {created}");

    let scope = dir.path().join("scope.md");
    write_file(
        &scope,
        b"\
## Problem statement\n\
\n\
Need a CLI.\n\
\n\
## Design notes\n\
\n\
Separate binary.\n\
\n\
## Implementation plan\n\
\n\
Ship bloomery-commission.\n\
\n\
**Size:** m\n\
**Implementation model:** sonnet\n\
**Routing reason:** focused CLI\n\
\n\
## Declared surface\n\
\n\
```text\n\
crates/aether-chassis-bloomery/src/commission/**\n\
```\n\
\n\
## Dogfood brief\n\
\n\
Create then show.\n",
    );
    let scope_path = match scope.to_str() {
        Some(path) => path,
        None => panic!("scope path is UTF-8"),
    };
    let scope_out = match cli(http_port, &["scope", "issue-5047", "--file", scope_path]) {
        Ok(output) => output,
        Err(error) => panic!("scope: {error}"),
    };
    let digest = first_token(&scope_out).to_owned();
    assert_eq!(digest.len(), 64, "scope prints a hex digest: {scope_out}");

    let envelope = dir.path().join("approve.json");
    write_file(&envelope, &envelope_for(AuthorityDoor::Approve, &digest));
    let envelope_path = match envelope.to_str() {
        Some(path) => path,
        None => panic!("envelope path is UTF-8"),
    };
    if let Err(error) = cli(http_port, &["approve", "issue-5047", "--scope", &digest, "--envelope", envelope_path]) {
        panic!("approve: {error}");
    }

    let shown = match cli(http_port, &["show", "issue-5047"]) {
        Ok(output) => output,
        Err(error) => panic!("show: {error}"),
    };
    assert!(shown.contains("issue-5047"), "show names the workpiece: {shown}");
    assert!(shown.contains("open"), "show reports open: {shown}");
    assert!(shown.contains(&digest), "show reports the current revision: {shown}");

    let listed = match cli(http_port, &["list", "--status", "open"]) {
        Ok(output) => output,
        Err(error) => panic!("list: {error}"),
    };
    assert!(listed.contains("issue-5047"), "list includes the commission: {listed}");

    let garbage = dir.path().join("garbage.json");
    write_file(&garbage, b"{not-an-envelope");
    let garbage_path = match garbage.to_str() {
        Some(path) => path,
        None => panic!("garbage path is UTF-8"),
    };
    match cli(http_port, &["approve", "issue-5047", "--scope", &digest, "--envelope", garbage_path]) {
        Ok(output) => panic!("malformed envelope must be refused, got {output}"),
        Err(error) => {
            let message = error.to_string();
            assert!(message.contains("SignatureEnvelope"), "malformed envelope must name the refusal, got {message}");
        }
    }

    let wrong = dir.path().join("wrong.json");
    write_file(&wrong, &envelope_for(AuthorityDoor::Approve, &"aa".repeat(32)));
    let wrong_path = match wrong.to_str() {
        Some(path) => path,
        None => panic!("wrong envelope path is UTF-8"),
    };
    match cli(http_port, &["approve", "issue-5047", "--scope", &digest, "--envelope", wrong_path]) {
        Ok(output) => panic!("mismatched envelope must be refused, got {output}"),
        Err(error) => {
            let message = error.to_string();
            assert!(message.contains("400:"), "a mismatched envelope is a 400, not a transport error: {message}");
        }
    }
}
