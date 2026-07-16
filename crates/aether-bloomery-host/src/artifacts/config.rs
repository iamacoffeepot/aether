//! The `artifacts` capability's boot configuration (ADR-0090 derive-`Config`).

/// Where the eviction-free artifacts content store lives, resolved
/// argv > env > default. There is deliberately **no** disk-budget or eviction
/// knob — the artifacts port is a canonical record that never evicts
/// (ADR-0149).
///
/// `root` is the store-root directory holding the content-addressed entries
/// and their sidecars. A bare `Option<String>` (not a literal default) so an
/// unset root resolves to a computed data-dir path at `init`
/// ([`resolve_root`](super::runtime::resolve_root)); `--artifacts-root` /
/// `AETHER_ARTIFACTS_ROOT` override it.
#[derive(Clone, Debug, Default, aether_substrate::Config)]
#[config(env_prefix = "AETHER_ARTIFACTS", cli_prefix = "artifacts")]
pub struct ArtifactsConfig {
    /// The store-root directory; unset → the computed data-dir default.
    #[config(env = "AETHER_ARTIFACTS_ROOT")]
    pub root: Option<String>,
}
