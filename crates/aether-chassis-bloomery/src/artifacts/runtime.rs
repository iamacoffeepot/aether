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
use super::kinds::{ArtifactsError, Get, GetRange, GetRangeResult, GetResult, Put, PutResult};

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

/// One stored artifact as a projection rebuild reads it (issue #3523): its
/// content-store digest, recorded derivation parents, and full bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactEntry {
    /// The content-store digest the bytes are addressed by.
    pub digest: String,
    /// The recorded derivation-DAG parents (an accepted study artifact names
    /// its graded attempt digest here).
    pub parents: Vec<String>,
    /// The stored bytes.
    pub bytes: Vec<u8>,
}

/// Resolve the store root: an explicit `--artifacts-root` / `AETHER_ARTIFACTS_ROOT`
/// wins; otherwise the platform data dir (`data_dir/aether/bloomery-artifacts`),
/// falling back to a temp-dir path when no data dir is resolvable. Mirrors the
/// hub engines cap's `resolve_fleet_store_root`.
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
    /// the digest, or the store's own write failure as an `AdapterError`. The
    /// `on_put` handler is a thin wrapper over this.
    pub fn put(&mut self, bytes: &[u8], parents: &[String]) -> PutResult {
        let digest = match self.store.upload(bytes, ArtifactMeta { parents: parents.to_vec() }, None) {
            Ok(digest) => digest,
            Err(error) => {
                return PutResult::Err {
                    error: ArtifactsError::AdapterError(format!("artifact bytes were not persisted: {error}")),
                };
            }
        };

        // Content-addressed dedup keeps the *original* sidecar metadata: a
        // re-upload of identical bytes under fresh `parents` bumps recency but
        // never rewrites the recorded parents (`ContentStore::upload`). That
        // holds for bytes a peer handle on the shared root stored too — the
        // upload adopts the on-disk entry before it would rewrite it. Surface
        // any newly-submitted parent the store dropped rather than replying Ok
        // as if the derivation edge landed — otherwise the new provenance
        // silently vanishes.
        if let Some(resolved) = self.store.get(&Selector::Hash(digest.clone())) {
            let recorded = &resolved.metadata.parents;
            let dropped: Vec<&String> = parents.iter().filter(|p| !recorded.contains(p)).collect();
            if !dropped.is_empty() {
                tracing::warn!(
                    target: "aether_chassis_bloomery::artifacts",
                    digest = %digest,
                    ?dropped,
                    "artifact re-upload deduped to an existing entry; newly-submitted derivation parents were not recorded (the store keeps the original metadata)"
                );
            }
        }
        PutResult::Ok { digest }
    }

    /// Every stored entry's digest, derivation parents, and bytes — the
    /// enumeration a projection rebuild reads (issue #3523). The eviction-free
    /// store is the only truth a rebuildable index projects over, so the rebuild
    /// scans it here rather than trusting the projection it is reconstructing.
    /// Entries whose bytes cannot be read from disk are skipped (a rebuild is
    /// best-effort over what is durably present, never a hard failure).
    ///
    /// The store root is shared: the executor reactor opens its own handle over
    /// it (`open_artifacts`) and files study records there long after this
    /// capability's handle was built at boot. `ContentStore::entries` answers
    /// from that boot-time index, so the scan refreshes first — otherwise the
    /// rebuild enumerates only what this handle happened to write itself.
    pub fn scan(&mut self) -> Vec<ArtifactEntry> {
        self.store.refresh();

        // Collect the (digest, parents) refs first: `get` borrows the store
        // mutably (it bumps recency), so it cannot run inside the `entries`
        // borrow.
        let refs: Vec<(String, Vec<String>)> =
            self.store.entries().map(|entry| (entry.hash.to_owned(), entry.metadata.parents.clone())).collect();
        let mut out = Vec::with_capacity(refs.len());
        for (digest, parents) in refs {
            if let Some(resolved) = self.store.get(&Selector::Hash(digest.clone()))
                && let Ok(bytes) = fs::read(&resolved.path)
            {
                out.push(ArtifactEntry { digest, parents, bytes });
            }
        }
        out
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
        tracing::info!(target: "aether_chassis_bloomery::artifacts", root = %state.store.root().display(), "artifacts store opened (eviction-free)");
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

    #[handler::single]
    fn on_get_range(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: GetRange) -> GetRangeResult {
        let GetRange { digest, offset, limit, decoded, notice } = mail;
        match state.get(digest.clone()) {
            GetResult::Ok { bytes, .. } => range_result(digest, &bytes, offset, limit, decoded, notice),
            GetResult::Err { digest, error } => GetRangeResult::Err { digest, error },
        }
    }
}

fn range_result(
    digest: String,
    bytes: &[u8],
    offset: u64,
    limit: u64,
    decoded: bool,
    notice: Option<String>,
) -> GetRangeResult {
    let total = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if offset >= total && !(offset == 0 && total == 0) {
        return GetRangeResult::Unsatisfiable { digest, total };
    }
    if decoded {
        return GetRangeResult::Ok {
            digest,
            bytes: bytes.to_vec(),
            total,
            offset,
            limit,
            decoded,
            notice,
            truncated: false,
        };
    }
    let start = usize::try_from(offset).unwrap_or(usize::MAX).min(bytes.len());
    let want = usize::try_from(limit).unwrap_or(usize::MAX);
    let end = start.saturating_add(want).min(bytes.len());
    GetRangeResult::Ok {
        digest,
        bytes: bytes[start..end].to_vec(),
        total,
        offset,
        limit,
        decoded,
        notice,
        truncated: end < bytes.len(),
    }
}
