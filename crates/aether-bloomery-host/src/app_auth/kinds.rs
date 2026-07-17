//! The `aether.app_auth.*` transact-mail kind family (ADR-0149 §Migration
//! step 3).
//!
//! The introspection surface the custody actor exposes: a caller mails
//! [`MintInstallationToken`] to the `aether.app_auth` mailbox to force a mint
//! (or return the cached token's) and learn the resulting expiry, without the
//! token or the private key ever crossing the mail boundary (ADR-0150). The
//! port shells never use this — they hold the in-process
//! [`AppTokenSource`](super::AppTokenSource) handle and never mail for a token —
//! so this exists for operational confirmation (a dogfood / boot check that
//! App-auth is wired) rather than the request hot path.

use serde::{Deserialize, Serialize};

/// Force a mint (or return the cached installation token's expiry) — the
/// custody actor's introspection request. Carries nothing: the App identity is
/// boot config, not a per-request field.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.app_auth.mint")]
pub struct MintInstallationToken;

/// Reply to [`MintInstallationToken`]. Reports the outcome and the resulting
/// token's expiry — never the token bytes or the private key (ADR-0150 keeps
/// the credential host-local; the shells consume it in-process, not over mail).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.app_auth.mint_result")]
pub enum MintTokenResult {
    /// A token is held — freshly minted or still-cached. Carries the expiry
    /// GitHub reported (RFC3339), for operational confirmation.
    Minted {
        /// The installation token's expiry as GitHub reports it (RFC3339).
        expires_at: String,
    },
    /// App-auth is not configured on this bin, so there is nothing to mint —
    /// the static-PAT path is in effect instead.
    Disabled,
    /// The mint failed — a JWT-signing or token-exchange fault.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}
