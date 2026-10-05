//! Reads every lane needs over [`Reads`]: a page of events, a wait on the
//! head, artifacts in batches, one decoded value, and a head's latest move.

use std::str;

use aether_bloomery_kinds::{
    ArtifactDigests, ClosureArtifact, ClosureLimit, JournalEntry, ReadArtifacts, ReadArtifactsResult, ReadEvents,
    ReadEventsResult, RecordedHead, RecordedHeadMove, WatchHead, WatchHeadResult,
};
use aether_codec::frame::max_frame_size;
use aether_data::{Digest, Storage};
use anyhow::{Context, Result, anyhow, bail};

use super::Reads;

/// The most entries one `read_events` may ask for: the journal's
/// `MAX_READ_EVENTS`.
const EVENTS_PER_PAGE: u32 = 128;

/// The journal head and the entries after `after`, at most one page of them.
///
/// # Errors
/// The journal refused the read or the transport failed.
pub fn page(reads: &mut impl Reads, after: u64) -> Result<(u64, Vec<JournalEntry>)> {
    match reads.read_events(ReadEvents { after, limit: EVENTS_PER_PAGE })? {
        ReadEventsResult::Ok { head, entries, .. } => Ok((head, entries)),
        ReadEventsResult::Err { message, .. } => bail!("reading the journal after {after}: {message}"),
    }
}

/// Block until the journal head passes `after`, and return the new head.
///
/// # Errors
/// The journal refused the watch, closed while it was parked, or the
/// transport failed.
pub fn wait_past(reads: &mut impl Reads, after: u64) -> Result<u64> {
    match reads.watch_head(WatchHead { after })? {
        WatchHeadResult::Advanced { head } => Ok(head),
        WatchHeadResult::Err { message } => bail!("watching the journal head past {after}: {message}"),
        WatchHeadResult::Ended => bail!("the journal closed while waiting past {after}"),
    }
}

/// Read every artifact `digests` names, in order, in as few
/// `read_artifacts` requests as the frame cap allows, and hand each to `each`
/// with the digest it was read under.
///
/// # Errors
/// An artifact is not stored, the journal refused a read, the transport
/// failed, or `each` did.
pub fn read_each(
    reads: &mut impl Reads,
    digests: &[Digest],
    mut each: impl FnMut(Digest, &ClosureArtifact) -> Result<()>,
) -> Result<()> {
    let limit_bytes = ClosureLimit::new(u64::try_from(max_frame_size() / 2)?)
        .map_err(|error| anyhow!("the frame cap does not make a read limit: {error}"))?;

    let mut rest = digests;
    while !rest.is_empty() {
        let asked = &rest[..rest.len().min(ReadArtifacts::MAX_ARTIFACTS)];
        let request = ReadArtifacts {
            digests: ArtifactDigests::new(asked.to_vec()).map_err(|error| anyhow!("the digests to read: {error}"))?,
            limit_bytes,
        };

        let artifacts = match reads.read_artifacts(&request)? {
            ReadArtifactsResult::Found { artifacts } if !artifacts.is_empty() => artifacts,
            ReadArtifactsResult::Found { .. } => {
                bail!("the journal answered a read of {} artifacts with none", asked.len())
            }
            ReadArtifactsResult::Missing { digest } => bail!("artifact {digest} is not stored"),
            ReadArtifactsResult::Err { message } => bail!("reading artifacts: {message}"),
        };
        for (digest, artifact) in asked.iter().zip(&artifacts) {
            each(*digest, artifact)?;
        }
        rest = &rest[artifacts.len().min(asked.len())..];
    }
    Ok(())
}

/// The payload of `artifact`, verified to hash to `digest`.
///
/// # Errors
/// The kind and payload do not hash to `digest`.
pub fn load(artifact: &ClosureArtifact, digest: Digest) -> Result<Vec<u8>> {
    Ok(artifact.load(digest)?)
}

/// Decode `artifact`, read under `digest`, as `K`.
///
/// # Errors
/// The artifact is not a `K`, does not hash to `digest`, or does not decode.
pub fn decode<K: Storage>(artifact: &ClosureArtifact, digest: Digest) -> Result<K> {
    if artifact.kind() != K::ID {
        bail!("artifact {digest} is not a {}", K::NAME);
    }
    K::decode_storage(&load(artifact, digest)?)
        .map(|data| data.value)
        .map_err(|error| anyhow!("decoding {digest} as {}: {error}", K::NAME))
}

/// Read the artifact `digest` names and decode it as `K`.
///
/// # Errors
/// The artifact is missing, is not a `K`, does not hash to `digest`, or does
/// not decode.
pub fn read_value<K: Storage>(reads: &mut impl Reads, digest: Digest) -> Result<K> {
    let mut value = None;
    read_each(reads, &[digest], |digest, artifact| {
        value = Some(decode(artifact, digest)?);
        Ok(())
    })?;
    value.with_context(|| format!("the journal answered no artifact {digest}"))
}

/// Decode `entry` as `K`, or `None` when it holds another kind.
///
/// # Errors
/// The entry holds a `K` that does not decode.
pub fn decode_entry<K: Storage>(entry: &JournalEntry) -> Result<Option<K>> {
    if entry.kind != K::ID {
        return Ok(None);
    }
    match entry.to_entry().decode::<K>() {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.is_unmatched() => Ok(None),
        Err(error) => Err(anyhow!("entry {} as {}: {error}", entry.seq, K::NAME)),
    }
}

/// The digest each of `heads` was last moved to, in the same order, or
/// `None` for a head no entry has moved; read through the whole journal as it
/// stands.
///
/// # Errors
/// A read failed or a head move did not decode.
pub fn latest_moves(reads: &mut impl Reads, heads: &[RecordedHead]) -> Result<Vec<Option<Digest>>> {
    let mut latest = vec![None; heads.len()];
    let mut after = 0;
    loop {
        let (head, entries) = page(reads, after)?;
        for entry in &entries {
            if let Some(moved) = decode_entry::<RecordedHeadMove>(entry)?
                && let Some(index) = heads.iter().position(|head| head == moved.head())
            {
                latest[index] = Some(moved.to());
            }
        }

        match entries.last() {
            Some(last) if last.seq < head => after = last.seq,
            _ => return Ok(latest),
        }
    }
}

/// A digest written as 64 lowercase hex digits, as every lane prints one.
///
/// # Errors
/// The text is not 64 hex digits.
pub fn parse_digest(text: &str) -> Result<Digest> {
    let refuse = || anyhow!("{text:?} is not a digest: 64 hex digits");
    if text.len() != 64 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(refuse());
    }

    let mut bytes = [0; 32];
    for (byte, pair) in bytes.iter_mut().zip(text.as_bytes().chunks_exact(2)) {
        *byte = u8::from_str_radix(str::from_utf8(pair)?, 16).map_err(|_| refuse())?;
    }
    Ok(Digest::from_bytes(bytes))
}
