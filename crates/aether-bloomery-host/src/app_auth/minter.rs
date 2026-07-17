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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aether_bloomery_github::{GithubError, InstallationToken, ReqwestGithub, StaticTokenSource, TokenSource};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::Serialize;

use crate::bloomery::GithubMirrorConfig;

/// GitHub installation access tokens live one hour; the source re-mints once a
/// cached token is within the refresh skew of this. The `expires_at` GitHub
/// returns is kept for diagnostics but not parsed — a mint-time lifetime plus a
/// generous skew refreshes well before the real expiry without a date-parsing
/// dependency.
const INSTALLATION_TOKEN_LIFETIME: Duration = Duration::from_hours(1);

/// The default refresh skew when `app_token_skew_secs` resolves to `0` — re-mint
/// five minutes before expiry.
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

/// A cached installation token and the instant it was minted, for the
/// refresh-before-expiry check.
struct Cached {
    token: InstallationToken,
    minted_at: Instant,
}

/// The in-process installation-token custody: mints, caches, and refreshes the
/// bearer the port shells' client authenticates with (ADR-0149 §Migration
/// step 3). Holds the App private key host-local (ADR-0150).
pub struct AppTokenSource {
    app_id: u64,
    installation_id: u64,
    key: EncodingKey,
    skew: Duration,
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
            skew_from_secs(config.app_token_skew_secs),
            exchanger,
        ))
    }

    /// Build the source over an explicit exchange — the test seam (a counting
    /// fake asserts the caching/refresh behavior with no network).
    #[must_use]
    pub fn with_exchange(
        app_id: u64,
        installation_id: u64,
        key: EncodingKey,
        skew: Duration,
        exchanger: Arc<dyn InstallationTokenExchange>,
    ) -> Self {
        Self { app_id, installation_id, key, skew, exchanger, cache: Mutex::new(None) }
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

    /// The current installation token — the cached one when it is still outside
    /// the refresh skew, else a freshly minted-and-cached one. The introspection
    /// handler reads the full token (for its expiry); [`TokenSource::token`]
    /// projects out the bearer.
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
            && cached.minted_at.elapsed() + self.skew < INSTALLATION_TOKEN_LIFETIME
        {
            return Ok(cached.token.clone());
        }
        let jwt = self.mint_jwt()?;
        let token = self.exchanger.exchange(&jwt, self.installation_id)?;
        *cache = Some(Cached { token: token.clone(), minted_at: Instant::now() });
        Ok(token)
    }
}

impl TokenSource for AppTokenSource {
    fn token(&self) -> Result<String, GithubError> {
        self.current().map(|minted| minted.token)
    }
}

/// Resolve the refresh skew, defaulting `0` to [`DEFAULT_SKEW_SECS`].
fn skew_from_secs(secs: u64) -> Duration {
    Duration::from_secs(if secs == 0 {
        DEFAULT_SKEW_SECS
    } else {
        secs
    })
}

/// Seconds since the Unix epoch, for the JWT `iat`/`exp`.
fn now_unix() -> Result<u64, GithubError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .map_err(|error| GithubError::Transport(format!("system clock is before the Unix epoch: {error}")))
}
