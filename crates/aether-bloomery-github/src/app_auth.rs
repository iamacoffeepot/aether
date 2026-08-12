//! The GitHub-App installation-token minter (ADR-0149 §Migration step 3,
//! ADR-0150 host-locality).
//!
//! [`AppTokenSource`] is the in-process [`TokenSource`] a client resolves each
//! request's bearer from. It takes the App private key as PEM bytes, mints a
//! short-lived RS256 App JWT from it, exchanges the JWT for an installation
//! access token, caches the token, and re-mints once the cached token is within
//! the refresh skew of expiry — so tokens are minted on a refresh cadence, never
//! per request. It sits here beside [`StaticTokenSource`] because both are
//! [`TokenSource`] implementations over the GitHub protocol; the embedder reads
//! the host-local key file and hands the bytes in, so the key's custody stays on
//! the host (ADR-0150) while the protocol stays in this adapter.
//!
//! The JWT→installation-token exchange is a network hop, so it sits behind the
//! [`InstallationTokenExchange`] seam: production drives the real
//! `POST /app/installations/{id}/access_tokens` through the adapter, and tests
//! inject a counting fake to assert the caching/refresh behavior with no
//! network.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::Serialize;

use crate::client::{GithubError, InstallationToken, ReqwestGithub, StaticTokenSource, TokenSource};

/// The default refresh skew when the caller's skew resolves to `0` — re-mint
/// five minutes before the GitHub-reported expiry.
const DEFAULT_SKEW_SECS: u64 = 300;

/// The App JWT's lifetime — nine minutes, under GitHub's ten-minute ceiling.
const JWT_LIFETIME_SECS: u64 = 540;

/// Backdate the JWT's `iat` a minute to tolerate clock skew against GitHub, as
/// GitHub's own App-auth guidance recommends.
const JWT_BACKDATE_SECS: u64 = 60;

/// The JWT → installation-token exchange
/// (`POST /app/installations/{id}/access_tokens`), behind a seam so the caching
/// logic is tested without a network. `Send + Sync` because [`AppTokenSource`]
/// is held behind an `Arc` and driven from a capability's dispatch thread.
pub trait InstallationTokenExchange: Send + Sync {
    /// Exchange the App `jwt` for an installation access token for
    /// `installation_id`.
    ///
    /// # Errors
    /// The exchange surface is unreachable or refused the JWT.
    fn exchange(&self, jwt: &str, installation_id: u64) -> Result<InstallationToken, GithubError>;
}

/// The production exchange: builds a client bearing the App JWT and drives the
/// real GitHub token-exchange endpoint.
struct HttpExchange {
    api_base: String,
}

impl InstallationTokenExchange for HttpExchange {
    fn exchange(&self, jwt: &str, installation_id: u64) -> Result<InstallationToken, GithubError> {
        // The exchange authenticates with the App JWT (not an installation
        // token); a client whose source is that JWT drives the single
        // `create_installation_token` call, then is discarded.
        let source = Arc::new(StaticTokenSource::new(jwt.to_owned()));
        let client = ReqwestGithub::with_token_source(source, self.api_base.clone(), String::new())?;
        client.create_installation_token(installation_id)
    }
}

/// The claims of a GitHub App JWT: issued-at, expiry, and the App id as issuer.
#[derive(Serialize)]
struct AppJwtClaims {
    iat: u64,
    exp: u64,
    iss: String,
}

/// A cached installation token and the unix second at which it becomes stale —
/// the GitHub-reported expiry minus the refresh skew. Driving refresh from the
/// real `expires_at` (not an assumed lifetime) keeps the cadence correct even if
/// GitHub changes the installation-token lifetime.
struct Cached {
    token: InstallationToken,
    refresh_at_unix: u64,
}

/// The in-process installation-token custody: mints, caches, and refreshes the
/// bearer a client authenticates with (ADR-0149 §Migration step 3). The App
/// private key is parsed once at construction and never echoed (ADR-0150).
pub struct AppTokenSource {
    app_id: u64,
    installation_id: u64,
    key: EncodingKey,
    skew_secs: u64,
    exchanger: Arc<dyn InstallationTokenExchange>,
    cache: Mutex<Option<Cached>>,
}

impl AppTokenSource {
    /// Build the source over plain values: parse the App's RSA private-key PEM
    /// and wire the production exchange against `api_base`. A malformed key
    /// fails fast here (ADR-0150 — no silent fallback to an ambient secret).
    /// `skew_secs` is how many seconds before the reported expiry to re-mint;
    /// `0` resolves to [`DEFAULT_SKEW_SECS`].
    ///
    /// The caller owns reading the key file, so the host-local path never
    /// crosses into this adapter.
    ///
    /// # Errors
    /// The bytes are not a valid RSA PEM.
    pub fn new(
        app_id: u64,
        installation_id: u64,
        private_key_pem: &[u8],
        skew_secs: u64,
        api_base: String,
    ) -> Result<Self, GithubError> {
        let key = EncodingKey::from_rsa_pem(private_key_pem)
            .map_err(|error| GithubError::Transport(format!("parsing GitHub App private-key PEM: {error}")))?;
        Ok(Self::with_exchange(
            app_id,
            installation_id,
            key,
            resolve_skew_secs(skew_secs),
            Arc::new(HttpExchange { api_base }),
        ))
    }

    /// Build the source over an explicit exchange — the test seam (a counting
    /// fake asserts the caching/refresh behavior with no network). `skew_secs` is
    /// how many seconds before the reported expiry to re-mint.
    #[must_use]
    pub fn with_exchange(
        app_id: u64,
        installation_id: u64,
        key: EncodingKey,
        skew_secs: u64,
        exchanger: Arc<dyn InstallationTokenExchange>,
    ) -> Self {
        Self { app_id, installation_id, key, skew_secs, exchanger, cache: Mutex::new(None) }
    }

    /// Sign a fresh App JWT from the private key (RS256, `iss` = App id, `iat`
    /// backdated for clock skew, `exp` under GitHub's ten-minute ceiling).
    ///
    /// # Errors
    /// The system clock is before the Unix epoch, or JWT signing failed.
    pub fn mint_jwt(&self) -> Result<String, GithubError> {
        let now = now_unix()?;
        let claims = AppJwtClaims {
            iat: now.saturating_sub(JWT_BACKDATE_SECS),
            exp: now + JWT_LIFETIME_SECS,
            iss: self.app_id.to_string(),
        };
        encode(&Header::new(Algorithm::RS256), &claims, &self.key)
            .map_err(|error| GithubError::Transport(format!("signing GitHub App JWT: {error}")))
    }

    /// The current installation token — the cached one while the wall clock is
    /// still before its skew-adjusted GitHub-reported expiry, else a freshly
    /// minted-and-cached one. [`TokenSource::token`] projects out the bearer.
    ///
    /// # Errors
    /// JWT signing failed or the token exchange was refused.
    // The cache lock is deliberately held across the mint+exchange: a concurrent
    // caller finding the token stale waits for the in-flight mint rather than
    // racing a second exchange against GitHub.
    #[allow(clippy::significant_drop_tightening)]
    fn current(&self) -> Result<InstallationToken, GithubError> {
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(cached) = cache.as_ref()
            && now_unix()? < cached.refresh_at_unix
        {
            return Ok(cached.token.clone());
        }
        let jwt = self.mint_jwt()?;
        let token = self.exchanger.exchange(&jwt, self.installation_id)?;
        let refresh_at_unix = refresh_deadline(&token.expires_at, self.skew_secs);
        *cache = Some(Cached { token: token.clone(), refresh_at_unix });
        Ok(token)
    }
}

impl TokenSource for AppTokenSource {
    fn token(&self) -> Result<String, GithubError> {
        self.current().map(|minted| minted.token)
    }
}

/// Resolve the refresh skew in seconds, defaulting `0` to [`DEFAULT_SKEW_SECS`].
fn resolve_skew_secs(secs: u64) -> u64 {
    if secs == 0 {
        DEFAULT_SKEW_SECS
    } else {
        secs
    }
}

/// The unix second at which a cached token becomes stale: the GitHub-reported
/// `expires_at` minus `skew_secs`, saturating so an extreme skew can never
/// underflow. A malformed expiry (never expected from GitHub) yields `0`, which
/// forces a re-mint on the next call rather than trusting a token of unknown
/// lifetime — fail-safe, never fail-open (the token minted alongside it is still
/// returned to the caller this once).
fn refresh_deadline(expires_at: &str, skew_secs: u64) -> u64 {
    let Some(expiry) = parse_rfc3339_to_unix(expires_at) else {
        tracing::warn!(
            target: "aether_bloomery_github::app_auth",
            expires_at,
            "installation token expiry did not parse; forcing a re-mint next call"
        );
        return 0;
    };
    expiry.saturating_sub(skew_secs)
}

/// Parse the strict `YYYY-MM-DDTHH:MM:SSZ` UTC timestamp GitHub returns for a
/// token's `expires_at` into seconds since the Unix epoch. Returns `None` on any
/// shape deviation (wrong length, non-digit field, missing separator, or an
/// out-of-range field) rather than guessing.
fn parse_rfc3339_to_unix(timestamp: &str) -> Option<u64> {
    let bytes = timestamp.as_bytes();
    if bytes.len() != 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
    {
        return None;
    }
    let field = |start: usize, len: usize| -> Option<i64> {
        let mut value: i64 = 0;
        for &digit in &bytes[start..start + len] {
            if !digit.is_ascii_digit() {
                return None;
            }
            value = value * 10 + i64::from(digit - b'0');
        }
        Some(value)
    };
    let year = field(0, 4)?;
    let month = field(5, 2)?;
    let day = field(8, 2)?;
    let hour = field(11, 2)?;
    let minute = field(14, 2)?;
    let second = field(17, 2)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second;
    u64::try_from(seconds).ok()
}

/// Days since the Unix epoch (1970-01-01) for a proleptic-Gregorian date —
/// Howard Hinnant's `days_from_civil` (correct across leap years and centuries).
/// `month` is 1..=12 and `day` is 1..=31 (validated by the caller).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = (if year >= 0 {
        year
    } else {
        year - 399
    }) / 400;
    let year_of_era = year - era * 400;
    let day_of_year =
        (153 * (if month > 2 {
            month - 3
        } else {
            month + 9
        }) + 2)
            / 5
            + day
            - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Seconds since the Unix epoch, for the JWT `iat`/`exp`.
fn now_unix() -> Result<u64, GithubError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .map_err(|error| GithubError::Transport(format!("system clock is before the Unix epoch: {error}")))
}

/// Unit tests for the App-token custody (ADR-0149 §Migration step 3).
///
/// The JWT minting and the cache/refresh logic are this module's own — a fixture
/// RSA keypair signs a real JWT (verified with the public key), and a counting
/// fake exchange stands in for the network so the refresh-before-expiry cadence
/// is asserted with no live GitHub. Tripwire: mint-once-then-cache vs
/// re-mint-when-stale is the behavior the design turns on, not a passthrough.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Validation, decode};
    use serde::Deserialize;

    use super::{AppTokenSource, InstallationTokenExchange, parse_rfc3339_to_unix, refresh_deadline};
    use crate::client::{GithubError, InstallationToken, TokenSource};

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
    fn new_parses_a_pem_into_a_signing_source_and_rejects_a_malformed_one() {
        // The plain-param constructor the embedder wires: valid PEM bytes yield a
        // source that signs, and bytes that are not an RSA PEM fail fast here
        // rather than at the first token request (ADR-0150).
        let source = AppTokenSource::new(7, 8, TEST_PRIVATE_KEY.as_bytes(), 0, "https://api.github.com".to_owned())
            .expect("the fixture PEM parses");
        assert!(source.mint_jwt().is_ok(), "a source built from PEM bytes signs a JWT");

        assert!(AppTokenSource::new(7, 8, b"not a pem", 0, "https://api.github.com".to_owned()).is_err());
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

    #[test]
    fn parse_rfc3339_matches_known_epochs() {
        // Tripwire: the hand-rolled civil-date math is pinned to well-known Unix
        // timestamps so a leap-year / century / field-offset slip trips here
        // rather than silently skewing every token's refresh deadline.
        assert_eq!(parse_rfc3339_to_unix("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_to_unix("2000-01-01T00:00:00Z"), Some(946_684_800));
        // 2024 is a leap year — 2024-03-01 is day 59 (Jan 31 + Feb 29) after the year start.
        assert_eq!(parse_rfc3339_to_unix("2024-03-01T00:00:00Z"), Some(1_709_251_200));
        assert_eq!(parse_rfc3339_to_unix("2026-07-17T13:00:00Z"), Some(1_784_293_200));
    }

    #[test]
    fn parse_rfc3339_rejects_malformed_shapes() {
        assert_eq!(parse_rfc3339_to_unix(""), None);
        assert_eq!(parse_rfc3339_to_unix("2026-07-17 13:00:00Z"), None); // space, not `T`
        assert_eq!(parse_rfc3339_to_unix("2026-07-17T13:00:00"), None); // missing `Z`
        assert_eq!(parse_rfc3339_to_unix("2026-13-17T13:00:00Z"), None); // month 13
        assert_eq!(parse_rfc3339_to_unix("2026-07-17T24:00:00Z"), None); // hour 24
        assert_eq!(parse_rfc3339_to_unix("20x6-07-17T13:00:00Z"), None); // non-digit
    }

    #[test]
    fn refresh_deadline_saturates_an_extreme_skew_to_zero() {
        // A pasted sentinel skew cannot underflow the deadline — it saturates to 0
        // (always stale) rather than wrapping to a far-future never-refresh value.
        assert_eq!(refresh_deadline("2000-01-01T00:00:00Z", u64::MAX), 0);
        // A malformed expiry also yields 0 (fail-safe: re-mint next call).
        assert_eq!(refresh_deadline("bogus", 300), 0);
        // A normal case subtracts the skew from the parsed expiry.
        assert_eq!(refresh_deadline("2000-01-01T00:00:00Z", 300), 946_684_800 - 300);
    }
}
