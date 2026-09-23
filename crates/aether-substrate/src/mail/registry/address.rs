use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::error::Error;
use std::fmt;

use aether_actor::{NamespaceError, validate_namespace_segment};
use aether_data::name_inventory::{
    ChildEntry, NameEntry, ParamKind, RootEntry, TemplateEntry, child_entries, name_entries, root_entries,
    template_entries,
};
use aether_data::{ActorId, MAILBOX_DOMAIN, MailboxId, PathSegment, ScopePathError, validate_scope_path};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedAddress {
    pub mailbox_id: MailboxId,
    pub canonical_path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActorAddressInventoryError {
    InvalidNamespace { namespace: String, reason: NamespaceError },
    InvalidActorTag { namespace: String, declared: ActorId, expected: ActorId },
}

impl fmt::Display for ActorAddressInventoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidNamespace { namespace, reason } => {
                write!(formatter, "actor namespace `{namespace}` is invalid: {reason:?}")
            }
            Self::InvalidActorTag { namespace, declared, expected } => {
                write!(formatter, "actor namespace `{namespace}` declares tag {declared:?}, expected {expected:?}")
            }
        }
    }
}

impl Error for ActorAddressInventoryError {}

/// Why a declared namespace carries no usable cardinality, and so is excluded
/// from the index alone (ADR-0166 §5). Both shapes mean the same thing to a
/// caller — the namespace is half-declared, so it anchors nothing — while
/// naming which half is wrong for whoever fixes the declaration.
///
/// Reachable only through a hand-written `inventory::submit!` of a `RootEntry`
/// / `ChildEntry` without the matching name entry; the derive cannot produce
/// either.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardinalityDefect {
    /// No singleton or dynamic-instanced name fact.
    Missing,
    /// Both a singleton and an instanced name fact.
    Contradictory,
}

impl fmt::Display for CardinalityDefect {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self {
            Self::Missing => "has no singleton or dynamic-instanced name fact",
            Self::Contradictory => "has both singleton and instanced name facts",
        };
        formatter.write_str(reason)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AddressResolutionError {
    InvalidInventory(ActorAddressInventoryError),
    UnknownRoot { root: String },
    InstancedRoot { root: String },
    HalfDeclaredRoot { root: String, defect: CardinalityDefect },
    IllegalSegment { parent: String, segment: String },
    AmbiguousSegment { parent: String, segment: String, candidates: Vec<String> },
    PathTooDeep { limit: usize },
    PathTooLong { limit: usize },
    NoLiveMailbox { canonical_path: String },
}

impl fmt::Display for AddressResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInventory(error) => write!(formatter, "actor address inventory is invalid: {error}"),
            Self::UnknownRoot { root } => write!(formatter, "unknown actor root `{root}`"),
            Self::InstancedRoot { root } => {
                write!(formatter, "actor root `{root}` is instanced, so it cannot root a short path")
            }
            Self::HalfDeclaredRoot { root, defect } => write!(
                formatter,
                "actor root `{root}` {defect}, so it cannot root a short path until its declaration is completed"
            ),
            Self::IllegalSegment { parent, segment } => {
                write!(formatter, "actor address segment `{segment}` is not legal beneath `{parent}`")
            }
            Self::AmbiguousSegment { parent, segment, candidates } => write!(
                formatter,
                "actor address segment `{segment}` is ambiguous beneath `{parent}`; use one of: {}",
                candidates.join(", ")
            ),
            Self::PathTooDeep { limit } => write!(formatter, "actor address exceeds the {limit}-segment path limit"),
            Self::PathTooLong { limit } => write!(formatter, "actor address exceeds the {limit}-byte path limit"),
            Self::NoLiveMailbox { canonical_path } => {
                write!(formatter, "canonical actor address `{canonical_path}` has no live mailbox")
            }
        }
    }
}

impl Error for AddressResolutionError {}

impl From<ActorAddressInventoryError> for AddressResolutionError {
    fn from(error: ActorAddressInventoryError) -> Self {
        Self::InvalidInventory(error)
    }
}

impl From<ScopePathError> for AddressResolutionError {
    fn from(error: ScopePathError) -> Self {
        match error {
            ScopePathError::TooDeep { limit } => Self::PathTooDeep { limit },
            ScopePathError::TooLong { limit } => Self::PathTooLong { limit },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Cardinality {
    Singleton,
    Instanced,
}

/// The declared children beneath one parent, keyed by namespace and split by
/// cardinality, so each step kind is one keyed lookup: a bare step reads
/// `singletons`, a `namespace:discriminator` step reads `instanced`, and a
/// hole reads `instanced`'s size. A namespace sits in at most one map, because
/// a namespace with contradicting cardinality facts is excluded at build.
struct Children {
    /// The parent's own namespace, which the `ActorId` key does not spell.
    parent_namespace: String,
    singletons: HashMap<String, ActorId>,
    instanced: HashMap<String, ActorId>,
}

impl Children {
    /// The instanced child namespaces, sorted: the candidates an ambiguous
    /// hole lists. Only error and gate paths read this.
    fn instanced_namespaces(&self) -> Vec<&str> {
        let mut namespaces = self.instanced.keys().map(String::as_str).collect::<Vec<_>>();
        namespaces.sort_unstable();
        namespaces
    }
}

pub(super) struct AddressIndex {
    roots: HashMap<String, ActorId>,
    /// Declared roots excluded from `roots` because their namespace is
    /// instanced (ADR-0166 §5). Retained so a short path rooted at one
    /// reports why it cannot anchor rather than reading as unknown.
    instanced_roots: BTreeSet<String>,
    /// Declared roots excluded from `roots` because their namespace carries no
    /// usable cardinality fact, with the defect that excluded each. Retained
    /// for the same reason as `instanced_roots`: a short path rooted at one
    /// reports the half-declaration rather than reading as unknown.
    half_declared_roots: BTreeMap<String, CardinalityDefect>,
    children: HashMap<ActorId, Children>,
}

/// Every parent in *this binary's* linked actor inventory beneath which a
/// hole is ambiguous (ADR-0166 §5), sorted by parent namespace.
///
/// The linkage is the point. A `child_of(...)` in one crate can collapse a
/// short path that another crate's callers depend on, and the collapse is
/// only visible to a binary that links both — so a gate over this reads the
/// same link-time facts the resolver does rather than scanning source
/// (iamacoffeepot/aether#4127).
///
/// # Errors
///
/// Returns [`ActorAddressInventoryError`] when a linked placement fact is
/// malformed — a namespace that is not a legal segment, or an actor tag
/// disagreeing with the namespace it claims. Those stay fatal to the whole
/// index, because such a fact cannot be trusted to name what it says it names.
pub fn ambiguous_holes() -> Result<Vec<AmbiguousHole>, ActorAddressInventoryError> {
    Ok(AddressIndex::from_inventory()?.ambiguous_holes())
}

/// A point in the linked declaration graph where a hole cannot be filled: more
/// than one instanced child namespace is declared beneath the same parent, so
/// `parent/:name` names no single child (ADR-0166 §5).
///
/// Ambiguity is a property of the declaration graph rather than of any one
/// actor, and a `child_of(...)` added in an unrelated crate can create it — so
/// the shape exists to be enumerated and gated, not only reported at the moment
/// an address fails to resolve (iamacoffeepot/aether#4127).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct AmbiguousHole {
    /// The parent whose holes cannot be filled.
    pub parent_namespace: String,
    /// The competing instanced child namespaces, sorted. A caller addressing
    /// this parent must name one of these explicitly as `namespace:discriminator`.
    pub child_namespaces: Vec<String>,
}

#[derive(Clone, Copy)]
struct RootFact<'a> {
    actor: ActorId,
    namespace: &'a str,
}

impl<'a> From<&'a RootEntry> for RootFact<'a> {
    fn from(entry: &'a RootEntry) -> Self {
        Self { actor: entry.actor, namespace: entry.namespace }
    }
}

#[derive(Clone, Copy)]
struct ChildFact<'a> {
    parent: ActorId,
    child: ActorId,
    parent_namespace: &'a str,
    child_namespace: &'a str,
}

impl<'a> From<&'a ChildEntry> for ChildFact<'a> {
    fn from(entry: &'a ChildEntry) -> Self {
        Self {
            parent: entry.parent,
            child: entry.child,
            parent_namespace: entry.parent_namespace,
            child_namespace: entry.child_namespace,
        }
    }
}

#[derive(Clone, Copy)]
struct CardinalityFact<'a> {
    namespace: &'a str,
    cardinality: Cardinality,
}

impl AddressIndex {
    pub(super) fn from_inventory() -> Result<Self, ActorAddressInventoryError> {
        let singleton_facts =
            name_entries().filter(|entry| entry.domain == MAILBOX_DOMAIN).map(|entry: &'static NameEntry| {
                CardinalityFact { namespace: entry.name, cardinality: Cardinality::Singleton }
            });
        let instanced_facts = template_entries()
            .filter(|entry| {
                entry.domain == MAILBOX_DOMAIN
                    && entry.template == ":{subname}"
                    && matches!(entry.param, ParamKind::Dynamic)
            })
            .map(|entry: &'static TemplateEntry| CardinalityFact {
                namespace: entry.prefix,
                cardinality: Cardinality::Instanced,
            });

        Self::build(
            root_entries().map(RootFact::from),
            child_entries().map(ChildFact::from),
            singleton_facts.chain(instanced_facts),
        )
    }

    fn build<'a>(
        root_facts: impl IntoIterator<Item = RootFact<'a>>,
        child_facts: impl IntoIterator<Item = ChildFact<'a>>,
        cardinality_facts: impl IntoIterator<Item = CardinalityFact<'a>>,
    ) -> Result<Self, ActorAddressInventoryError> {
        let root_facts = root_facts.into_iter().collect::<Vec<_>>();
        let child_facts = child_facts.into_iter().collect::<Vec<_>>();
        let mut cardinalities = BTreeMap::<&str, BTreeSet<Cardinality>>::new();
        for fact in cardinality_facts {
            cardinalities.entry(fact.namespace).or_default().insert(fact.cardinality);
        }

        let mut referenced_namespaces = BTreeSet::new();
        for fact in &root_facts {
            validate_actor_fact(fact.actor, fact.namespace)?;
            referenced_namespaces.insert(fact.namespace);
        }
        for fact in &child_facts {
            validate_actor_fact(fact.parent, fact.parent_namespace)?;
            validate_actor_fact(fact.child, fact.child_namespace)?;
            referenced_namespaces.insert(fact.parent_namespace);
            referenced_namespaces.insert(fact.child_namespace);
        }

        // A namespace whose placement fact carries no matching cardinality fact
        // — or two contradicting ones — is excluded from the index alone, on
        // the same argument the instanced-root exclusion below rests on:
        // rejecting the whole index would disable short paths
        // process-wide over one unrelated declaration, and the error would name
        // a namespace the caller was not addressing.
        let mut resolved_cardinalities = BTreeMap::new();
        let mut half_declared = BTreeMap::<&str, CardinalityDefect>::new();
        for namespace in referenced_namespaces {
            match cardinalities.get(namespace) {
                None => {
                    half_declared.insert(namespace, CardinalityDefect::Missing);
                }
                Some(values) if values.len() > 1 => {
                    half_declared.insert(namespace, CardinalityDefect::Contradictory);
                }
                Some(values) => {
                    resolved_cardinalities.insert(
                        namespace,
                        *values.first().expect("one cardinality remains after the empty and contradictory cases"),
                    );
                }
            }
        }
        for (namespace, defect) in &half_declared {
            tracing::warn!(
                namespace,
                %defect,
                "declared namespace carries no usable cardinality; excluded from the address index"
            );
        }

        let mut roots = HashMap::new();
        let mut instanced_roots = BTreeSet::new();
        let mut half_declared_roots = BTreeMap::new();
        for fact in root_facts {
            // Retain an excluded root's reason so a short path rooted at one
            // reports why it cannot anchor rather than reading as unknown.
            if let Some(defect) = half_declared.get(fact.namespace) {
                half_declared_roots.insert(fact.namespace.to_owned(), *defect);
                continue;
            }
            // ADR-0166 §5: a short path's root is the exact NAMESPACE of a
            // declared root, so an instanced namespace identifies no single
            // actor and cannot anchor one.
            if resolved_cardinalities[&fact.namespace] == Cardinality::Instanced {
                if instanced_roots.insert(fact.namespace.to_owned()) {
                    tracing::warn!(
                        namespace = fact.namespace,
                        "declared root is instanced; excluded as a short-path root"
                    );
                }
                continue;
            }
            roots.insert(fact.namespace.to_owned(), fact.actor);
        }

        // An edge touching an excluded namespace is dropped rather than walked:
        // the child's cardinality decides which step reaches it, so an edge
        // with no cardinality has no defined traversal. Sibling edges are unaffected.
        // Duplicate records of one logical edge collapse into the same key.
        let mut children = HashMap::<ActorId, Children>::new();
        for fact in child_facts {
            if half_declared.contains_key(fact.parent_namespace) {
                continue;
            }
            let Some(cardinality) = resolved_cardinalities.get(fact.child_namespace) else {
                continue;
            };
            let parent = children.entry(fact.parent).or_insert_with(|| Children {
                parent_namespace: fact.parent_namespace.to_owned(),
                singletons: HashMap::new(),
                instanced: HashMap::new(),
            });
            let table = match cardinality {
                Cardinality::Singleton => &mut parent.singletons,
                Cardinality::Instanced => &mut parent.instanced,
            };
            table.insert(fact.child_namespace.to_owned(), fact.child);
        }

        Ok(Self { roots, instanced_roots, half_declared_roots, children })
    }

    /// Every parent beneath which a hole is ambiguous, sorted by parent
    /// namespace. Reads the same resolved edges `expand_segment` walks,
    /// so what this reports and what a caller hits at resolution time cannot
    /// disagree.
    pub(super) fn ambiguous_holes(&self) -> Vec<AmbiguousHole> {
        // One instanced child fills a hole; none leaves nothing to fill it.
        let mut points = self
            .children
            .values()
            .filter(|children| children.instanced.len() >= 2)
            .map(|children| AmbiguousHole {
                parent_namespace: children.parent_namespace.clone(),
                child_namespaces: children.instanced_namespaces().into_iter().map(str::to_owned).collect(),
            })
            .collect::<Vec<_>>();
        points.sort();
        points
    }

    /// Expand a short path's parsed root and steps to its canonical path. The
    /// steps come from an `ActorPath`, so their grammar and the written caps
    /// already hold; the byte cap is rechecked as the path grows, because a
    /// hole expands into `namespace:discriminator`.
    pub(super) fn expand(&self, root: &str, steps: &[PathSegment<'_>]) -> Result<String, AddressResolutionError> {
        let mut current = *self.roots.get(root).ok_or_else(|| self.unanchored_root(root))?;
        let mut canonical_segments = vec![root.to_owned()];

        for segment in steps {
            let parent = canonical_segments.join("/");
            current = self.expand_segment(current, &parent, segment, &mut canonical_segments)?;
            validate_owned_scope_path(&canonical_segments)?;
        }

        validate_owned_scope_path(&canonical_segments)?;
        Ok(canonical_segments.join("/"))
    }

    /// Distinguish a prefix that names a declared root excluded at build —
    /// instanced, or half-declared — from one nothing declares at all. The
    /// three are the same miss in `roots` but different things to fix.
    fn unanchored_root(&self, root: &str) -> AddressResolutionError {
        if self.instanced_roots.contains(root) {
            AddressResolutionError::InstancedRoot { root: root.to_owned() }
        } else if let Some(defect) = self.half_declared_roots.get(root) {
            AddressResolutionError::HalfDeclaredRoot { root: root.to_owned(), defect: *defect }
        } else {
            AddressResolutionError::UnknownRoot { root: root.to_owned() }
        }
    }

    fn expand_segment(
        &self,
        current: ActorId,
        parent: &str,
        segment: &PathSegment<'_>,
        canonical_segments: &mut Vec<String>,
    ) -> Result<ActorId, AddressResolutionError> {
        let illegal =
            || AddressResolutionError::IllegalSegment { parent: parent.to_owned(), segment: segment.to_string() };
        let children = self.children.get(&current).ok_or_else(illegal)?;
        match *segment {
            PathSegment::Qualified { namespace, discriminator } => {
                let child = *children.instanced.get(namespace).ok_or_else(illegal)?;
                canonical_segments.push(format!("{namespace}:{discriminator}"));
                Ok(child)
            }
            PathSegment::Bare(namespace) => {
                let child = *children.singletons.get(namespace).ok_or_else(illegal)?;
                canonical_segments.push(namespace.to_owned());
                Ok(child)
            }
            PathSegment::Hole { discriminator } => match children.instanced.len() {
                0 => Err(illegal()),
                1 => {
                    let (namespace, child) = children.instanced.iter().next().ok_or_else(illegal)?;
                    canonical_segments.push(format!("{namespace}:{discriminator}"));
                    Ok(*child)
                }
                _ => Err(AddressResolutionError::AmbiguousSegment {
                    parent: parent.to_owned(),
                    segment: segment.to_string(),
                    candidates: children
                        .instanced_namespaces()
                        .into_iter()
                        .map(|namespace| format!("{namespace}:{discriminator}"))
                        .collect(),
                }),
            },
        }
    }
}

fn validate_actor_fact(actor: ActorId, namespace: &str) -> Result<(), ActorAddressInventoryError> {
    validate_address_part(namespace)
        .map_err(|reason| ActorAddressInventoryError::InvalidNamespace { namespace: namespace.to_owned(), reason })?;
    let expected = ActorId::singleton(namespace);
    if actor != expected {
        return Err(ActorAddressInventoryError::InvalidActorTag {
            namespace: namespace.to_owned(),
            declared: actor,
            expected,
        });
    }
    Ok(())
}

fn validate_address_part(value: &str) -> Result<(), NamespaceError> {
    validate_namespace_segment(value)?;
    if value.contains('/') {
        return Err(NamespaceError::ContainsSeparator);
    }
    Ok(())
}

fn validate_owned_scope_path(segments: &[String]) -> Result<(), AddressResolutionError> {
    validate_scope_path(&segments.iter().map(String::as_str).collect::<Vec<_>>()).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use aether_data::{ActorPath, ActorPathForm};

    use super::*;

    /// Parse `text` as a short `ActorPath` and expand it, as
    /// `Registry::resolve_address` does.
    fn expand(index: &AddressIndex, text: &str) -> Result<String, AddressResolutionError> {
        let path = ActorPath::new(text).expect("fixture is a well-formed actor path");
        let ActorPathForm::Short { root, steps } = path.form() else {
            panic!("fixture `{text}` is not a short path");
        };
        index.expand(root, &steps)
    }

    fn root(namespace: &str) -> RootFact<'_> {
        RootFact { actor: ActorId::singleton(namespace), namespace }
    }

    fn child<'a>(parent_namespace: &'a str, child_namespace: &'a str) -> ChildFact<'a> {
        ChildFact {
            parent: ActorId::singleton(parent_namespace),
            child: ActorId::singleton(child_namespace),
            parent_namespace,
            child_namespace,
        }
    }

    fn singleton(namespace: &str) -> CardinalityFact<'_> {
        CardinalityFact { namespace, cardinality: Cardinality::Singleton }
    }

    fn instanced(namespace: &str) -> CardinalityFact<'_> {
        CardinalityFact { namespace, cardinality: Cardinality::Instanced }
    }

    #[test]
    fn expands_singleton_and_unique_instanced_children_iteratively() {
        let index = AddressIndex::build(
            [root("root")],
            [child("root", "manager"), child("manager", "worker")],
            [singleton("root"), singleton("manager"), instanced("worker")],
        )
        .expect("valid topology");

        assert_eq!(expand(&index, "root/manager/:camera"), Ok("root/manager/worker:camera".to_owned()));
    }

    #[test]
    fn branching_requires_an_explicit_instanced_namespace() {
        let index = AddressIndex::build(
            [root("root")],
            [child("root", "camera"), child("root", "microphone")],
            [singleton("root"), instanced("camera"), instanced("microphone")],
        )
        .expect("valid topology");

        assert_eq!(
            expand(&index, "root/:main"),
            Err(AddressResolutionError::AmbiguousSegment {
                parent: "root".to_owned(),
                segment: ":main".to_owned(),
                candidates: vec!["camera:main".to_owned(), "microphone:main".to_owned()],
            })
        );
    }

    #[test]
    fn duplicate_runtime_variants_do_not_create_false_ambiguity() {
        let index = AddressIndex::build(
            [root("root"), root("root")],
            [child("root", "worker"), child("root", "worker")],
            [singleton("root"), singleton("root"), instanced("worker"), instanced("worker")],
        )
        .expect("duplicate logical records deduplicate");

        assert_eq!(expand(&index, "root/:camera"), Ok("root/worker:camera".to_owned()));
    }

    #[test]
    fn diamond_topology_walks_the_declared_edge_for_each_parent() {
        let index = AddressIndex::build(
            [root("root")],
            [child("root", "left"), child("root", "right"), child("left", "leaf"), child("right", "leaf")],
            [singleton("root"), singleton("left"), singleton("right"), instanced("leaf")],
        )
        .expect("valid diamond");

        assert_eq!(expand(&index, "root/left/:one"), Ok("root/left/leaf:one".to_owned()));
        assert_eq!(expand(&index, "root/right/:one"), Ok("root/right/leaf:one".to_owned()));
    }

    #[test]
    fn a_bare_step_is_a_singleton_and_a_hole_is_an_instance() {
        let index = AddressIndex::build(
            [root("root")],
            [child("root", "status"), child("root", "worker")],
            [singleton("root"), singleton("status"), instanced("worker")],
        )
        .expect("valid topology");

        assert_eq!(expand(&index, "root/:status"), Ok("root/worker:status".to_owned()));
        assert_eq!(
            expand(&index, "root/camera/:x"),
            Err(AddressResolutionError::IllegalSegment { parent: "root".to_owned(), segment: "camera".to_owned() })
        );
        assert_eq!(
            expand(&index, "root/status/:x"),
            Err(AddressResolutionError::IllegalSegment { parent: "root/status".to_owned(), segment: ":x".to_owned() })
        );
    }

    #[test]
    fn an_unknown_root_is_reported_as_unknown() {
        let index =
            AddressIndex::build([root("root")], [child("root", "worker")], [singleton("root"), instanced("worker")])
                .expect("valid topology");

        assert_eq!(
            expand(&index, "missing/:one"),
            Err(AddressResolutionError::UnknownRoot { root: "missing".to_owned() })
        );
    }

    #[test]
    fn an_instanced_root_is_excluded_without_disabling_the_rest_of_the_index() {
        let index = AddressIndex::build(
            [root("root"), root("swarm")],
            [child("root", "worker"), child("swarm", "worker")],
            [singleton("root"), instanced("swarm"), instanced("worker")],
        )
        .expect("an instanced root excludes itself rather than failing the index");

        assert_eq!(expand(&index, "root/:camera"), Ok("root/worker:camera".to_owned()));
        assert_eq!(
            expand(&index, "swarm/:camera"),
            Err(AddressResolutionError::InstancedRoot { root: "swarm".to_owned() })
        );
        assert_eq!(
            expand(&index, "missing/:camera"),
            Err(AddressResolutionError::UnknownRoot { root: "missing".to_owned() })
        );
    }

    #[test]
    fn a_mistagged_namespace_still_rejects_the_whole_index() {
        // The two surviving inventory errors are declaration faults no
        // exclusion can localize: a namespace that does not validate, and an
        // actor tag that disagrees with the namespace it claims. Both mean the
        // fact itself cannot be trusted to name what it says it names, so
        // there is no "offending namespace" to exclude.
        assert!(matches!(
            AddressIndex::build([RootFact { actor: ActorId(7), namespace: "root" }], [], [singleton("root")]),
            Err(ActorAddressInventoryError::InvalidActorTag { .. })
        ));
    }

    #[test]
    fn a_half_declared_root_is_excluded_without_disabling_the_rest_of_the_index() {
        // #4138's blast radius, inverted: a half-declared namespace used to
        // return InvalidInventory for *every* short path, including
        // roots from other crates whose facts are entirely healthy.
        for (defect, cardinality_facts) in [
            (CardinalityDefect::Missing, vec![singleton("root"), instanced("worker")]),
            (
                CardinalityDefect::Contradictory,
                vec![singleton("root"), instanced("worker"), singleton("swarm"), instanced("swarm")],
            ),
        ] {
            let index =
                AddressIndex::build([root("root"), root("swarm")], [child("root", "worker")], cardinality_facts)
                    .expect("a half-declared namespace excludes itself rather than failing the index");

            assert_eq!(expand(&index, "root/:camera"), Ok("root/worker:camera".to_owned()));
            assert_eq!(
                expand(&index, "swarm/:camera"),
                Err(AddressResolutionError::HalfDeclaredRoot { root: "swarm".to_owned(), defect })
            );
            assert_eq!(
                expand(&index, "missing/:camera"),
                Err(AddressResolutionError::UnknownRoot { root: "missing".to_owned() })
            );
        }
    }

    #[test]
    fn only_the_edges_touching_a_half_declared_namespace_are_dropped() {
        // A child namespace with no cardinality fact has no defined step, so
        // its edge cannot be walked — but its siblings under the same parent
        // still resolve, and the parent itself still anchors.
        let index = AddressIndex::build(
            [root("root")],
            [child("root", "worker"), child("root", "status")],
            [singleton("root"), instanced("worker")],
        )
        .expect("a half-declared child excludes its own edge");

        assert_eq!(expand(&index, "root/:camera"), Ok("root/worker:camera".to_owned()));
        assert_eq!(
            expand(&index, "root/status/:x"),
            Err(AddressResolutionError::IllegalSegment { parent: "root".to_owned(), segment: "status".to_owned() }),
            "with `status` excluded, its edge is not walked"
        );
    }
}
