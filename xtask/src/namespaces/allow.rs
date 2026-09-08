//! The allow file: the literals that are a dependency-direction fact rather
//! than a hand-named address.
//!
//! A crate that cannot depend on the crate declaring a namespace has no const
//! to read, so it writes the name. That is honest, and the substrate's own
//! registry says so in prose already
//! (`crates/aether-substrate/src/mail/registry/names.rs`). This file is where
//! that prose becomes a checked statement: one entry per (crate, namespace)
//! pair, each carrying the reason it cannot be typed away.
//!
//! Entries are keyed at crate granularity rather than per line so a moved call
//! site does not need the file re-edited — the claim being made is about the
//! dependency edge, which is a property of the crate pair.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct AllowFile {
    #[serde(default)]
    allow: Vec<Entry>,
}

#[derive(Debug, Deserialize)]
pub struct Entry {
    #[serde(rename = "crate")]
    pub crate_name: String,
    pub namespace: String,
    /// Why this crate cannot read the declaring crate's const. Required — an
    /// entry without one is an unexplained exemption, which is the thing the
    /// check exists to stop.
    pub reason: String,
}

/// The allow file's entries, plus the bookkeeping that reports the ones no
/// literal matched any more.
pub struct AllowList {
    entries: Vec<Entry>,
    matched: BTreeSet<usize>,
}

impl AllowList {
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let file: AllowFile = toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        Ok(Self { entries: file.allow, matched: BTreeSet::new() })
    }

    /// Whether this crate is allowed to write this namespace, recording the
    /// hit so [`stale`](Self::stale) can report entries nothing needed.
    pub fn allows(&mut self, crate_name: &str, namespace: &str) -> bool {
        let found =
            self.entries.iter().position(|entry| entry.crate_name == crate_name && entry.namespace == namespace);
        if let Some(index) = found {
            self.matched.insert(index);
        }
        found.is_some()
    }

    /// Entries no literal matched. An allowance that has outlived the literal
    /// it covered is a standing licence nobody asked for, so the check reports
    /// it rather than letting the file accumulate.
    pub fn stale(&self) -> Vec<&Entry> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(index, _)| !self.matched.contains(index))
            .map(|(_, entry)| entry)
            .collect()
    }
}
