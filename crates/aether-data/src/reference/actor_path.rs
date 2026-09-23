//! [`crate::ActorPath`]: an unresolved, unproven, fully qualified actor
//! address in the ADR-0166 grammar.
//!
//! The text is checked when the value is built or decoded and is then stored
//! exactly as written. An abbreviation is never expanded here: expansion needs
//! the engine's linked root and child declarations, which a client process does
//! not share (ADR-0166 §5). The value claims nothing about existence or
//! placement, and it becomes a position only through the host registry's
//! `resolve_address` (ADR-0230 §3).

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::error::Error as StdError;
use core::fmt;

use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::segment::{SegmentFault, check_segment};
use crate::hash::{MAX_SCOPE_PATH_BYTES, MAX_SCOPE_PATH_DEPTH, ScopePathError};
use crate::schema::{LabelNode, SchemaType};
use crate::wire::{Error as WireError, WireDecode, WireEncode};
use crate::{CastEligible, Schema};

/// The separator between an abbreviated address's root and its relative path.
const ABBREVIATION: &str = "://";

/// A fully qualified actor address, canonical
/// (`aether.component/aether.embedded:probe`) or abbreviated
/// (`aether.component://probe`), valid by construction on every path in:
/// [`new`](Self::new), wire decode, and `Deserialize` all run the same check.
///
/// Equality is textual. An abbreviation and its canonical expansion are
/// unequal values, because only the engine can tell that they name the same
/// actor.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ActorPath(Box<str>);

/// How an [`ActorPath`] is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActorPathForm<'a> {
    /// A canonical `/`-rendered lineage path.
    Canonical(&'a str),
    /// An ADR-0166 abbreviation: a bare root namespace and the relative
    /// segments written after `://`, which may be none.
    Abbreviated { root: &'a str, relative: Vec<PathSegment<'a>> },
}

/// One relative segment of an abbreviated [`ActorPath`], as written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathSegment<'a> {
    /// A segment with no `:`. The registry decides whether it names a
    /// singleton child or elides an instanced child's namespace.
    Bare(&'a str),
    /// An explicit instanced child, `namespace:discriminator`.
    Qualified { namespace: &'a str, discriminator: &'a str },
}

impl ActorPath {
    /// Validate `text` against the ADR-0166 address grammar.
    ///
    /// The whole text is at most [`MAX_SCOPE_PATH_BYTES`] bytes. Text holding
    /// `://` is abbreviated: its root is one bare segment and the rest is
    /// empty or `/`-separated relative segments. Other text is canonical:
    /// `/`-separated segments, at least one. The depth, counting an
    /// abbreviation's root, is at most [`MAX_SCOPE_PATH_DEPTH`]. Every segment
    /// is `namespace` or `namespace:discriminator`, and each part follows the
    /// namespace-segment grammar.
    ///
    /// # Errors
    ///
    /// Returns [`ActorPathError`] naming the breached cap, or the written
    /// segment and the rule it broke.
    pub fn new(text: &str) -> Result<Self, ActorPathError> {
        check_path(text)?;
        Ok(Self(text.into()))
    }

    /// The parsed view of the written text. Infallible, because the text was
    /// validated on the way in.
    #[must_use]
    pub fn form(&self) -> ActorPathForm<'_> {
        match self.0.split_once(ABBREVIATION) {
            None => ActorPathForm::Canonical(&self.0),
            Some((root, relative)) => {
                ActorPathForm::Abbreviated { root, relative: relative_segments(relative).map(parse_segment).collect() }
            }
        }
    }
}

impl fmt::Display for ActorPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for PathSegment<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bare(segment) => f.write_str(segment),
            Self::Qualified { namespace, discriminator } => write!(f, "{namespace}:{discriminator}"),
        }
    }
}

/// [`ActorPath::new`] rejection.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ActorPathError {
    /// The written segment at `index` (0-based; an abbreviation's root is 0)
    /// broke `fault`.
    Segment { index: usize, fault: SegmentFault },
    /// The path breaches the depth or byte cap.
    Scope(ScopePathError),
}

impl fmt::Display for ActorPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Segment { index, fault } => write!(f, "invalid actor path: segment {index}: {}", fault.message()),
            Self::Scope(ScopePathError::TooDeep { limit }) => {
                write!(f, "invalid actor path: more than {limit} segments")
            }
            Self::Scope(ScopePathError::TooLong { limit }) => {
                write!(f, "invalid actor path: exceeds the {limit}-byte path limit")
            }
        }
    }
}

impl StdError for ActorPathError {}

fn check_path(text: &str) -> Result<(), ActorPathError> {
    if text.len() > MAX_SCOPE_PATH_BYTES {
        return Err(ActorPathError::Scope(ScopePathError::TooLong { limit: MAX_SCOPE_PATH_BYTES }));
    }
    let abbreviated = text.split_once(ABBREVIATION);
    let depth = match abbreviated {
        Some((_, relative)) => relative_segments(relative).count() + 1,
        None => text.split('/').count(),
    };
    if depth > MAX_SCOPE_PATH_DEPTH {
        return Err(ActorPathError::Scope(ScopePathError::TooDeep { limit: MAX_SCOPE_PATH_DEPTH }));
    }
    match abbreviated {
        Some((root, relative)) => check_segment(root.as_bytes())
            .map_err(|fault| ActorPathError::Segment { index: 0, fault })
            .and_then(|()| check_segments(relative_segments(relative), 1)),
        None => check_segments(text.split('/'), 0),
    }
}

/// Check each written segment, numbering them from `first`.
fn check_segments<'a>(segments: impl Iterator<Item = &'a str>, first: usize) -> Result<(), ActorPathError> {
    segments.enumerate().try_for_each(|(offset, segment)| {
        let parts = match parse_segment(segment) {
            PathSegment::Bare(segment) => check_segment(segment.as_bytes()),
            PathSegment::Qualified { namespace, discriminator } => {
                check_segment(namespace.as_bytes()).and_then(|()| check_segment(discriminator.as_bytes()))
            }
        };
        parts.map_err(|fault| ActorPathError::Segment { index: first + offset, fault })
    })
}

/// The relative segments after `://`: none when the relative path is empty.
fn relative_segments(relative: &str) -> impl Iterator<Item = &str> {
    relative.split('/').filter(move |_| !relative.is_empty())
}

/// Split a written segment at its first `:`. A second `:` stays in the
/// discriminator, where the segment check rejects it as a separator.
fn parse_segment(segment: &str) -> PathSegment<'_> {
    match segment.split_once(':') {
        None => PathSegment::Bare(segment),
        Some((namespace, discriminator)) => PathSegment::Qualified { namespace, discriminator },
    }
}

impl Schema for ActorPath {
    const SCHEMA: SchemaType = SchemaType::String;
    const LABEL: Option<&'static str> = None;
    const LABEL_NODE: LabelNode = LabelNode::Anonymous;
}

impl CastEligible for ActorPath {
    const ELIGIBLE: bool = false;
}

impl WireEncode for ActorPath {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        (*self.0).encode(out)
    }
}

impl<'de> WireDecode<'de> for ActorPath {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        let text = String::decode(cursor)?;
        check_path(&text).map_err(|_| WireError::InvalidActorPath)?;
        Ok(Self(text.into()))
    }
}

impl Serialize for ActorPath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ActorPath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <Box<str>>::deserialize(deserializer)?;
        Self::new(&text).map_err(DeError::custom)
    }
}

#[cfg(test)]
mod tests {
    use alloc::format;
    use alloc::vec;

    use super::*;

    #[test]
    fn rejects_malformed_segments_in_either_form() {
        let segment = |index, fault| Err(ActorPathError::Segment { index, fault });
        assert_eq!(ActorPath::new("a//b"), segment(1, SegmentFault::Empty));
        assert_eq!(ActorPath::new("a/"), segment(1, SegmentFault::Empty));
        assert_eq!(ActorPath::new("root://worker:bad:key"), segment(1, SegmentFault::ContainsSeparator));
        assert_eq!(ActorPath::new("a:b://c"), segment(0, SegmentFault::ContainsSeparator));
        assert_eq!(ActorPath::new("a b"), segment(0, SegmentFault::ContainsControlOrWhitespace));
        assert_eq!(ActorPath::new(""), segment(0, SegmentFault::Empty));
    }

    #[test]
    fn rejects_paths_over_the_scope_caps() {
        let too_deep = Err(ActorPathError::Scope(ScopePathError::TooDeep { limit: MAX_SCOPE_PATH_DEPTH }));
        let canonical = |depth| vec!["seg"; depth].join("/");
        assert!(ActorPath::new(&canonical(MAX_SCOPE_PATH_DEPTH)).is_ok());
        assert_eq!(ActorPath::new(&canonical(MAX_SCOPE_PATH_DEPTH + 1)), too_deep);

        let abbreviated = |relative| format!("root://{}", vec!["seg"; relative].join("/"));
        assert!(ActorPath::new(&abbreviated(MAX_SCOPE_PATH_DEPTH - 1)).is_ok());
        assert_eq!(ActorPath::new(&abbreviated(MAX_SCOPE_PATH_DEPTH)), too_deep);

        assert_eq!(
            ActorPath::new(&"a".repeat(MAX_SCOPE_PATH_BYTES + 1)),
            Err(ActorPathError::Scope(ScopePathError::TooLong { limit: MAX_SCOPE_PATH_BYTES }))
        );
    }

    #[test]
    fn decode_rejects_a_malformed_path() {
        let bytes = [4, 0, 0, 0, b'a', b'/', b'/', b'b'];
        let mut cursor: &[u8] = &bytes;
        assert_eq!(ActorPath::decode(&mut cursor), Err(WireError::InvalidActorPath));
    }
}
