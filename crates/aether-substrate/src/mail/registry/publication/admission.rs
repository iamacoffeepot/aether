//! The one static check a module publish runs (ADR-0241 §4).
//!
//! [`admit`] reads the candidate module's surface and each of its exported
//! namespaces' current holders, never a live instance. It runs these rules
//! in order and refuses the whole publish at the first failure:
//!
//! 1. **Namespace.** No exported namespace is native. Every module that holds
//!    one of the candidate's namespaces is its predecessor, and the candidate
//!    exports every namespace each predecessor exports: a namespace, once
//!    published, stays published (§3).
//! 2. **Contract growth.** Each exported namespace a predecessor holds keeps
//!    that predecessor's rows and fallback (ADR-0231 §5).
//! 3. **Private child types.** Each private child type a predecessor declares
//!    is still declared, privately or as an export, and keeps its rows and
//!    fallback, so an inline child's alias never advertises a row its code no
//!    longer handles.
//! 4. **Same hash.** A candidate whose hash already holds every one of its
//!    namespaces changes nothing (§9).
//!
//! The module's kinds register in the same owner batch, after this check.

use std::fmt;
use std::sync::Arc;

use aether_data::BlobHash;

use crate::actor::wasm::kind_manifest::ActorInputs;
use crate::actor::wasm::module::ModuleManifest;
use crate::mail::KindId;
use crate::mail::registry::{ContractBreak, RouteContract};

/// What a module publishes and declares, as admission compares it: each
/// exported namespace and each private child type with its contract.
pub struct ModuleSurface {
    exported: Vec<(Arc<str>, RouteContract)>,
    private: Vec<(Arc<str>, RouteContract)>,
}

impl ModuleSurface {
    /// The surface a module's manifest declares.
    pub fn of(manifest: &ModuleManifest) -> Self {
        Self::new(contracts(manifest.exported_groups()), contracts(manifest.private_groups()))
    }

    pub(super) fn new(exported: Vec<(Arc<str>, RouteContract)>, private: Vec<(Arc<str>, RouteContract)>) -> Self {
        Self { exported, private }
    }

    /// Every namespace the module exports.
    pub(super) fn exported_namespaces(&self) -> impl Iterator<Item = &Arc<str>> {
        self.exported.iter().map(|(namespace, _)| namespace)
    }

    fn exported_contract(&self, namespace: &str) -> Option<&RouteContract> {
        find(&self.exported, namespace)
    }

    /// The contract of a type the module declares, privately or as an export.
    fn declared_contract(&self, namespace: &str) -> Option<&RouteContract> {
        find(&self.private, namespace).or_else(|| self.exported_contract(namespace))
    }
}

fn contracts<'a>(groups: impl Iterator<Item = (&'a str, &'a ActorInputs)>) -> Vec<(Arc<str>, RouteContract)> {
    groups
        .map(|(namespace, group)| (Arc::from(namespace), RouteContract::from_capabilities(&group.capabilities)))
        .collect()
}

fn find<'a>(types: &'a [(Arc<str>, RouteContract)], namespace: &str) -> Option<&'a RouteContract> {
    types.iter().find(|(declared, _)| &**declared == namespace).map(|(_, contract)| contract)
}

/// Who holds a namespace, as admission reads it.
#[derive(Clone, Copy)]
pub enum Holder<'a> {
    /// A native actor linked into the binary.
    Native,
    /// A published module, by its content hash.
    Module { hash: BlobHash, surface: &'a ModuleSurface },
}

/// What an admitted publish does to the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admitted {
    /// The candidate's hash already holds every one of its namespaces.
    Unchanged,
    /// Every namespace the candidate exports now points at it.
    Publish,
}

/// Why admission refused a module publish: the first failing namespace and
/// the rule it failed. A narrowed contract names its first break, and the
/// broken row's kind by its registered name when the registry has one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmissionRefusal {
    /// Namespace rule: a native actor linked into this engine publishes it.
    NativeNamespace { namespace: Arc<str> },
    /// Namespace rule: a predecessor exports it and the candidate does not.
    DroppedNamespace { namespace: Arc<str> },
    /// Contract-growth rule: the candidate narrows an exported namespace's
    /// contract.
    ContractNarrowed { namespace: Arc<str>, contract_break: ContractBreak, kind_name: Option<Arc<str>> },
    /// Contract-growth rule: a predecessor declares this private child type
    /// and the candidate declares no type by its namespace.
    DroppedPrivateType { namespace: Arc<str> },
    /// Contract-growth rule: the candidate narrows a predecessor's private
    /// child type's contract.
    PrivateContractNarrowed { namespace: Arc<str>, contract_break: ContractBreak, kind_name: Option<Arc<str>> },
}

impl fmt::Display for AdmissionRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        const GROWTH: &str = "contract growth: rows only grow and a fallback is kept";
        match self {
            Self::NativeNamespace { namespace } => write!(
                formatter,
                "{namespace} is published by a native actor linked into this engine (namespace rule: \
                 no module publishes a native namespace)"
            ),
            Self::DroppedNamespace { namespace } => write!(
                formatter,
                "{namespace} is exported by this module's predecessor and not by the module (namespace rule: \
                 a published namespace stays published)"
            ),
            Self::ContractNarrowed { namespace, contract_break, kind_name } => {
                write!(formatter, "{namespace} {} ({GROWTH})", Narrowing(*contract_break, kind_name.as_deref()))
            }
            Self::DroppedPrivateType { namespace } => write!(
                formatter,
                "private child type {namespace} of this module's predecessor is not declared by the module \
                 (contract growth: a predecessor's private child type stays declared)"
            ),
            Self::PrivateContractNarrowed { namespace, contract_break, kind_name } => write!(
                formatter,
                "private child type {namespace} {} ({GROWTH})",
                Narrowing(*contract_break, kind_name.as_deref())
            ),
        }
    }
}

/// A contract break worded with the broken row's kind name, when known.
struct Narrowing<'a>(ContractBreak, Option<&'a str>);

impl fmt::Display for Narrowing<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Narrowing(ContractBreak::Row(_), Some(name)) => write!(formatter, "drops or changes its row for {name}"),
            Narrowing(contract_break, _) => contract_break.fmt(formatter),
        }
    }
}

/// Admit the module `hash` names, whose surface is `candidate`, against the
/// current holder of each namespace. `kind_name` names a broken row's kind
/// for the refusal. See the module docs for the rules.
pub fn admit<'a>(
    hash: BlobHash,
    candidate: &ModuleSurface,
    holder: impl Fn(&str) -> Option<Holder<'a>>,
    kind_name: impl Fn(KindId) -> Option<Arc<str>>,
) -> Result<Admitted, AdmissionRefusal> {
    let name_of = |contract_break| match contract_break {
        ContractBreak::Row(kind) => kind_name(kind),
        ContractBreak::Fallback => None,
    };

    let mut held = Vec::new();
    let mut predecessors = Vec::<(BlobHash, &ModuleSurface)>::new();
    for (namespace, contract) in &candidate.exported {
        match holder(namespace) {
            None => {}
            Some(Holder::Native) => return Err(AdmissionRefusal::NativeNamespace { namespace: Arc::clone(namespace) }),
            Some(Holder::Module { hash: predecessor, surface }) => {
                held.push((namespace, contract, surface));
                if !predecessors.iter().any(|(known, _)| *known == predecessor) {
                    predecessors.push((predecessor, surface));
                }
            }
        }
    }

    for (_, predecessor) in &predecessors {
        if let Some(dropped) =
            predecessor.exported_namespaces().find(|namespace| candidate.exported_contract(namespace).is_none())
        {
            return Err(AdmissionRefusal::DroppedNamespace { namespace: Arc::clone(dropped) });
        }
    }

    for (namespace, contract, predecessor) in held.iter().copied() {
        let published = predecessor.exported_contract(namespace).expect("a holder exports the namespace it holds");
        if let Some(contract_break) = published.first_break(contract) {
            return Err(AdmissionRefusal::ContractNarrowed {
                namespace: Arc::clone(namespace),
                contract_break,
                kind_name: name_of(contract_break),
            });
        }
    }

    for (_, predecessor) in &predecessors {
        for (namespace, declared) in &predecessor.private {
            let Some(successor) = candidate.declared_contract(namespace) else {
                return Err(AdmissionRefusal::DroppedPrivateType { namespace: Arc::clone(namespace) });
            };
            if let Some(contract_break) = declared.first_break(successor) {
                return Err(AdmissionRefusal::PrivateContractNarrowed {
                    namespace: Arc::clone(namespace),
                    contract_break,
                    kind_name: name_of(contract_break),
                });
            }
        }
    }

    let unchanged = held.len() == candidate.exported.len() && predecessors.iter().all(|(known, _)| *known == hash);
    Ok(if unchanged {
        Admitted::Unchanged
    } else {
        Admitted::Publish
    })
}
