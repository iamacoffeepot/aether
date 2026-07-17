//! The `app_auth` native capability — GitHub-App installation-token custody
//! (ADR-0149 §Migration step 3, ADR-0150 host-locality).
//!
//! Bloomery's GitHub adapter authenticated every request with one static bearer
//! token frozen at construction (`GithubConfig.token`, the `GITHUB_TOKEN` PAT).
//! ADR-0149 §Migration step 3 relocates App-key custody off the ambient
//! workflow secret into this host native capability: it holds the App private
//! key host-local (ADR-0150 — the key bytes never leave the machine, never
//! cross into wasm or a config echo), mints a short-lived RS256 App JWT from it,
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
//! and the source caches-and-refreshes behind them. This capability is the
//! addressable custody actor — it reads the key at boot (failing fast when it is
//! absent, rather than silently falling back to an ambient secret) and exposes
//! an `aether.app_auth.mint` introspection surface that forces a mint and
//! reports the resulting expiry without ever echoing the token or the key.
//!
//! Identity/runtime split (ADR-0122): the [`AppAuthCapability`] ZST + the
//! `aether.app_auth.*` kind family are always-on; the key-holding,
//! JWT-minting runtime lives behind the `runtime` feature.

pub mod kinds;
pub use kinds::*;

#[cfg(feature = "runtime")]
mod config;
#[cfg(feature = "runtime")]
pub use config::AppAuthConfig;

#[cfg(feature = "runtime")]
mod minter;
#[cfg(feature = "runtime")]
pub use minter::{AppTokenSource, InstallationTokenExchange};

use aether_actor::actor;

/// Addressing identity for the `aether.app_auth` capability.
#[actor(singleton)]
pub struct AppAuthCapability;

#[cfg(feature = "runtime")]
mod runtime;
#[cfg(feature = "runtime")]
pub use runtime::AppAuthCapabilityState;

#[cfg(all(test, feature = "runtime"))]
mod tests;
