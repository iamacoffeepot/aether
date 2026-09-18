//! Injected-data sandbox a program runs against.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::vec::Vec;
use core::marker::PhantomData;

use aether_bloomery_kinds::{ClosureArtifact, Digest, EncodedArtifact, OpaqueBytes, Ref, Refusal, Utf8Text};
use aether_data::{Cites, Storage};

use crate::kinds::Detail;

/// The only environment mode this slice ships: read the injected closure, stage artifacts.
pub struct Pure;

/// Sandbox parameterized by mode. Built from an [`aether_bloomery_kinds::Invoke`] closure.
pub struct Env<M> {
    closure: BTreeMap<Digest, ClosureArtifact>,
    staged: Vec<EncodedArtifact>,
    _mode: PhantomData<M>,
}

impl Env<Pure> {
    pub(crate) fn from_closure(closure: Vec<ClosureArtifact>) -> Self {
        let mut artifacts = BTreeMap::new();
        for artifact in closure {
            artifacts.insert(artifact.digest(), artifact);
        }
        Self { closure: artifacts, staged: Vec::new(), _mode: PhantomData }
    }

    /// Load `r` from the injected closure.
    ///
    /// # Errors
    ///
    /// [`Refusal::InputMissing`] when the digest is absent.
    /// [`Refusal::InputDecode`] when the kind prefix differs or the payload does not decode.
    pub fn read<K: Storage>(&self, r: Ref<K>) -> Result<K, Refusal> {
        let artifact = self.closure.get(&r.digest()).ok_or(Refusal::InputMissing)?;
        if artifact.kind() != K::ID {
            return Err(Refusal::InputDecode);
        }
        K::decode_storage(artifact.bytes()).map(|data| data.value).map_err(|_| Refusal::InputDecode)
    }

    /// Stage `payload` as [`OpaqueBytes`]. Identical payloads yield one artifact.
    pub fn stage_bytes(&mut self, payload: &[u8]) -> Ref<OpaqueBytes> {
        Ref::from_digest(self.record(EncodedArtifact::opaque_bytes(payload)))
    }

    /// Stage UTF-8 `text` as [`Utf8Text`]. Identical payloads yield one artifact.
    pub fn stage_text(&mut self, text: &str) -> Ref<Utf8Text> {
        Ref::from_digest(self.record(EncodedArtifact::text(text)))
    }

    /// Encode `value` and record it as a staged artifact.
    ///
    /// # Errors
    ///
    /// [`Refusal::Refused`] when encoding fails.
    pub fn stage_encoded<K: Storage + Clone + Cites>(&mut self, value: &K) -> Result<Ref<K>, Refusal> {
        match EncodedArtifact::new(value) {
            Ok(encoded) => Ok(Ref::from_digest(self.record(encoded))),
            Err(error) => Err(Refusal::Refused { reason: Detail::new(format!("{error}")) }),
        }
    }

    pub(crate) fn staged(&self) -> &[EncodedArtifact] {
        &self.staged
    }

    pub(crate) fn into_staged(self) -> Vec<EncodedArtifact> {
        self.staged
    }

    fn record(&mut self, artifact: EncodedArtifact) -> Digest {
        let digest = artifact.digest();
        if self.staged.iter().all(|existing| existing.digest() != digest) {
            self.staged.push(artifact);
        }
        digest
    }
}
