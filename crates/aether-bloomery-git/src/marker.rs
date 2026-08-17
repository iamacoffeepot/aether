//! The stable-metadata marker (#3459 step 3).
//!
//! Idempotency and rebuildability ride a marker carrying the internal
//! Bloomery **key** (the stable id the projection is found by) plus a
//! **content digest** (what the projection is compared against to decide
//! create / update / no-op). In issue and comment bodies the marker is an
//! HTML comment — the repo's established convention (`<!-- issue-label-bot -->`
//! and the pipeline's park markers are the precedent) — so it is invisible in
//! the rendered page and survives edits *around* it. On a check-run the same
//! pair rides the native `external_id` field.
//!
//! The key is hex-encoded in the wire form so an arbitrary [`WorkpieceId`]
//! string (which may contain spaces, quotes, or a stray `-->`) can never
//! break the delimiter or inject a second marker. The parser reads only a
//! well-formed marker it rendered itself; free-form body text around it is
//! never interpreted.
//!
//! [`WorkpieceId`]: aether_bloomery::WorkpieceId

use aether_bloomery::Digest;

/// The internal metadata a projection embeds: the stable find-by key and the
/// content digest the reconcile compares.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Marker {
    /// The stable projection key — found by exact match, never changes for a
    /// given projected object.
    pub key: String,
    /// The content digest of the desired projection; a mismatch drives an
    /// update, an exact match a no-op.
    pub digest: Digest,
}

const OPEN: &str = "<!-- bloomery id=";
const MID: &str = " digest=";
const CLOSE: &str = " -->";

/// Render `marker` as an HTML comment for embedding in an issue/comment body.
#[must_use]
pub fn render_marker(marker: &Marker) -> String {
    format!("{OPEN}{}{MID}{}{CLOSE}", to_hex(marker.key.as_bytes()), digest_hex(&marker.digest))
}

/// Parse the first well-formed marker out of `body`, if present. A body with
/// no marker (a deleted-and-recreated object, or a hand-authored one) returns
/// `None`, which the reconcile reads as "recreate".
#[must_use]
pub fn parse_marker(body: &str) -> Option<Marker> {
    let start = body.find(OPEN)?;
    let rest = &body[start + OPEN.len()..];
    let mid = rest.find(MID)?;
    let key_hex = &rest[..mid];
    let after_mid = &rest[mid + MID.len()..];
    let end = after_mid.find(CLOSE)?;
    let digest_hex = &after_mid[..end];

    let key = String::from_utf8(from_hex(key_hex)?).ok()?;
    let digest = digest_from_hex(digest_hex)?;
    Some(Marker { key, digest })
}

/// The check-run `external_id` form of a marker: `<keyhex>@<digesthex>`. The
/// native field replaces the HTML comment — a check-run has no free-form body
/// a marker could hide in.
#[must_use]
pub fn check_run_external_id(marker: &Marker) -> String {
    format!("{}@{}", to_hex(marker.key.as_bytes()), digest_hex(&marker.digest))
}

/// Parse a check-run `external_id` back into a marker.
#[must_use]
pub fn parse_check_run_external_id(external_id: &str) -> Option<Marker> {
    let (key_hex, digest_hex) = external_id.split_once('@')?;
    let key = String::from_utf8(from_hex(key_hex)?).ok()?;
    let digest = digest_from_hex(digest_hex)?;
    Some(Marker { key, digest })
}

fn digest_hex(digest: &Digest) -> String {
    to_hex(digest.as_bytes())
}

fn digest_from_hex(hex: &str) -> Option<Digest> {
    let bytes = from_hex(hex)?;
    let arr: [u8; 32] = bytes.try_into().ok()?;
    Some(Digest::from_bytes(arr))
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

fn from_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let bytes = hex.as_bytes();
    let mut out = Vec::with_capacity(hex.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16)?;
        let lo = (bytes[i + 1] as char).to_digit(16)?;
        out.push(u8::try_from((hi << 4) | lo).ok()?);
        i += 2;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use aether_bloomery::Digest;
    use sha2::{Digest as _, Sha256};

    use super::{Marker, check_run_external_id, parse_check_run_external_id, parse_marker, render_marker};

    // A digest *computed* from spec bytes, not a literal — so the roundtrip
    // tripwire fails if the embed/parse hex logic drifts, rather than
    // mirroring a hand-written constant.
    fn digest_of_bytes(spec: &[u8]) -> Digest {
        let mut hasher = Sha256::new();
        hasher.update(spec);
        Digest::from_bytes(hasher.finalize().into())
    }

    #[test]
    fn marker_roundtrips_through_a_rendered_body() {
        // Tripwire: a marker rendered into a body and parsed back must recover
        // the exact key and computed digest — the idempotency key's stability.
        let marker = Marker { key: "wp:reactor-core@bloom-7".into(), digest: digest_of_bytes(b"scope-revision-bytes") };
        let body = format!("Some human-visible issue text.\n\n{}\n", render_marker(&marker));
        assert_eq!(parse_marker(&body), Some(marker));
    }

    #[test]
    fn body_edited_around_the_marker_still_parses() {
        let marker = Marker { key: "wp:x".into(), digest: digest_of_bytes(b"y") };
        let body = format!("prefix {} suffix — a reviewer typed here", render_marker(&marker));
        assert_eq!(parse_marker(&body), Some(marker));
    }

    #[test]
    fn a_key_carrying_marker_delimiters_cannot_break_the_marker() {
        // Hex-encoding the key means an adversarial workpiece string can never
        // inject a false `-->` or a second `id=`.
        let marker = Marker { key: "a --> b id= c".into(), digest: digest_of_bytes(b"z") };
        let body = render_marker(&marker);
        assert_eq!(parse_marker(&body), Some(marker));
    }

    #[test]
    fn no_marker_present_yields_none() {
        // The rebuild property's basis: a deleted-and-recreated object (or a
        // hand-authored one) has no marker, which the reconcile reads as
        // "recreate".
        assert_eq!(parse_marker("just a plain human comment, no marker"), None);
    }

    #[test]
    fn check_run_external_id_roundtrips() {
        let marker = Marker { key: "bloom:aggregate".into(), digest: digest_of_bytes(b"view") };
        assert_eq!(parse_check_run_external_id(&check_run_external_id(&marker)), Some(marker));
    }
}
