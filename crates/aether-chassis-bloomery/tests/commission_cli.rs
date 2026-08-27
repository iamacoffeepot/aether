//! Commission CLI against a running coordinator (#5047): create, scope,
//! approve, show, list over the control API, a malformed envelope refused
//! with an operator-readable reason, and a bare `bloomery` still the daemon.

mod common;

use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use aether_bloomery::{AuthorityDoor, Digest, KeyId, SignatureEnvelope, authorization_message};
use aether_chassis_bloomery::commission;
use common::client::spawn_and_connect;
use common::{Coordinator, Ingress};
use ed25519_dalek::{Signer, SigningKey};
use tempfile::TempDir;

const TOKEN: &str = "secret";

fn owner_key() -> SigningKey {
    SigningKey::from_bytes(&[42u8; 32])
}

fn hex_bytes(bytes: &[u8]) -> String {
    aether_bloomery::encode_hex(bytes)
}

fn owner_allowlist() -> String {
    format!("owner:{}", hex_bytes(&owner_key().verifying_key().to_bytes()))
}

fn write_file(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
}

fn temp_dir() -> TempDir {
    tempfile::tempdir().unwrap_or_else(|error| panic!("temp dir: {error}"))
}

fn utf8_path(path: &Path) -> &str {
    path.to_str().unwrap_or_else(|| panic!("{} is UTF-8", path.display()))
}

fn spawn(policy_path: &str, allowlist: &str) -> Coordinator {
    Coordinator::spawn(
        0,
        &[
            ("AETHER_STORE_PATH", ":memory:"),
            ("AETHER_HTTP_CONTROL_TOKEN", TOKEN),
            ("AETHER_SIGNING_ALLOWLIST", allowlist),
            ("AETHER_APPROVAL_POLICY_FILE", policy_path),
        ],
    )
}

fn cli(http_port: u16, rest: &[&str]) -> Result<String, anyhow::Error> {
    let port = http_port.to_string();
    let mut invocation =
        vec!["bloomery-commission".to_owned(), "--http-port".to_owned(), port, "--token".to_owned(), TOKEN.to_owned()];
    invocation.extend(rest.iter().map(|word| (*word).to_owned()));
    commission::run(invocation)
}

/// Fork with OS-assigned RPC and HTTP, handshake the child we spawned, then
/// wait until that child's HTTP answers `list`. A reserved port here burns the
/// ready deadline against a closed socket once a sibling steals the bind — the
/// full-suite flake at this panic — so both ports are the child's own and it
/// says which they are.
fn spawn_ready(policy_path: &str) -> (u16, Coordinator) {
    let allowlist = owner_allowlist();
    let (coordinator, _stream) =
        spawn_and_connect("commission-cli", Duration::from_mins(1), || spawn(policy_path, &allowlist));
    (wait_http(&coordinator), coordinator)
}

/// The child's REST port, once it answers `list`.
///
/// Two gates, and both are needed: the child announces the port it bound, and
/// the CLI's own `list` is what says the routes behind it have registered.
fn wait_http(coordinator: &Coordinator) -> u16 {
    let deadline = Instant::now() + Duration::from_secs(30);
    let port = coordinator
        .await_port(Ingress::Http, deadline)
        .unwrap_or_else(|why| panic!("the coordinator never bound its REST control API: {why}"));

    // Assigned on every path that reaches the assert, so it always names the
    // refusal the last attempt actually met.
    let mut last;
    loop {
        match cli(port, &["list"]) {
            Ok(_) => return port,
            Err(error) => last = error.to_string(),
        }
        assert!(Instant::now() < deadline, "coordinator HTTP never became ready for commission list: {last}");
        thread::sleep(Duration::from_millis(50));
    }
}

fn envelope_for(door: AuthorityDoor, digest_hex: &str) -> Vec<u8> {
    let Some(bytes) = hex_to_32(digest_hex) else {
        panic!("test digest must be 32 bytes hex, got {digest_hex}");
    };
    let digest = Digest::from_bytes(bytes);
    let message = authorization_message(door, digest, digest.as_bytes());
    let envelope = SignatureEnvelope {
        signer: KeyId("owner".to_owned()),
        signature: owner_key().sign(message.as_bytes()).to_bytes().to_vec(),
    };
    serde_json::to_vec(&envelope).unwrap_or_else(|error| panic!("encode envelope: {error}"))
}

fn hex_to_32(hex: &str) -> Option<[u8; 32]> {
    Digest::from_hex(hex).map(|digest| *digest.as_bytes())
}

fn first_token(output: &str) -> &str {
    output.split_whitespace().next().unwrap_or_else(|| panic!("expected output, got {output:?}"))
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
    let dir = temp_dir();
    let policy = dir.path().join("policy.toml");
    write_file(&policy, b"default = \"judge\"\n");
    let (_http_port, mut coordinator) = spawn_ready(utf8_path(&policy));
    assert!(coordinator.is_alive(), "a bare bloomery must still be the daemon process");
}

#[test]
fn create_scope_approve_show_and_list_round_trip() {
    let dir = temp_dir();
    let policy = dir.path().join("policy.toml");
    write_file(&policy, b"default = \"judge\"\n");
    let (http_port, _coordinator) = spawn_ready(utf8_path(&policy));

    let intent = dir.path().join("intent.txt");
    write_file(&intent, b"ship the commission CLI");
    let created = cli(http_port, &["create", "--id", "issue-5047", "--intent-file", utf8_path(&intent)])
        .unwrap_or_else(|error| panic!("create: {error}"));
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
    let scope_out = cli(http_port, &["scope", "issue-5047", "--file", utf8_path(&scope)])
        .unwrap_or_else(|error| panic!("scope: {error}"));
    let digest = first_token(&scope_out).to_owned();
    assert_eq!(digest.len(), 64, "scope prints a hex digest: {scope_out}");

    let envelope = dir.path().join("approve.json");
    write_file(&envelope, &envelope_for(AuthorityDoor::Approve, &digest));
    cli(http_port, &["approve", "issue-5047", "--scope", &digest, "--envelope", utf8_path(&envelope)])
        .unwrap_or_else(|error| panic!("approve: {error}"));

    let shown = cli(http_port, &["show", "issue-5047"]).unwrap_or_else(|error| panic!("show: {error}"));
    assert!(shown.contains("issue-5047"), "show names the workpiece: {shown}");
    assert!(shown.contains("open"), "show reports open: {shown}");
    assert!(shown.contains(&digest), "show reports the current revision: {shown}");

    let listed = cli(http_port, &["list", "--status", "open"]).unwrap_or_else(|error| panic!("list: {error}"));
    assert!(listed.contains("issue-5047"), "list includes the commission: {listed}");

    let garbage = dir.path().join("garbage.json");
    write_file(&garbage, b"{not-an-envelope");
    match cli(http_port, &["approve", "issue-5047", "--scope", &digest, "--envelope", utf8_path(&garbage)]) {
        Ok(output) => panic!("malformed envelope must be refused, got {output}"),
        Err(error) => {
            let message = error.to_string();
            assert!(message.contains("SignatureEnvelope"), "malformed envelope must name the refusal, got {message}");
        }
    }

    let wrong = dir.path().join("wrong.json");
    write_file(&wrong, &envelope_for(AuthorityDoor::Approve, &"aa".repeat(32)));
    match cli(http_port, &["approve", "issue-5047", "--scope", &digest, "--envelope", utf8_path(&wrong)]) {
        Ok(output) => panic!("mismatched envelope must be refused, got {output}"),
        Err(error) => {
            let message = error.to_string();
            assert!(message.contains("400:"), "a mismatched envelope is a 400, not a transport error: {message}");
        }
    }
}

#[test]
fn approve_refuses_an_uncommissioned_dependency() {
    let dir = temp_dir();
    let policy = dir.path().join("policy.toml");
    write_file(&policy, b"default = \"judge\"\n");
    let (http_port, _coordinator) = spawn_ready(utf8_path(&policy));

    let intent = dir.path().join("intent.txt");
    write_file(&intent, b"ship a scoped workpiece");
    cli(http_port, &["create", "--id", "issue-5286", "--intent-file", utf8_path(&intent)])
        .unwrap_or_else(|error| panic!("create: {error}"));

    let scope = dir.path().join("scope.md");
    write_file(
        &scope,
        b"\
## Problem statement\n\
\n\
Need a dependency check.\n\
\n\
## Design notes\n\
\n\
Refuse at approve.\n\
\n\
## Implementation plan\n\
\n\
Gate the live approve door.\n\
\n\
**Size:** m\n\
**Implementation model:** grok-4.6\n\
**Routing reason:** focused store check\n\
\n\
## Depends on\n\
\n\
- issue-ghost\n\
\n\
## Declared surface\n\
\n\
```text\n\
crates/aether-chassis-bloomery/src/store/commission/**\n\
```\n",
    );
    let scope_out = cli(http_port, &["scope", "issue-5286", "--file", utf8_path(&scope)])
        .unwrap_or_else(|error| panic!("scope: {error}"));
    let digest = first_token(&scope_out).to_owned();
    assert_eq!(digest.len(), 64, "scope prints a hex digest: {scope_out}");

    let envelope = dir.path().join("approve.json");
    write_file(&envelope, &envelope_for(AuthorityDoor::Approve, &digest));
    match cli(http_port, &["approve", "issue-5286", "--scope", &digest, "--envelope", utf8_path(&envelope)]) {
        Ok(output) => panic!("uncommissioned dependency must be refused, got {output}"),
        Err(error) => {
            let message = error.to_string();
            assert!(
                message.contains("400:"),
                "an uncommissioned dependency is a 400, not a transport error: {message}"
            );
            assert!(message.contains("issue-ghost"), "refusal names the missing workpiece: {message}");
        }
    }
}
