//! A lane slot withheld because its child could not be terminated.
//!
//! The alternative — reclaim the slot and let the next dispatch `git clean`
//! a checkout a surviving child is still writing into — is the corruption
//! a re-adopted cancel used to cause by returning `Ok(())` for a kill it
//! never performed (issue #4999). Quarantine keeps the slot named in
//! occupancy so the janitor's sweep blocks on that one directory rather
//! than treating every slot as possibly-live.
//!
//! The record is a sibling of the slot checkout (`slot-<index>.quarantine`),
//! not a file inside it: a dispatch resets the checkout with `git clean
//! --force --force -d -x`, which would delete anything we left in the tree.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::identity::ProcessIdentity;

/// The suffix completing `slot-<index>` into the quarantine file's name.
const QUARANTINE_SUFFIX: &str = ".quarantine";

/// The prefix a lane slot's checkout (and this sibling record) carry.
const SLOT_PREFIX: &str = "slot-";

/// What a quarantine file records: which dispatch caused it, and the process
/// identity that could not be killed, when one was known.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SlotQuarantine {
    /// The slot index this record withholds.
    pub slot: usize,
    /// The dispatch whose child could not be terminated.
    pub nonce: String,
    /// The identity that was recorded for that child, when the evidence
    /// directory still held one. `None` for a run that predated identity
    /// records — those clear through the operator door like any other.
    pub identity: Option<ProcessIdentity>,
}

impl SlotQuarantine {
    /// The path of slot `index`'s quarantine record under `base_dir`.
    #[must_use]
    pub fn path(base_dir: &Path, slot: usize) -> PathBuf {
        base_dir.join(format!("{SLOT_PREFIX}{slot}{QUARANTINE_SUFFIX}"))
    }
}

/// Persist a quarantine for `slot` under `base_dir`. Best-effort: a write
/// fault is logged rather than failing the cancel that already could not
/// kill the child — the in-process slot claim is what withholds it for
/// the life of this coordinator, and the file is what survives a restart.
pub fn record(base_dir: &Path, slot: usize, nonce: &str, identity: Option<&ProcessIdentity>) {
    let path = SlotQuarantine::path(base_dir, slot);
    let body = SlotQuarantine { slot, nonce: nonce.to_owned(), identity: identity.cloned() };
    match serde_json::to_string_pretty(&body) {
        Ok(mut rendered) => {
            rendered.push('\n');
            if let Err(error) = fs::write(&path, rendered) {
                tracing::warn!(
                    target: "aether_chassis_bloomery::executor",
                    slot,
                    nonce,
                    path = %path.display(),
                    %error,
                    "local executor backend: could not persist the slot quarantine; this process still withholds the slot",
                );
            }
        }
        Err(error) => tracing::warn!(
            target: "aether_chassis_bloomery::executor",
            slot,
            nonce,
            %error,
            "local executor backend: could not encode the slot quarantine",
        ),
    }
}

/// Drop the quarantine file for `slot` if one is present. A missing file is
/// the slot already being free to allocate; a remove fault is logged.
pub fn clear(base_dir: &Path, slot: usize) {
    let path = SlotQuarantine::path(base_dir, slot);
    if let Err(error) = fs::remove_file(&path)
        && error.kind() != ErrorKind::NotFound
    {
        tracing::warn!(
            target: "aether_chassis_bloomery::executor",
            slot,
            path = %path.display(),
            %error,
            "local executor backend: could not remove the slot quarantine file",
        );
    }
}

/// Every slot index that currently has a quarantine file under `base_dir`.
///
/// Read off the directory each time it is asked: the operator clears a
/// quarantine by deleting the file, and a cached set would keep withholding
/// a slot the operator had already released.
#[must_use]
pub fn slots_on_disk(base_dir: &Path) -> HashSet<usize> {
    let Ok(entries) = fs::read_dir(base_dir) else {
        return HashSet::new();
    };
    entries.flatten().filter_map(|entry| slot_index_of(&entry.file_name())).collect()
}

fn slot_index_of(name: &OsStr) -> Option<usize> {
    let name = name.to_str()?;
    let index = name.strip_prefix(SLOT_PREFIX)?.strip_suffix(QUARANTINE_SUFFIX)?;
    if index.is_empty() || !index.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    index.parse().ok()
}
