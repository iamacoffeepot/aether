//! The one way to produce an execution.

use std::error::Error;
use std::fmt;
use std::marker::PhantomData;

use aether_bloomery_journal::{Batch, BatchError};
use aether_bloomery_kinds::{Digest, OpaqueBytes, Ref, Utf8Text};
use aether_data::{Cites, Storage, StorageError};

use crate::Program;

/// Accumulator of blobs staged for one execution. Event writes are not
/// reachable, which is how "executors never write events" is enforced by type.
pub struct Staging<P: Program> {
    batch: Batch,
    staged: Vec<Digest>,
    _program: PhantomData<fn() -> P>,
}

impl<P: Program> Staging<P> {
    /// Empty staging.
    #[must_use]
    pub fn new() -> Self {
        Self { batch: Batch::new(), staged: Vec::new(), _program: PhantomData }
    }

    /// Stage `payload` as [`OpaqueBytes`].
    pub fn stage_bytes(&mut self, payload: &[u8]) -> Ref<OpaqueBytes> {
        let staged = self.batch.stage_bytes(payload);
        self.record(staged.digest());
        staged
    }

    /// Stage UTF-8 `text` as [`Utf8Text`].
    pub fn stage_text(&mut self, text: &str) -> Ref<Utf8Text> {
        let staged = self.batch.stage_text(text);
        self.record(staged.digest());
        staged
    }

    /// Encode `value`, prefix `K::ID`, and walk it for citations.
    ///
    /// # Errors
    ///
    /// [`StorageError`] when encoding fails.
    pub fn stage_encoded<K: Storage + Clone + Cites>(&mut self, value: &K) -> Result<Ref<K>, StorageError> {
        let staged = self.batch.stage_encoded(value).map_err(storage)?;
        self.record(staged.digest());
        Ok(staged)
    }

    /// The only way to finish. Stages `result` as the root and refuses if any
    /// staged blob is unreachable from it.
    ///
    /// # Errors
    ///
    /// [`FinishError::Storage`] when encoding the result fails.
    /// [`FinishError::Orphaned`] when a staged blob is not reachable from the result.
    pub fn finish(mut self, result: P::Result) -> Result<Execution<P>, FinishError> {
        let owned = result;
        let result = self.stage_encoded(&owned)?;
        let reachable = reachable_from(&self.batch, result.digest());
        if let Some(digest) = self.staged.iter().copied().find(|digest| !reachable.contains(digest)) {
            return Err(FinishError::Orphaned { digest });
        }
        Ok(Execution { batch: self.batch, result })
    }

    fn record(&mut self, digest: Digest) {
        if !self.staged.contains(&digest) {
            self.staged.push(digest);
        }
    }
}

impl<P: Program> Default for Staging<P> {
    fn default() -> Self {
        Self::new()
    }
}

/// Staged blobs plus the result they are rooted at. No public constructor.
pub struct Execution<P: Program> {
    batch: Batch,
    result: Ref<P::Result>,
}

impl<P: Program> Execution<P> {
    /// The result this execution is rooted at.
    #[must_use]
    pub fn result(&self) -> Ref<P::Result> {
        self.result
    }

    pub(crate) fn into_erased(self) -> (Batch, Digest) {
        (self.batch, self.result.digest())
    }
}

/// Failure to finish a staging.
#[derive(Debug)]
pub enum FinishError {
    /// Encoding a staged value failed.
    Storage(StorageError),
    /// A blob was staged but is not reachable from the result.
    Orphaned {
        /// Digest of the unreachable blob.
        digest: Digest,
    },
}

impl fmt::Display for FinishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => write!(f, "failed to encode staged value: {error}"),
            Self::Orphaned { digest } => write!(f, "staged blob {digest} is not reachable from the result"),
        }
    }
}

impl Error for FinishError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Storage(error) => Some(error),
            Self::Orphaned { .. } => None,
        }
    }
}

impl From<StorageError> for FinishError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

fn storage(error: BatchError) -> StorageError {
    match error {
        BatchError::Storage(error) => error,
    }
}

fn reachable_from(batch: &Batch, root: Digest) -> Vec<Digest> {
    let mut seen = Vec::new();
    let mut stack = vec![root];
    while let Some(digest) = stack.pop() {
        if seen.contains(&digest) {
            continue;
        }
        seen.push(digest);
        let Some(citations) = batch.staged_citations(&digest) else {
            continue;
        };
        for citation in citations {
            let Ok(bytes) = <[u8; 32]>::try_from(citation.bytes.as_slice()) else {
                continue;
            };
            let child = Digest::from_bytes(bytes);
            if batch.staged_citations(&child).is_some() {
                stack.push(child);
            }
        }
    }
    seen
}
