//! The key-holding runtime for [`AppAuthCapability`] (ADR-0149 §Migration
//! step 3, ADR-0150 host-locality).
//!
//! State holds the [`AppTokenSource`] custody built at boot — `Some` when
//! App-auth is configured (a missing key fails boot rather than silently
//! falling back), `None` on an unconfigured bin, where the static-PAT path is in
//! effect and the mint request answers [`MintTokenResult::Disabled`]. The port
//! shells never route through this mailbox; they hold their own in-process
//! [`AppTokenSource`] handle. This actor is the addressable custody identity and
//! the `aether.app_auth.mint` operational-confirmation surface.

use std::sync::Arc;

use aether_actor::runtime;

use super::AppAuthCapability;
use super::kinds::{MintInstallationToken, MintTokenResult};
use super::minter::AppTokenSource;

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;

/// Runtime state for [`AppAuthCapability`]: the installation-token custody, or
/// `None` when App-auth is unconfigured.
pub struct AppAuthCapabilityState {
    source: Option<Arc<AppTokenSource>>,
}

impl AppAuthCapabilityState {
    /// Build state over an explicit custody (or `None` for the unconfigured
    /// path) — the seam the handler tests drive.
    #[must_use]
    pub fn new(source: Option<Arc<AppTokenSource>>) -> Self {
        Self { source }
    }

    /// Force a mint (or return the cached token's expiry) — the body of the
    /// `aether.app_auth.mint` handler. Reports only the expiry, never the token
    /// bytes or the key (ADR-0150).
    #[must_use]
    pub fn mint(&self) -> MintTokenResult {
        self.source.as_ref().map_or(MintTokenResult::Disabled, |source| match source.current() {
            Ok(token) => MintTokenResult::Minted { expires_at: token.expires_at },
            Err(error) => MintTokenResult::Err { error: error.to_string() },
        })
    }
}

#[runtime]
impl NativeActor for AppAuthCapability {
    type State = AppAuthCapabilityState;
    type Config = super::AppAuthConfig;

    const NAMESPACE: &'static str = "aether.app_auth";

    fn init(config: super::AppAuthConfig, _ctx: &mut NativeInitCtx<'_>) -> Result<AppAuthCapabilityState, BootError> {
        // App-auth configured → read the host-local key and build the custody,
        // failing boot on an absent / malformed key (ADR-0150 — no silent
        // fallback). Unconfigured → inert custody, the static-PAT path in effect.
        let source = if config.app_auth_configured() {
            let source = AppTokenSource::from_config(&config).map_err(|error| BootError::Other(Box::new(error)))?;
            tracing::info!(
                target: "aether_bloomery_host::app_auth",
                app_id = config.app_id,
                installation_id = config.app_installation_id,
                "app-auth installation-token custody configured"
            );
            Some(Arc::new(source))
        } else {
            tracing::info!(
                target: "aether_bloomery_host::app_auth",
                "app-auth unconfigured; static-PAT auth in effect"
            );
            None
        };
        Ok(AppAuthCapabilityState { source })
    }

    // The `#[handler::single]` contract requires the mail by value; the request
    // is a ZST carrying no fields, so clippy sees a by-ref opportunity the macro
    // signature cannot take.
    #[allow(clippy::needless_pass_by_value)]
    #[handler::single]
    fn on_mint(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: MintInstallationToken) -> MintTokenResult {
        state.mint()
    }
}
