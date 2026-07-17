//! The `app_auth` capability's boot configuration (ADR-0090 derive-`Config`).
//!
//! The App-auth knobs — `app_id`, the host-local `app_private_key_path`, the
//! `app_installation_id`, and the refresh `app_token_skew_secs` — ride the same
//! [`GithubMirrorConfig`](crate::bloomery::GithubMirrorConfig) the mirror,
//! source, and executor shells use (`bloomery/mirror.rs`): one GitHub-connection
//! config carries the whole outbound path's auth, so the custody actor and the
//! port shells resolve the same App identity rather than duplicating the knobs.

pub use crate::bloomery::GithubMirrorConfig as AppAuthConfig;
