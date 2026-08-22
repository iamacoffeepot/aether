//! The JSON symbol table: one row per fn / struct / trait / impl method.

use std::cmp::Ordering;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum SymbolKind {
    Fn,
    ImplMethod,
    Struct,
    Trait,
    /// A method a trait *declares*, as distinct from an implementing type's
    /// definition of it (#5300).
    ///
    /// Named `Trait::method`, the way an [`ImplMethod`](Self::ImplMethod) is
    /// named `Type::method`. Without this row a trait declaration is invisible
    /// to the inventory — which is exactly the file a signature change is
    /// guaranteed to touch, so a search that could not see it reported a
    /// declared surface as complete when it was not.
    TraitMethod,
}

/// One inventoried item.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Symbol {
    #[serde(rename = "crate")]
    pub crate_name: String,
    pub name: String,
    pub kind: SymbolKind,
    pub module: String,
    pub visibility: String,
    pub signature: Signature,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    pub test: bool,
    pub path: String,
}

/// Arity plus normalized input/output types — the shape a lane greps, not a type-checker view.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Signature {
    pub arity: usize,
    pub inputs: Vec<String>,
    pub output: String,
}

/// Sorted inventory. Field order and symbol order are part of the emit contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Table {
    pub symbols: Vec<Symbol>,
}

impl Symbol {
    /// Identity used by `diff`: same helper in the same module, ignoring path churn.
    pub fn identity(&self) -> (&str, &str, &str, SymbolKind) {
        (self.crate_name.as_str(), self.module.as_str(), self.name.as_str(), self.kind)
    }
}

impl Ord for Symbol {
    fn cmp(&self, other: &Self) -> Ordering {
        self.crate_name
            .cmp(&other.crate_name)
            .then_with(|| self.path.cmp(&other.path))
            .then_with(|| self.module.cmp(&other.module))
            .then_with(|| self.kind.cmp(&other.kind))
            .then_with(|| self.name.cmp(&other.name))
            .then_with(|| self.signature.cmp(&other.signature))
            .then_with(|| self.visibility.cmp(&other.visibility))
            .then_with(|| self.test.cmp(&other.test))
    }
}

impl PartialOrd for Symbol {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Table {
    pub fn new(mut symbols: Vec<Symbol>) -> Self {
        symbols.sort();
        Self { symbols }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let bytes = fs::read(path).with_context(|| format!("read symbol table {}", path.display()))?;
        serde_json::from_slice(&bytes).with_context(|| format!("parse symbol table {}", path.display()))
    }

    pub fn to_json(&self) -> Result<String> {
        let mut json = serde_json::to_string_pretty(self).context("serialize symbol table")?;
        json.push('\n');
        Ok(json)
    }
}
