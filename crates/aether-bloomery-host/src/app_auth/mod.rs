//! The `app_auth` custody module — GitHub-App installation-token minting
//! (ADR-0149 §Migration step 3, ADR-0150 host-locality).
//!
//! Bloomery's GitHub adapter authenticated every request with one static bearer
//! token frozen at construction (`GithubConfig.token`, the `GITHUB_TOKEN` PAT).
//! ADR-0149 §Migration step 3 relocates App-key custody off the ambient
//! workflow secret into a host-local minter: it holds the App private key
//! host-local (ADR-0150 — the key bytes never leave the machine, never cross
//! into wasm or a config echo), mints a short-lived RS256 App JWT from it,
//! exchanges the JWT for an installation access token
//! (`POST /app/installations/{id}/access_tokens`), caches the token, and
//! re-mints before expiry. The adapter's client consumes the minted token
//! through the [`aether_bloomery_github::TokenSource`] seam instead of a static
//! PAT.
//!
//! The custody is an in-process [`AppTokenSource`] handle, not a per-request
//! mail hop (a round-trip on every GitHub call would serialize the hot path):
//! the port shells build their client from the source at boot
//! ([`GithubMirrorConfig::connect_client`](crate::bloomery::GithubMirrorConfig::connect_client))
//! and the source caches-and-refreshes behind them. An absent key fails that
//! client construction fast (ADR-0150 — no silent fallback to an ambient
//! secret), never over a mail boundary the key would have to cross.

#[cfg(feature = "runtime")]
mod minter;
#[cfg(feature = "runtime")]
pub use minter::{AppTokenSource, InstallationTokenExchange};

#[cfg(all(test, feature = "runtime"))]
mod tests;
