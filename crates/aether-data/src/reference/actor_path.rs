//! [`crate::ActorPath`]: an unresolved, unproven, fully qualified actor
//! address in the ADR-0166 grammar.
//!
//! The text is checked when the value is built or decoded and is then stored
//! exactly as written. A short path's holes are never filled here: expansion
//! needs the engine's linked root and child declarations, which a client
//! process does not share (ADR-0166 §5). The value claims nothing about
//! existence or placement, and it becomes a position only through the host
//! registry's `resolve_address` (ADR-0230 §3).

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

/// The retired separator of the old short form, refused wherever it appears.
const RETIRED_SHORT_FORM: &str = "://";

/// A fully qualified actor address, canonical
/// (`aether.component/aether.embedded:probe`) or short
/// (`aether.component/:probe`), valid by construction on every path in:
/// [`new`](Self::new), wire decode, and `Deserialize` all run the same check.
///
/// Equality is textual. A short path and its canonical expansion are unequal
/// values, because only the engine can tell that they name the same actor.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ActorPath(Box<str>);

/// How an [`ActorPath`] is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActorPathForm<'a> {
    /// A canonical `/`-rendered lineage path: no step is a hole.
    Canonical(&'a str),
    /// An ADR-0166 short path: at least one step is a hole, and the root is a
    /// bare namespace. `steps` are the steps written after the root.
    Short { root: &'a str, steps: Vec<PathSegment<'a>> },
}

/// One step of a short [`ActorPath`] after its root, as written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathSegment<'a> {
    /// A step with no `:`: always a singleton child.
    Bare(&'a str),
    /// An explicit instanced child, `namespace:discriminator`.
    Qualified { namespace: &'a str, discriminator: &'a str },
    /// A hole, `:discriminator`: an instance of the one instanced child
    /// declared under the current actor.
    Hole { discriminator: &'a str },
}

impl ActorPath {
    /// Validate `text` against the ADR-0166 address grammar.
    ///
    /// The whole text is at most [`MAX_SCOPE_PATH_BYTES`] bytes and never
    /// holds `://`. It is `/`-separated steps, at least one and at most
    /// [`MAX_SCOPE_PATH_DEPTH`]. The first step is `namespace` or
    /// `namespace:discriminator`; each later step is `namespace`,
    /// `namespace:discriminator`, or a hole `:discriminator`. A path with a
    /// hole is short, and its first step must be a bare namespace. Each part
    /// follows the namespace-segment grammar.
    ///
    /// # Errors
    ///
    /// Returns [`ActorPathError`] naming the breached cap, the retired `://`
    /// form, a short path whose first step is an instance, or the written
    /// step and the rule it broke.
    pub fn new(text: &str) -> Result<Self, ActorPathError> {
        check_path(text)?;
        Ok(Self(text.into()))
    }

    /// The parsed view of the written text. Infallible, because the text was
    /// validated on the way in.
    #[must_use]
    pub fn form(&self) -> ActorPathForm<'_> {
        let mut steps = self.0.split('/');
        let root = steps.next().unwrap_or_default();
        let steps: Vec<_> = steps.map(parse_segment).collect();
        if steps.iter().any(|step| matches!(step, PathSegment::Hole { .. })) {
            ActorPathForm::Short { root, steps }
        } else {
            ActorPathForm::Canonical(&self.0)
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
            Self::Hole { discriminator } => write!(f, ":{discriminator}"),
        }
    }
}

/// [`ActorPath::new`] rejection.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ActorPathError {
    /// The written step at `index` (0-based; the root is 0) broke `fault`.
    Segment { index: usize, fault: SegmentFault },
    /// The path breaches the depth or byte cap.
    Scope(ScopePathError),
    /// The text holds `://`, the retired short form. A short path names the
    /// one instanced child with a hole instead.
    RetiredShortForm,
    /// The path has a hole but its first step is an instance, qualified
    /// (`swarm:3/:x`) or itself a hole (`:a/b`). A short path is expanded
    /// from a root's declarations, so it must start at a bare root namespace.
    ShortPathFromInstance,
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
            Self::RetiredShortForm => f.write_str(
                "invalid actor path: `://` was removed; name the one instanced child with a hole, \
                 e.g. `aether.component/:camera`",
            ),
            Self::ShortPathFromInstance => f.write_str(
                "invalid actor path: a short path must start at a root namespace, not an instance; start it at \
                 the root (e.g. `aether.component/:camera`) or spell every step canonically",
            ),
        }
    }
}

impl StdError for ActorPathError {}

fn check_path(text: &str) -> Result<(), ActorPathError> {
    if text.len() > MAX_SCOPE_PATH_BYTES {
        return Err(ActorPathError::Scope(ScopePathError::TooLong { limit: MAX_SCOPE_PATH_BYTES }));
    }
    if text.contains(RETIRED_SHORT_FORM) {
        return Err(ActorPathError::RetiredShortForm);
    }
    let steps: Vec<_> = text.split('/').map(parse_segment).collect();
    if steps.len() > MAX_SCOPE_PATH_DEPTH {
        return Err(ActorPathError::Scope(ScopePathError::TooDeep { limit: MAX_SCOPE_PATH_DEPTH }));
    }

    let short = steps.iter().any(|step| matches!(step, PathSegment::Hole { .. }));
    if short && !matches!(steps.first(), Some(PathSegment::Bare(_))) {
        return Err(ActorPathError::ShortPathFromInstance);
    }

    steps.into_iter().enumerate().try_for_each(|(index, step)| {
        let parts = match step {
            PathSegment::Bare(namespace) => check_segment(namespace.as_bytes()),
            PathSegment::Qualified { namespace, discriminator } => {
                check_segment(namespace.as_bytes()).and_then(|()| check_segment(discriminator.as_bytes()))
            }
            PathSegment::Hole { discriminator } => check_segment(discriminator.as_bytes()),
        };
        parts.map_err(|fault| ActorPathError::Segment { index, fault })
    })
}

/// Split a written step at its first `:`. An empty namespace is a hole. A
/// second `:` stays in the discriminator, where the segment check rejects it
/// as a separator.
fn parse_segment(segment: &str) -> PathSegment<'_> {
    match segment.split_once(':') {
        None => PathSegment::Bare(segment),
        Some(("", discriminator)) => PathSegment::Hole { discriminator },
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
        assert_eq!(ActorPath::new("root/worker:bad:key"), segment(1, SegmentFault::ContainsSeparator));
        assert_eq!(ActorPath::new("a:b/:c"), Err(ActorPathError::ShortPathFromInstance));
        assert_eq!(ActorPath::new(":a/b"), Err(ActorPathError::ShortPathFromInstance));
        assert_eq!(ActorPath::new("a b"), segment(0, SegmentFault::ContainsControlOrWhitespace));
        assert_eq!(ActorPath::new(""), segment(0, SegmentFault::Empty));
        assert_eq!(ActorPath::new("aether.component://camera"), Err(ActorPathError::RetiredShortForm));
    }

    #[test]
    fn rejects_paths_over_the_scope_caps() {
        let too_deep = Err(ActorPathError::Scope(ScopePathError::TooDeep { limit: MAX_SCOPE_PATH_DEPTH }));
        let canonical = |depth| vec!["seg"; depth].join("/");
        assert!(ActorPath::new(&canonical(MAX_SCOPE_PATH_DEPTH)).is_ok());
        assert_eq!(ActorPath::new(&canonical(MAX_SCOPE_PATH_DEPTH + 1)), too_deep);

        let short = |holes| format!("root/{}", vec![":seg"; holes].join("/"));
        assert!(ActorPath::new(&short(MAX_SCOPE_PATH_DEPTH - 1)).is_ok());
        assert_eq!(ActorPath::new(&short(MAX_SCOPE_PATH_DEPTH)), too_deep);

        assert_eq!(
            ActorPath::new(&"a".repeat(MAX_SCOPE_PATH_BYTES + 1)),
            Err(ActorPathError::Scope(ScopePathError::TooLong { limit: MAX_SCOPE_PATH_BYTES }))
        );
    }

    #[test]
    fn form_reports_a_hole_path_as_short() {
        let short = ActorPath::new("root/manager/:camera").expect("a short path");
        assert_eq!(
            short.form(),
            ActorPathForm::Short {
                root: "root",
                steps: vec![PathSegment::Bare("manager"), PathSegment::Hole { discriminator: "camera" }],
            }
        );

        let canonical = ActorPath::new("root/manager/worker:camera").expect("a canonical path");
        assert_eq!(canonical.form(), ActorPathForm::Canonical("root/manager/worker:camera"));
    }

    #[test]
    fn decode_rejects_a_malformed_path() {
        let bytes = [4, 0, 0, 0, b'a', b'/', b'/', b'b'];
        let mut cursor: &[u8] = &bytes;
        assert_eq!(ActorPath::decode(&mut cursor), Err(WireError::InvalidActorPath));
    }
}
