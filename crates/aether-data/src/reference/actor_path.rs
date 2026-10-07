//! [`crate::ErasedActorPath`]: an unresolved, unproven, fully qualified actor
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
use core::iter;

use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::segment::{SegmentFault, check_segment};
use crate::hash::{MAX_SCOPE_PATH_BYTES, MAX_SCOPE_PATH_DEPTH, ScopePathError, fold_lineage};
use crate::ids::{ActorId, MailboxId};
use crate::schema::{LabelNode, SchemaType};
use crate::tagged_id::{Tag, with_tag};
use crate::wire::{Error as WireError, WireDecode, WireEncode};
use crate::{CastEligible, CrossesActors, CrossesWire, Schema};

/// The retired separator of the old short form, refused wherever it appears.
const RETIRED_SHORT_FORM: &str = "://";

/// A fully qualified actor address, canonical
/// (`test.trunk/test.leaf:probe`) or short
/// (`test.trunk/:probe`), valid by construction on every path in:
/// [`new`](Self::new), wire decode, and `Deserialize` all run the same check.
///
/// Equality and order are textual. A short path and its canonical expansion
/// are unequal values, because only the engine can tell that they name the
/// same actor.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ErasedActorPath(Box<str>);

/// How an [`ErasedActorPath`] is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActorPathForm<'a> {
    /// A canonical `/`-rendered lineage path: no step is a hole.
    Canonical(CanonicalPath<'a>),
    /// An ADR-0166 short path: at least one step is a hole, and the root is a
    /// bare namespace. `steps` are the steps written after the root.
    Short { root: &'a str, steps: Vec<PathSegment<'a>> },
}

/// A borrowed view of proven-canonical path text, handed out by
/// [`ErasedActorPath::form`]. Reaching the walk requires matching the
/// canonical arm, so a short path can never be folded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CanonicalPath<'a>(&'a str);

impl<'a> CanonicalPath<'a> {
    /// The proven-canonical text.
    #[must_use]
    pub fn text(&self) -> &'a str {
        self.0
    }

    /// The position the path names: the ADR-0099 §4 parse → fold, the
    /// inverse of the render. Each segment is one node, a bare namespace a
    /// singleton and `namespace:discriminator` an instance, folded root to
    /// leaf and tagged once at the end. A one-segment path is the depth-1
    /// fixed point.
    #[must_use]
    pub fn lineage_id(&self) -> MailboxId {
        let (root, beneath) = self.root_and_beneath();
        let folded = beneath.fold(node(root).0, |parent, segment| fold_lineage(parent, node(segment)));

        MailboxId(with_tag(Tag::Mailbox, folded))
    }

    /// The immediate parent: the text before the last `/` with the position
    /// that prefix names, or `None` for a root, which has no parent.
    #[must_use]
    pub fn parent(&self) -> Option<(&'a str, MailboxId)> {
        let (parent, _) = self.0.rsplit_once('/')?;

        Some((parent, Self(parent).lineage_id()))
    }

    /// Every prefix of the path that ends on a segment boundary, root first,
    /// each with the position it names: the path's ancestors in order, then
    /// the path itself. The fold carries the untagged hash from one segment
    /// to the next and tags a copy for each prefix, so the id of a prefix is
    /// the id that prefix has as a path of its own.
    pub fn ancestors(&self) -> impl Iterator<Item = (&'a str, MailboxId)> + 'a {
        let text = self.0;
        let (root, beneath) = self.root_and_beneath();
        let root_fold = node(root).0;
        // A separator precedes every segment beneath the root.
        let beneath = beneath.scan((root.len(), root_fold), move |(end, parent), segment| {
            *end += 1 + segment.len();
            *parent = fold_lineage(*parent, node(segment));

            Some((&text[..*end], *parent))
        });

        iter::once((root, root_fold))
            .chain(beneath)
            .map(|(prefix, folded)| (prefix, MailboxId(with_tag(Tag::Mailbox, folded))))
    }

    /// The root segment and the segments beneath it, in order. A canonical
    /// path always has a root, so the walk starts from a hash and never from
    /// "nothing folded yet".
    fn root_and_beneath(&self) -> (&'a str, impl Iterator<Item = &'a str> + 'a) {
        let (root, beneath) = self.0.split_once('/').unwrap_or((self.0, ""));

        (root, beneath.split_terminator('/'))
    }
}

/// The lineage node one canonical segment names: `namespace:discriminator` an
/// instance, a bare namespace a singleton.
fn node(segment: &str) -> ActorId {
    match segment.split_once(':') {
        Some((namespace, discriminator)) => ActorId::instanced(namespace, discriminator),
        None => ActorId::singleton(segment),
    }
}

/// One step of a short [`ErasedActorPath`] after its root, as written.
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

impl ErasedActorPath {
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

    /// The written text, exactly as it was validated.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
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
            ActorPathForm::Canonical(CanonicalPath(&self.0))
        }
    }
}

impl fmt::Display for ErasedActorPath {
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

/// [`ErasedActorPath::new`] rejection.
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
                 e.g. `aether.window/:main`",
            ),
            Self::ShortPathFromInstance => f.write_str(
                "invalid actor path: a short path must start at a root namespace, not an instance; start it at \
                 the root (e.g. `aether.window/:main`) or spell every step canonically",
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

impl Schema for ErasedActorPath {
    const SCHEMA: SchemaType = SchemaType::String;
    const LABEL: Option<&'static str> = None;
    const LABEL_NODE: LabelNode = LabelNode::Anonymous;
}

impl CrossesActors for ErasedActorPath {}
impl CrossesWire for ErasedActorPath {}

impl CastEligible for ErasedActorPath {
    const ELIGIBLE: bool = false;
}

impl WireEncode for ErasedActorPath {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        (*self.0).encode(out)
    }
}

impl<'de> WireDecode<'de> for ErasedActorPath {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        let text = String::decode(cursor)?;
        check_path(&text).map_err(|_| WireError::InvalidActorPath)?;
        Ok(Self(text.into()))
    }
}

impl Serialize for ErasedActorPath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ErasedActorPath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <Box<str>>::deserialize(deserializer)?;
        Self::new(&text).map_err(DeError::custom)
    }
}

#[cfg(test)]
mod tests {
    use alloc::format;
    use alloc::vec;
    use alloc::vec::Vec;

    use super::*;

    #[test]
    fn rejects_malformed_segments_in_either_form() {
        let segment = |index, fault| Err(ActorPathError::Segment { index, fault });
        assert_eq!(ErasedActorPath::new("a//b"), segment(1, SegmentFault::Empty));
        assert_eq!(ErasedActorPath::new("a/"), segment(1, SegmentFault::Empty));
        assert_eq!(ErasedActorPath::new("root/worker:bad:key"), segment(1, SegmentFault::ContainsSeparator));
        assert_eq!(ErasedActorPath::new("a:b/:c"), Err(ActorPathError::ShortPathFromInstance));
        assert_eq!(ErasedActorPath::new(":a/b"), Err(ActorPathError::ShortPathFromInstance));
        assert_eq!(ErasedActorPath::new("a b"), segment(0, SegmentFault::ContainsControlOrWhitespace));
        assert_eq!(ErasedActorPath::new(""), segment(0, SegmentFault::Empty));
        assert_eq!(ErasedActorPath::new("aether.component://camera"), Err(ActorPathError::RetiredShortForm));
    }

    #[test]
    fn rejects_paths_over_the_scope_caps() {
        let too_deep = Err(ActorPathError::Scope(ScopePathError::TooDeep { limit: MAX_SCOPE_PATH_DEPTH }));
        let canonical = |depth| vec!["seg"; depth].join("/");
        assert!(ErasedActorPath::new(&canonical(MAX_SCOPE_PATH_DEPTH)).is_ok());
        assert_eq!(ErasedActorPath::new(&canonical(MAX_SCOPE_PATH_DEPTH + 1)), too_deep);

        let short = |holes| format!("root/{}", vec![":seg"; holes].join("/"));
        assert!(ErasedActorPath::new(&short(MAX_SCOPE_PATH_DEPTH - 1)).is_ok());
        assert_eq!(ErasedActorPath::new(&short(MAX_SCOPE_PATH_DEPTH)), too_deep);

        assert_eq!(
            ErasedActorPath::new(&"a".repeat(MAX_SCOPE_PATH_BYTES + 1)),
            Err(ActorPathError::Scope(ScopePathError::TooLong { limit: MAX_SCOPE_PATH_BYTES }))
        );
    }

    #[test]
    fn form_reports_a_hole_path_as_short() {
        let short = ErasedActorPath::new("root/manager/:camera").expect("a short path");
        assert_eq!(
            short.form(),
            ActorPathForm::Short {
                root: "root",
                steps: vec![PathSegment::Bare("manager"), PathSegment::Hole { discriminator: "camera" }],
            }
        );

        let canonical = ErasedActorPath::new("root/manager/worker:camera").expect("a canonical path");
        assert_eq!(canonical.form(), ActorPathForm::Canonical(CanonicalPath("root/manager/worker:camera")));
    }

    #[test]
    fn canonical_view_parent_is_none_for_a_root_and_the_immediate_prefix_beneath() {
        let root = ErasedActorPath::new("root").expect("a canonical root");
        let ActorPathForm::Canonical(view) = root.form() else {
            panic!("a hole-free path is canonical");
        };
        assert_eq!(view.text(), "root");
        assert_eq!(view.parent(), None);

        let nested = ErasedActorPath::new("root/manager/worker:camera").expect("a canonical path");
        let ActorPathForm::Canonical(nested_view) = nested.form() else {
            panic!("a hole-free path is canonical");
        };
        let parent_text = "root/manager";
        let parent_path = ErasedActorPath::new(parent_text).expect("a canonical prefix");
        let ActorPathForm::Canonical(parent_view) = parent_path.form() else {
            panic!("a hole-free prefix is canonical");
        };
        assert_eq!(nested_view.parent(), Some((parent_text, parent_view.lineage_id())));

        let instanced = ErasedActorPath::new("root/scope:7").expect("an instanced-leaf path");
        let ActorPathForm::Canonical(instanced_view) = instanced.form() else {
            panic!("a hole-free path is canonical");
        };
        let root_prefix = ErasedActorPath::new("root").expect("a canonical prefix");
        let ActorPathForm::Canonical(root_view) = root_prefix.form() else {
            panic!("a hole-free prefix is canonical");
        };
        assert_eq!(instanced_view.parent(), Some(("root", root_view.lineage_id())));
    }

    #[test]
    fn canonical_view_ancestors_walk_root_first_with_each_prefix_fold() {
        let path = ErasedActorPath::new("root/manager/worker:camera").expect("a canonical path");
        let ActorPathForm::Canonical(view) = path.form() else {
            panic!("a hole-free path is canonical");
        };
        let prefixes = ["root", "root/manager", "root/manager/worker:camera"];
        let ancestors = view.ancestors().collect::<Vec<_>>();
        assert_eq!(ancestors.len(), prefixes.len(), "one step per written prefix");
        for (index, prefix) in prefixes.into_iter().enumerate() {
            let prefix_path = ErasedActorPath::new(prefix).expect("a canonical prefix");
            let ActorPathForm::Canonical(prefix_view) = prefix_path.form() else {
                panic!("a hole-free prefix is canonical");
            };
            assert_eq!(ancestors[index].0, prefix, "the walk stays root-first without skipping a level");
            assert_eq!(ancestors[index].1, prefix_view.lineage_id(), "each step folds its own prefix");
        }
    }

    #[test]
    fn decode_rejects_a_malformed_path() {
        let bytes = [4, 0, 0, 0, b'a', b'/', b'/', b'b'];
        let mut cursor: &[u8] = &bytes;
        assert_eq!(ErasedActorPath::decode(&mut cursor), Err(WireError::InvalidActorPath));
    }
}
