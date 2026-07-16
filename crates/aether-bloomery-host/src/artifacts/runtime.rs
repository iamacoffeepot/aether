//! The eviction-free runtime for [`ArtifactsCapability`] (ADR-0149 §The
//! boundary).
//!
//! State is one [`ContentStore<ArtifactMeta>`] — the extracted content-address
//! core (`aether_substrate::content_store`) opened with
//! [`EvictionPolicy::None`], so nothing is ever reclaimed: a canonical record,
//! not a cache. Provenance (an artifact's derivation-DAG parents) rides the
//! store's per-entry sidecar as [`ArtifactMeta`], never an LRU cache. Being a
//! second *consumer* of the one addressing core — not a rival store — is the
//! ADR-0116 reuse-not-rival outcome ADR-0149 §The boundary requires.

use std::path::{Path, PathBuf};
use std::{env, fs};

use aether_actor::runtime;
use aether_substrate::content_store::{ContentStore, EvictionPolicy, Selector};
use serde::{Deserialize, Serialize};

use super::ArtifactsCapability;
use super::kinds::{ArtifactsError, Get, GetResult, Put, PutResult};

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;

/// The per-entry sidecar metadata: an artifact's derivation-DAG parents
/// ("every artifact names its parents", ADR-0149 §The value vocabulary). The
/// content store persists this JSON alongside each entry's bytes and restores
/// it on reopen, so parents survive a capability restart with no name and no
/// pin — the eviction-free property this slice exists to hold.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactMeta {
    /// The content digests this artifact was derived from. Recorded as-is;
    /// the store does not validate their presence (a reducer invariant, not a
    /// byte-store gate — ADR-0149).
    pub parents: Vec<String>,
}

/// Resolve the store root: an explicit `--artifacts-root` / `AETHER_ARTIFACTS_ROOT`
/// wins; otherwise the platform data dir (`data_dir/aether/bloomery-artifacts`),
/// falling back to a temp-dir path when no data dir is resolvable. Mirrors the
/// hub engines cap's `resolve_engine_store_root`.
#[must_use]
pub fn resolve_root(configured: Option<&str>) -> PathBuf {
    if let Some(dir) = configured.filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(data) = dirs::data_dir() {
        return data.join("aether").join("bloomery-artifacts");
    }
    env::temp_dir().join("aether-bloomery-artifacts")
}

/// Runtime state for [`ArtifactsCapability`]: the one eviction-free content
/// store the dispatcher owns.
pub struct ArtifactsCapabilityState {
    store: ContentStore<ArtifactMeta>,
}

impl ArtifactsCapabilityState {
    /// Open (or restore) the eviction-free store rooted at `root` — the seam
    /// the handler and restart tests drive over a temp root.
    ///
    /// # Errors
    ///
    /// Returns [`BootError`] only on total storage failure (the configured
    /// root and the core's temp fallback both uncreatable).
    pub fn open(root: &Path) -> Result<Self, BootError> {
        let store =
            ContentStore::open(root, EvictionPolicy::None).map_err(|error| BootError::Other(Box::new(error)))?;
        Ok(Self { store })
    }

    /// Store `bytes` content-addressed with their declared `parents`, replying
    /// the digest. The `on_put` handler is a thin wrapper over this; it carries
    /// the durability check.
    pub fn put(&mut self, bytes: &[u8], parents: &[String]) -> PutResult {
        let digest = self.store.upload(bytes, ArtifactMeta { parents: parents.to_vec() }, None);
        // `upload` swallows a write failure (logs a warn, leaves the entry
        // unindexed) and still returns the hash, so `contains` is the honest
        // durability check: an unindexed digest means the bytes never landed.
        if !self.store.contains(&digest) {
            return PutResult::Err {
                error: ArtifactsError::AdapterError("artifact bytes were not persisted".to_owned()),
            };
        }
        // Content-addressed dedup keeps the *original* sidecar metadata: a
        // re-upload of identical bytes under fresh `parents` bumps recency but
        // never rewrites the recorded parents (`ContentStore::upload`). Surface
        // any newly-submitted parent the store dropped rather than replying Ok
        // as if the derivation edge landed — otherwise the new provenance
        // silently vanishes.
        if let Some(resolved) = self.store.get(&Selector::Hash(digest.clone())) {
            let recorded = &resolved.metadata.parents;
            let dropped: Vec<&String> = parents.iter().filter(|p| !recorded.contains(p)).collect();
            if !dropped.is_empty() {
                tracing::warn!(
                    target: "aether_bloomery_host::artifacts",
                    digest = %digest,
                    ?dropped,
                    "artifact re-upload deduped to an existing entry; newly-submitted derivation parents were not recorded (the store keeps the original metadata)"
                );
            }
        }
        PutResult::Ok { digest }
    }

    /// Resolve `digest` to its bytes + recorded parents, replying `NotFound`
    /// for an absent digest and `AdapterError` for a disk read failure of an
    /// indexed entry. The `on_get` handler delegates here.
    pub fn get(&mut self, digest: String) -> GetResult {
        match self.store.get(&Selector::Hash(digest.clone())) {
            Some(resolved) => match fs::read(&resolved.path) {
                Ok(bytes) => GetResult::Ok { digest, bytes, parents: resolved.metadata.parents },
                Err(error) => GetResult::Err { digest, error: ArtifactsError::AdapterError(error.to_string()) },
            },
            None => GetResult::Err { digest, error: ArtifactsError::NotFound },
        }
    }
}

#[runtime]
impl NativeActor for ArtifactsCapability {
    type State = ArtifactsCapabilityState;
    type Config = super::ArtifactsConfig;

    const NAMESPACE: &'static str = "aether.artifacts";

    fn init(
        config: super::ArtifactsConfig,
        _ctx: &mut NativeInitCtx<'_>,
    ) -> Result<ArtifactsCapabilityState, BootError> {
        let root = resolve_root(config.root.as_deref());
        let state = ArtifactsCapabilityState::open(&root)?;
        tracing::info!(target: "aether_bloomery_host::artifacts", root = %state.store.root().display(), "artifacts store opened (eviction-free)");
        Ok(state)
    }

    #[handler::single]
    fn on_put(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: Put) -> PutResult {
        let Put { bytes, parents } = mail;
        state.put(&bytes, &parents)
    }

    #[handler::single]
    fn on_get(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: Get) -> GetResult {
        state.get(mail.digest)
    }
}
