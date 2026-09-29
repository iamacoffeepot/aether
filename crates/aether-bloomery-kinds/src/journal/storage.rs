//! The artifact storage protocol (ADR-0240 D7).

use crate::{ReadArtifact, ReadArtifactResult, ReadClosure, ReadClosureResult, Stage, StageResult};

/// A place artifacts are read from and staged to.
///
/// The journal owner (`aether.bloomery.journal`) is the one target: it covers
/// every row, and staging through it keeps the journal the only writer of its
/// root. A workspace request names its store as a `ProtocolPath` to this
/// protocol in its `source`, so the workspace reads and writes through
/// whichever journal the request came from without naming the journal crate.
///
/// `read_closure` reads a whole tree in one round trip when its closure fits
/// the reader's budget; `read` fetches one artifact, and `stage` stores
/// artifacts with no event and no head move.
#[aether_actor::protocol]
pub trait ArtifactStorage {
    /// Read one stored artifact.
    fn read(mail: ReadArtifact) -> ReadArtifactResult;
    /// Read an artifact's transitive closure under a byte limit.
    fn read_closure(mail: ReadClosure) -> ReadClosureResult;
    /// Store artifacts content-addressed, unfenced.
    fn stage(mail: Stage) -> StageResult;
}
