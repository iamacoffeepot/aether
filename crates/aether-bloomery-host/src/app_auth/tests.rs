//! Unit tests for the `app_auth` custody (ADR-0149 §Migration step 3).
//!
//! The JWT minting and the cache/refresh logic are this crate's own — a fixture
//! RSA keypair signs a real JWT (verified with the public key), and a counting
//! fake exchange stands in for the network so the refresh-before-expiry cadence
//! is asserted with no live GitHub. Tripwire: mint-once-then-cache vs
//! re-mint-when-stale is the behavior the design turns on, not a passthrough.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use aether_bloomery_github::{GithubError, InstallationToken, TokenSource};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Validation, decode};
use serde::Deserialize;

use super::minter::{AppTokenSource, InstallationTokenExchange};
use crate::bloomery::GithubMirrorConfig;

// A throwaway 2048-bit RSA keypair (never a real credential) — the fixture the
// JWT-signing test signs with and verifies against.
const TEST_PRIVATE_KEY: &str = "-----BEGIN RSA PRIVATE KEY-----
MIIEpQIBAAKCAQEAv2JzVEAUCyxtdyoUeFfFzydL9W9BOwO5W1fKkGhQ9dfgid5c
1dJwUR/jWb5KHXZ2cAvZ5j6wK8PaKG5WSxtSqO5ingxHJA7SzxX6kQseXLUHamAv
OZ6i3iiNDY3xuO9MdEd8BVT0iNpSm3eQN10+Ug1kXfw9rnIqNp7g/xwSYllKG9o8
rISa26Huo/WQ+PB/aKRHgVQ3Un8ajKnIc0UBTUa95t1kg1a1S5w2meBLb/sh2y3n
Wy1V4yMjVO70x2sWgoVTn2s7PAyzTpc0CmQ78/5Y/XxEZlK27VFOP3W4xdlPWC0S
pggjNDzSt+Q7bur8qzf20HeZOpeTyVCN3croVwIDAQABAoIBAD89sQ5t/jGTBLkT
1p/NoTfKrHb1xIBTwrREVlNRpS8XnsLwD404dJTaDK5jCuqhcpGj2OUUYfKUTUp+
61T2OmJII55GQFvR6ic0BBBZtDa+Oy0Ti4dmvDrc+383IGET8heaZ4j7gbKXMiTd
ZXJmBWnnsvq7l0ZFw105MvAZvplwhHczkq+CjfwypM0VRV9XKxUfuQdiFG377jjZ
LARbtN7kWxtm2iwL2ZtfKQPbYjxCUrYWVN1q9e3kcL+bITf3GwK34k7umuYy1+rw
zanpQ0L+F5sU6XCu+3G3XNhM76kKXoi9SCwlhLp4r6T+Wkk6yvBl2nBRtB90qeHE
ONM1FYECgYEA+sieSVEvvRPc7oH4na/P6JVKdoxCtf4oHHHm5uAltmjF30169R2V
a2mdQ8pPlLx2WeN9tzM/dEuDCR1f0Gb/04IZBJBlirj1G1yG85GxQqB34C246MlP
+nlc/wCgZi8I79RZ7Hp8OSY86M+h0gZWc6njWzEhyQ+BO4h9KB21cU8CgYEAw12K
tG53ml3wTTXSvkWrS07rkePR6MmyTBFn8TU3xYTI0Do5a6zM2pViqub3cFh9IhZp
Odox7onKS+mut/aXfDHZScba0+s9cZOUc/rW8Z2m4Os6vvCa8cguxUW3P/uZ5/Ey
XrSzJnwb+dKINnC/a6ag/JPwGSmQm4LDYnJ3hnkCgYEA6HZAazvLYZvg5mEp4JlQ
wopoTL01NVfTPJLEc2yA6LXz/Urn2ABFOhzbPzRwUjHkDuyV4tSpVBaO70sAPsDL
EPb+U8G5rj5GTceV/H8nbdgrZm1bgsTg0w/eiS2+gRnGUfFoLZFYRu1P9opIuNNR
HcPz0NsZMzOhGlspkJ8BSncCgYEAv8nHzgOYNJm9uv54qcPZOi/6wJjHS+EdwOFh
igD1hFkrjodqMVNNM9RtLVtaVBb6mQkpOdsDI6pvRwDcPcq9wfVp26x0zI/mHOaF
WSpJ8p4S4kDqxeGMKombqJwdHpnP6Ev3Z9O6/6/dAu50PAWJVZQZ/Hr6vKj6RkAj
sTSwM/kCgYEA+J08Bt+2+HDSw8Grsc3WOiPJTuIMaX3uhEjxwlozq36GPah6T8+d
q9nQWTzvE1G118enh8FoJE0/v3x+IGXpLXoseASCSkOuJvIZB4LIuz/sndc6QcDX
xAtw6HCuoUIzjbWZe1H+wS8KmJmYkTvf8f70x0/jMYRUyvMQy3beUUQ=
-----END RSA PRIVATE KEY-----";

const TEST_PUBLIC_KEY: &str = "-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAv2JzVEAUCyxtdyoUeFfF
zydL9W9BOwO5W1fKkGhQ9dfgid5c1dJwUR/jWb5KHXZ2cAvZ5j6wK8PaKG5WSxtS
qO5ingxHJA7SzxX6kQseXLUHamAvOZ6i3iiNDY3xuO9MdEd8BVT0iNpSm3eQN10+
Ug1kXfw9rnIqNp7g/xwSYllKG9o8rISa26Huo/WQ+PB/aKRHgVQ3Un8ajKnIc0UB
TUa95t1kg1a1S5w2meBLb/sh2y3nWy1V4yMjVO70x2sWgoVTn2s7PAyzTpc0CmQ7
8/5Y/XxEZlK27VFOP3W4xdlPWC0SpggjNDzSt+Q7bur8qzf20HeZOpeTyVCN3cro
VwIDAQAB
-----END PUBLIC KEY-----";

// A counting fake exchange: records how many mints it served and hands back a
// distinct token each time, so a re-mint is observable by the token changing.
struct CountingExchange {
    calls: AtomicUsize,
    expires_at: String,
}

impl CountingExchange {
    fn new(expires_at: &str) -> Self {
        Self { calls: AtomicUsize::new(0), expires_at: expires_at.to_owned() }
    }
}

impl InstallationTokenExchange for CountingExchange {
    fn exchange(&self, _jwt: &str, _installation_id: u64) -> Result<InstallationToken, GithubError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(InstallationToken { token: format!("ghs_minted_{n}"), expires_at: self.expires_at.clone() })
    }
}

#[derive(Deserialize)]
struct TestClaims {
    iat: u64,
    exp: u64,
    iss: String,
}

fn test_key() -> EncodingKey {
    EncodingKey::from_rsa_pem(TEST_PRIVATE_KEY.as_bytes()).expect("fixture RSA key parses")
}

fn configured(app_id: u64, key_path: &str, installation_id: u64) -> GithubMirrorConfig {
    GithubMirrorConfig {
        app_id,
        app_private_key_path: key_path.to_owned(),
        app_installation_id: installation_id,
        ..GithubMirrorConfig::default()
    }
}

#[test]
fn app_auth_configured_requires_all_three_knobs() {
    // The default (empty) config is the static-PAT path — App-auth off.
    assert!(!GithubMirrorConfig::default().app_auth_configured());
    // All three present → on.
    assert!(configured(12345, "/keys/app.pem", 42).app_auth_configured());
    // Any one missing → off (a partial config never silently half-enables).
    assert!(!configured(0, "/keys/app.pem", 42).app_auth_configured());
    assert!(!configured(12345, "", 42).app_auth_configured());
    assert!(!configured(12345, "/keys/app.pem", 0).app_auth_configured());
}

#[test]
fn from_config_fails_fast_when_the_key_file_is_absent() {
    // ADR-0150: an absent key is a boot fault, never a silent fallback to an
    // ambient secret.
    let config = configured(12345, "/nonexistent/does-not-exist.pem", 42);
    assert!(AppTokenSource::from_config(&config).is_err());
}

#[test]
fn connect_client_takes_the_app_branch_when_configured_and_the_static_branch_otherwise() {
    // The host wiring under test (plan step 4): `connect_client` branches on
    // `app_auth_configured`. The App branch reads the host-local key and builds a
    // minted-token client; the static branch builds the backward-compatible
    // PAT client. Assert the branch is taken by its distinguishing behavior — a
    // configured-but-missing-key config errors (only the App branch reads a key),
    // while the same knobs pointed at a real key, and an unconfigured config,
    // both construct a client.
    use std::io::Write as _;

    // Unconfigured → the static-PAT branch constructs a client (no key read).
    assert!(GithubMirrorConfig::default().connect_client().is_ok(), "static-PAT path builds a client");

    // Configured with an absent key → the App branch is taken and fails fast
    // (the static branch would have succeeded, so the error proves the branch).
    let missing = configured(12345, "/nonexistent/does-not-exist.pem", 42);
    assert!(missing.connect_client().is_err(), "App path reads the key and fails fast when it is absent");

    // Configured with a real key on disk → the App branch constructs a
    // minted-token client.
    let mut key_file = tempfile::NamedTempFile::new().unwrap();
    key_file.write_all(TEST_PRIVATE_KEY.as_bytes()).unwrap();
    let path = key_file.path().to_str().unwrap().to_owned();
    let with_key = configured(12345, &path, 42);
    assert!(with_key.connect_client().is_ok(), "App path builds a client from a present key");
}

#[test]
fn mint_jwt_signs_a_verifiable_rs256_token_issued_by_the_app() {
    let source = AppTokenSource::with_exchange(999, 42, test_key(), 300, Arc::new(CountingExchange::new("e")));
    let jwt = source.mint_jwt().expect("the fixture key signs a JWT");

    // The public key verifies the signature (proving the private key signed it)
    // and the claims carry the App id as issuer with exp after iat.
    let decoded = decode::<TestClaims>(
        &jwt,
        &DecodingKey::from_rsa_pem(TEST_PUBLIC_KEY.as_bytes()).expect("fixture public key parses"),
        &Validation::new(Algorithm::RS256),
    )
    .expect("the JWT verifies against the public key");
    assert_eq!(decoded.claims.iss, "999");
    assert!(decoded.claims.exp > decoded.claims.iat);
}

#[test]
fn a_fresh_token_is_cached_and_reused_without_re_minting() {
    // A token whose GitHub-reported expiry is far in the future is still fresh on
    // the next call: exactly one exchange, and the same token both times.
    let exchange = Arc::new(CountingExchange::new("2099-01-01T00:00:00Z"));
    let source = AppTokenSource::with_exchange(1, 2, test_key(), 300, exchange.clone());

    let first = source.token().expect("first mint");
    let second = source.token().expect("cached reuse");
    assert_eq!(first, "ghs_minted_0");
    assert_eq!(second, "ghs_minted_0");
    assert_eq!(exchange.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn a_stale_token_is_re_minted_before_expiry() {
    // A token already past its GitHub-reported expiry is stale, so every call
    // re-mints — the cadence driven by the real reported expiry, not an assumed
    // lifetime. Each call yields a fresh token.
    let exchange = Arc::new(CountingExchange::new("2000-01-01T00:00:00Z"));
    let source = AppTokenSource::with_exchange(1, 2, test_key(), 300, exchange.clone());

    assert_eq!(source.token().expect("first mint"), "ghs_minted_0");
    assert_eq!(source.token().expect("re-mint"), "ghs_minted_1");
    assert_eq!(exchange.calls.load(Ordering::SeqCst), 2);
}

#[test]
fn an_unparseable_expiry_forces_a_re_mint_rather_than_trusting_it() {
    // GitHub should always return a strict RFC3339 expiry, but if it ever returns
    // something unparseable the source must not trust a token of unknown
    // lifetime: it caches a stale deadline (0) so the next call re-mints —
    // fail-safe, never fail-open on a malformed expiry.
    let exchange = Arc::new(CountingExchange::new("not-a-timestamp"));
    let source = AppTokenSource::with_exchange(1, 2, test_key(), 300, exchange.clone());

    assert_eq!(source.token().expect("first mint"), "ghs_minted_0");
    assert_eq!(source.token().expect("re-mint"), "ghs_minted_1");
    assert_eq!(exchange.calls.load(Ordering::SeqCst), 2);
}
