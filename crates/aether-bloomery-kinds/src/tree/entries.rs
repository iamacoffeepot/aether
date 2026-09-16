//! Canonical directory map. Valid by construction: no case-fold collision.

use alloc::collections::BTreeMap;
use alloc::string::String;
use core::error::Error as StdError;
use core::fmt;

use crate::tree::name::Name;
use crate::tree::node::Node;

/// Why [`super::Tree::new`] refused a map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeError {
    /// Two names that are distinct as map keys collide after NFC plus case folding.
    Collides { a: Name, b: Name },
}

impl aether_data::Invariant for TreeError {
    fn reason(&self) -> &'static str {
        match self {
            Self::Collides { .. } => "collides",
        }
    }
}

impl fmt::Display for TreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Collides { a, b } => write!(f, "collides: {} and {}", a.as_str(), b.as_str()),
        }
    }
}

impl StdError for TreeError {}

/// [`BTreeMap`] of valid names that also refuses a case-insensitive collision.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
pub(super) struct Entries(BTreeMap<Name, Node>);

impl Entries {
    pub(super) fn new(map: BTreeMap<Name, Node>) -> Result<Self, TreeError> {
        Self::check(&map)?;
        Ok(Self(map))
    }

    pub(super) fn empty() -> Self {
        Self(BTreeMap::new())
    }

    pub(super) fn as_map(&self) -> &BTreeMap<Name, Node> {
        &self.0
    }

    fn check(map: &BTreeMap<Name, Node>) -> Result<(), TreeError> {
        if let Some((a, b)) = first_collision(map) {
            return Err(TreeError::Collides { a, b });
        }
        Ok(())
    }
}

fn fold_key(name: &Name) -> String {
    name.as_str().chars().flat_map(char::to_lowercase).collect()
}

fn first_collision(map: &BTreeMap<Name, Node>) -> Option<(Name, Name)> {
    let mut seen: BTreeMap<String, Name> = BTreeMap::new();
    for name in map.keys() {
        let folded = fold_key(name);
        if let Some(prior) = seen.get(&folded) {
            return Some((prior.clone(), name.clone()));
        }
        seen.insert(folded, name.clone());
    }
    None
}
