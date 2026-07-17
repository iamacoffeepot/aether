//! The GitHub-App installation-token minter (ADR-0149 §Migration step 3,
//! ADR-0150 host-locality).
//!
//! [`AppTokenSource`] is the in-process [`TokenSource`] the port shells' client
//! resolves each request's bearer from. It holds the App private key host-local
//! (read once at construction, never echoed), mints a short-lived RS256 App JWT
//! from it, exchanges the JWT for an installation access token, caches the
//! token, and re-mints once the cached token is within the refresh skew of
//! expiry — so tokens are minted on a refresh cadence, never per request.
//!
//! The JWT→installation-token exchange is a network hop, so it sits behind the
//! [`InstallationTokenExchange`] seam: production drives the real
//! `POST /app/installations/{id}/access_tokens` through the adapter, and tests
//! inject a counting fake to assert the caching/refresh behavior with no
//! network.

use std::fs;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use aether_bloomery_github::{GithubError, InstallationToken, ReqwestGithub, StaticTokenSource, TokenSource};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::Serialize;

use crate::bloomery::GithubMirrorConfig;

/// The default refresh skew when `app_token_skew_secs` resolves to `0` — re-mint
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
/// bearer the port shells' client authenticates with (ADR-0149 §Migration
/// step 3). Holds the App private key host-local (ADR-0150).
pub struct AppTokenSource {
    app_id: u64,
    installation_id: u64,
    key: EncodingKey,
    skew_secs: u64,
    exchanger: Arc<dyn InstallationTokenExchange>,
    cache: Mutex<Option<Cached>>,
}

impl AppTokenSource {
    /// Build the source from resolved config: read the host-local private-key
    /// PEM, parse the RSA key, and wire the production exchange. A missing or
    /// malformed key fails fast here (ADR-0150 — no silent fallback to an
    /// ambient secret).
    ///
    /// # Errors
    /// The private-key file is unreadable, or its bytes are not a valid RSA PEM.
    pub fn from_config(config: &GithubMirrorConfig) -> Result<Self, GithubError> {
        let pem = fs::read(&config.app_private_key_path).map_err(|error| {
            GithubError::Transport(format!("reading GitHub App private key '{}': {error}", config.app_private_key_path))
        })?;
        let key = EncodingKey::from_rsa_pem(&pem)
            .map_err(|error| GithubError::Transport(format!("parsing GitHub App private-key PEM: {error}")))?;
        let exchanger = Arc::new(HttpExchange { api_base: config.api_base.clone() });
        Ok(Self::with_exchange(
            config.app_id,
            config.app_installation_id,
            key,
            skew_secs(config.app_token_skew_secs),
            exchanger,
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

    /// Sign a fresh App JWT from the host-local key (RS256, `iss` = App id,
    /// `iat` backdated for clock skew, `exp` under GitHub's ten-minute ceiling).
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
    /// minted-and-cached one. The introspection handler reads the full token (for
    /// its expiry); [`TokenSource::token`] projects out the bearer.
    ///
    /// # Errors
    /// JWT signing failed or the token exchange was refused.
    // The cache lock is deliberately held across the mint+exchange: a concurrent
    // caller finding the token stale waits for the in-flight mint rather than
    // racing a second exchange against GitHub.
    #[allow(clippy::significant_drop_tightening)]
    pub fn current(&self) -> Result<InstallationToken, GithubError> {
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
fn skew_secs(secs: u64) -> u64 {
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
            target: "aether_bloomery_host::app_auth",
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

#[cfg(test)]
mod tests {
    use super::{parse_rfc3339_to_unix, refresh_deadline};

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
