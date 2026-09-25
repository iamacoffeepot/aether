//! One header's path, link target, and typeflag, checked against the tree rules.

use std::{iter, str};

use aether_bloomery_kinds::{Name, Path};

use super::Rules;
use crate::block::typeflag;
use crate::{MAX_DEPTH, Refusal};

/// A normalized entry path: at least one segment, every one a valid [`Name`].
pub(super) struct EntryPath {
    parents: Vec<Name>,
    name: Name,
}

impl EntryPath {
    pub(super) fn parents(&self) -> &[Name] {
        &self.parents
    }

    pub(super) fn name(&self) -> &Name {
        &self.name
    }

    /// Whether the entry sits somewhere below a top-level `dev` directory.
    pub(super) fn in_dev(&self) -> bool {
        self.parents.first().is_some_and(|top| top.as_str() == "dev")
    }
}

/// Where an entry lands.
pub(super) enum Target {
    /// `.`, `./`, or an empty path.
    Root,
    Entry(EntryPath),
}

/// Normalize a raw archive path: strip leading `./` runs and one trailing
/// `/`, then check every segment.
pub(super) fn normalize(raw: &[u8]) -> Result<Target, Refusal> {
    let text = str::from_utf8(raw).map_err(|_| Refusal::NotUtf8)?;
    if text.starts_with('/') {
        return Err(Refusal::Absolute);
    }
    let mut rest = text;
    while let Some(stripped) = rest.strip_prefix("./") {
        rest = stripped;
    }
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest.is_empty() || rest == "." {
        return Ok(Target::Root);
    }
    if rest.split('/').count() > MAX_DEPTH {
        return Err(Refusal::TooDeep);
    }
    let mut parents = rest
        .split('/')
        .map(|segment| match segment {
            ".." => Err(Refusal::ParentSegment),
            segment => Name::new(segment).map_err(Refusal::Name),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let Some(name) = parents.pop() else {
        return Ok(Target::Root);
    };
    Ok(Target::Entry(EntryPath { parents, name }))
}

/// The symlink target of the entry at `entry`, checked by [`Path::new`].
///
/// Under [`Rules::userland`], an absolute target `/rest` is first rewritten
/// relative to the entry's directory: one `..` per parent segment, then
/// `rest`. A rewrite that is still not a valid [`Path`] is refused.
pub(super) fn link_target(raw: &[u8], entry: &EntryPath, rules: &Rules) -> Result<Path, Refusal> {
    let text = str::from_utf8(raw).map_err(|_| Refusal::NotUtf8)?;
    let target = match text.strip_prefix('/') {
        Some(rest) if rules.is_userland() => from_root(entry.parents().len(), rest),
        _ => text.to_owned(),
    };
    Path::new(target).map_err(Refusal::LinkTarget)
}

/// `rest`, a path relative to the root, spelled from a directory `depth`
/// levels below the root. The root itself is `.` from the root.
fn from_root(depth: usize, rest: &str) -> String {
    let segments = iter::repeat_n("..", depth).chain(Some(rest).filter(|rest| !rest.is_empty())).collect::<Vec<_>>();
    if segments.is_empty() {
        ".".to_owned()
    } else {
        segments.join("/")
    }
}

/// What an entry header makes, once extended headers are consumed.
pub(super) enum EntryKind {
    Regular {
        executable: bool,
    },
    Hardlink,
    Symlink,
    Directory,
    /// A character or block device, which only [`Rules::userland`] admits,
    /// and only to drop it.
    Device {
        flag: u8,
    },
}

/// Classify an entry typeflag. The owner-exec bit makes an executable, as in
/// Git. A device is refused here unless the rules are [`Rules::userland`].
pub(super) fn classify(flag: u8, mode: u64, rules: &Rules) -> Result<EntryKind, Refusal> {
    match flag {
        typeflag::REGULAR | typeflag::REGULAR_OLD => Ok(EntryKind::Regular { executable: mode & 0o100 != 0 }),
        typeflag::HARDLINK => Ok(EntryKind::Hardlink),
        typeflag::SYMLINK => Ok(EntryKind::Symlink),
        typeflag::DIRECTORY => Ok(EntryKind::Directory),
        typeflag::CHAR_DEVICE | typeflag::BLOCK_DEVICE if rules.is_userland() => Ok(EntryKind::Device { flag }),
        other => Err(Refusal::UnsupportedType(other)),
    }
}
