//! The adapter's connection configuration.
//!
//! A plain data struct, not an ADR-0090 derive-`Config`: this crate is a pure
//! adapter rlib and does not depend on `aether-substrate` (where the `Config`
//! derive lives). The host owns the derive-`Config` knobs (token, owner/name,
//! API base, CAS-land enable) and constructs this POD from the resolved
//! values (#3459 §Affected surfaces — "the derive-`Config` knobs live in host
//! cap wiring"). Keeping the resolution host-side is also what keeps the
//! `no core module names a GitHub type` boundary intact: the env/argv layer
//! never sees a github-crate type either.

/// Where the adapter talks to GitHub, and whether the (out-of-scope this
/// slice) compare-and-swap mainline land is permitted.
#[derive(Clone, Debug)]
pub struct GithubConfig {
    /// The bearer token the client authenticates with.
    pub token: String,
    /// The repository owner (user or org) the projections live under.
    pub owner: String,
    /// The repository name.
    pub repo: String,
    /// The REST API base — `https://api.github.com` for github.com, or a
    /// GitHub Enterprise base. No trailing slash.
    pub api_base: String,
    /// Whether compare-and-swap mainline landing is permitted. Defaults off
    /// (ADR-0149 gates it to migration step 3); the git source port that
    /// consumes it is a separate slice, so this flag is carried but unused
    /// here.
    pub cas_land_enabled: bool,
}

impl GithubConfig {
    /// The repo path segment (`owner/repo`) every REST route is built on.
    #[must_use]
    pub fn repo_path(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}
