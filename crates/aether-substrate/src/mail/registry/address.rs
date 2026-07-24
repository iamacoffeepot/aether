use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::error::Error;
use std::fmt;

use aether_actor::{NamespaceError, validate_namespace_segment};
use aether_data::name_inventory::{
    ChildEntry, NameEntry, ParamKind, RootEntry, TemplateEntry, child_entries, name_entries, root_entries,
    template_entries,
};
use aether_data::{ActorId, MAILBOX_DOMAIN, MAX_SCOPE_PATH_DEPTH, MailboxId, ScopePathError, validate_scope_path};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedAddress {
    pub mailbox_id: MailboxId,
    pub canonical_path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActorAddressInventoryError {
    InvalidNamespace { namespace: String, reason: NamespaceError },
    InvalidActorTag { namespace: String, declared: ActorId, expected: ActorId },
    MissingCardinality { namespace: String },
    ContradictoryCardinality { namespace: String },
    InstancedRoot { namespace: String },
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
            Self::MissingCardinality { namespace } => {
                write!(formatter, "actor namespace `{namespace}` has no singleton or dynamic-instanced name fact")
            }
            Self::ContradictoryCardinality { namespace } => {
                write!(formatter, "actor namespace `{namespace}` has both singleton and instanced name facts")
            }
            Self::InstancedRoot { namespace } => {
                write!(formatter, "declared root namespace `{namespace}` is instanced and cannot be abbreviated")
            }
        }
    }
}

impl Error for ActorAddressInventoryError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AddressResolutionError {
    InvalidInventory(ActorAddressInventoryError),
    UnknownRoot { root: String },
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

#[derive(Clone, Debug)]
struct ChildNode {
    actor: ActorId,
    namespace: String,
    cardinality: Cardinality,
}

pub(super) struct AddressIndex {
    roots: HashMap<String, ActorId>,
    children: HashMap<ActorId, Vec<ChildNode>>,
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

        let mut resolved_cardinalities = BTreeMap::new();
        for namespace in referenced_namespaces {
            match cardinalities.get(namespace) {
                None => {
                    return Err(ActorAddressInventoryError::MissingCardinality { namespace: namespace.to_owned() });
                }
                Some(values) if values.len() > 1 => {
                    return Err(ActorAddressInventoryError::ContradictoryCardinality {
                        namespace: namespace.to_owned(),
                    });
                }
                Some(values) => {
                    resolved_cardinalities.insert(
                        namespace,
                        *values.first().expect("one cardinality remains after the empty and contradictory cases"),
                    );
                }
            }
        }

        let mut roots = HashMap::new();
        for fact in root_facts {
            if resolved_cardinalities[&fact.namespace] == Cardinality::Instanced {
                return Err(ActorAddressInventoryError::InstancedRoot { namespace: fact.namespace.to_owned() });
            }
            roots.insert(fact.namespace.to_owned(), fact.actor);
        }

        let mut logical_edges = BTreeSet::new();
        for fact in child_facts {
            logical_edges.insert((
                fact.parent,
                fact.child,
                fact.child_namespace,
                resolved_cardinalities[&fact.child_namespace],
            ));
        }

        let mut children = HashMap::<ActorId, Vec<ChildNode>>::new();
        for (parent, child, child_namespace, cardinality) in logical_edges {
            children.entry(parent).or_default().push(ChildNode {
                actor: child,
                namespace: child_namespace.to_owned(),
                cardinality,
            });
        }
        for nodes in children.values_mut() {
            nodes.sort_by(|left, right| left.namespace.cmp(&right.namespace).then(left.actor.cmp(&right.actor)));
        }

        Ok(Self { roots, children })
    }

    pub(super) fn expand(&self, root: &str, relative: &str) -> Result<String, AddressResolutionError> {
        validate_address_part(root).map_err(|_| AddressResolutionError::UnknownRoot { root: root.to_owned() })?;
        let mut current =
            *self.roots.get(root).ok_or_else(|| AddressResolutionError::UnknownRoot { root: root.to_owned() })?;
        let mut canonical_segments = vec![root.to_owned()];

        if !relative.is_empty() {
            let relative_segments = relative.split('/').collect::<Vec<_>>();
            if relative_segments.len() + 1 > MAX_SCOPE_PATH_DEPTH {
                return Err(AddressResolutionError::PathTooDeep { limit: MAX_SCOPE_PATH_DEPTH });
            }
            if root.len() + 1 + relative.len() > aether_data::MAX_SCOPE_PATH_BYTES {
                return Err(AddressResolutionError::PathTooLong { limit: aether_data::MAX_SCOPE_PATH_BYTES });
            }

            for segment in relative_segments {
                let parent = canonical_segments.join("/");
                current = self.expand_segment(current, &parent, segment, &mut canonical_segments)?;
                validate_owned_scope_path(&canonical_segments)?;
            }
        }

        validate_owned_scope_path(&canonical_segments)?;
        Ok(canonical_segments.join("/"))
    }

    fn expand_segment(
        &self,
        current: ActorId,
        parent: &str,
        segment: &str,
        canonical_segments: &mut Vec<String>,
    ) -> Result<ActorId, AddressResolutionError> {
        let children = self.children.get(&current).map(Vec::as_slice).unwrap_or_default();
        if let Some((namespace, discriminator)) = segment.split_once(':') {
            if validate_address_part(namespace).is_err() || validate_address_part(discriminator).is_err() {
                return Err(AddressResolutionError::IllegalSegment {
                    parent: parent.to_owned(),
                    segment: segment.to_owned(),
                });
            }
            let Some(child) = children
                .iter()
                .find(|child| child.namespace == namespace && child.cardinality == Cardinality::Instanced)
            else {
                return Err(AddressResolutionError::IllegalSegment {
                    parent: parent.to_owned(),
                    segment: segment.to_owned(),
                });
            };
            canonical_segments.push(format!("{}:{discriminator}", child.namespace));
            return Ok(child.actor);
        }

        if validate_address_part(segment).is_err() {
            return Err(AddressResolutionError::IllegalSegment {
                parent: parent.to_owned(),
                segment: segment.to_owned(),
            });
        }
        if let Some(child) =
            children.iter().find(|child| child.namespace == segment && child.cardinality == Cardinality::Singleton)
        {
            canonical_segments.push(child.namespace.clone());
            return Ok(child.actor);
        }

        let instanced = children.iter().filter(|child| child.cardinality == Cardinality::Instanced).collect::<Vec<_>>();
        match instanced.as_slice() {
            [] => {
                Err(AddressResolutionError::IllegalSegment { parent: parent.to_owned(), segment: segment.to_owned() })
            }
            [child] => {
                canonical_segments.push(format!("{}:{segment}", child.namespace));
                Ok(child.actor)
            }
            _ => Err(AddressResolutionError::AmbiguousSegment {
                parent: parent.to_owned(),
                segment: segment.to_owned(),
                candidates: instanced.iter().map(|child| format!("{}:{segment}", child.namespace)).collect(),
            }),
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
    use std::iter::repeat_n;

    use super::*;

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

        assert_eq!(index.expand("root", "manager/camera"), Ok("root/manager/worker:camera".to_owned()));
        assert_eq!(index.expand("root", "manager/worker:camera"), Ok("root/manager/worker:camera".to_owned()));
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
            index.expand("root", "main"),
            Err(AddressResolutionError::AmbiguousSegment {
                parent: "root".to_owned(),
                segment: "main".to_owned(),
                candidates: vec!["camera:main".to_owned(), "microphone:main".to_owned()],
            })
        );
        assert_eq!(index.expand("root", "camera:main"), Ok("root/camera:main".to_owned()));
    }

    #[test]
    fn duplicate_runtime_variants_do_not_create_false_ambiguity() {
        let index = AddressIndex::build(
            [root("root"), root("root")],
            [child("root", "worker"), child("root", "worker")],
            [singleton("root"), singleton("root"), instanced("worker"), instanced("worker")],
        )
        .expect("duplicate logical records deduplicate");

        assert_eq!(index.expand("root", "camera"), Ok("root/worker:camera".to_owned()));
    }

    #[test]
    fn diamond_topology_walks_the_declared_edge_for_each_parent() {
        let index = AddressIndex::build(
            [root("root")],
            [child("root", "left"), child("root", "right"), child("left", "leaf"), child("right", "leaf")],
            [singleton("root"), singleton("left"), singleton("right"), instanced("leaf")],
        )
        .expect("valid diamond");

        assert_eq!(index.expand("root", "left/one"), Ok("root/left/leaf:one".to_owned()));
        assert_eq!(index.expand("root", "right/one"), Ok("root/right/leaf:one".to_owned()));
    }

    #[test]
    fn singleton_exact_match_precedes_bare_instanced_elision() {
        let index = AddressIndex::build(
            [root("root")],
            [child("root", "status"), child("root", "worker")],
            [singleton("root"), singleton("status"), instanced("worker")],
        )
        .expect("valid topology");

        assert_eq!(index.expand("root", "status"), Ok("root/status".to_owned()));
        assert_eq!(index.expand("root", "worker:status"), Ok("root/worker:status".to_owned()));
    }

    #[test]
    fn malformed_unknown_and_bounded_paths_are_distinct() {
        let index =
            AddressIndex::build([root("root")], [child("root", "worker")], [singleton("root"), instanced("worker")])
                .expect("valid topology");

        assert_eq!(
            index.expand("missing", "one"),
            Err(AddressResolutionError::UnknownRoot { root: "missing".to_owned() })
        );
        assert!(matches!(index.expand("root", "worker:bad:key"), Err(AddressResolutionError::IllegalSegment { .. })));
        let too_deep = repeat_n("one", MAX_SCOPE_PATH_DEPTH).collect::<Vec<_>>().join("/");
        assert_eq!(
            index.expand("root", &too_deep),
            Err(AddressResolutionError::PathTooDeep { limit: MAX_SCOPE_PATH_DEPTH })
        );
        let too_long = "x".repeat(aether_data::MAX_SCOPE_PATH_BYTES);
        assert_eq!(
            index.expand("root", &too_long),
            Err(AddressResolutionError::PathTooLong { limit: aether_data::MAX_SCOPE_PATH_BYTES })
        );
    }

    #[test]
    fn invalid_or_contradictory_generated_metadata_is_rejected() {
        assert!(matches!(
            AddressIndex::build([RootFact { actor: ActorId(7), namespace: "root" }], [], [singleton("root")],),
            Err(ActorAddressInventoryError::InvalidActorTag { .. })
        ));
        assert!(matches!(
            AddressIndex::build([root("root")], [], [singleton("root"), instanced("root")],),
            Err(ActorAddressInventoryError::ContradictoryCardinality { .. })
        ));
        assert_eq!(
            AddressIndex::build([root("root")], [child("root", "worker")], [singleton("root")])
                .err()
                .expect("missing child cardinality"),
            ActorAddressInventoryError::MissingCardinality { namespace: "worker".to_owned() }
        );
    }
}
