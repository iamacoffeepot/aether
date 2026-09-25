//! One header's path, link target, and typeflag, checked against the tree rules.

use std::str;

use aether_bloomery_kinds::{Name, Path};

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

/// A symlink target, checked by [`Path::new`].
pub(super) fn link_target(raw: &[u8]) -> Result<Path, Refusal> {
    let text = str::from_utf8(raw).map_err(|_| Refusal::NotUtf8)?;
    Path::new(text).map_err(Refusal::LinkTarget)
}

/// What an entry header makes, once extended headers are consumed.
pub(super) enum EntryKind {
    Regular { executable: bool },
    Hardlink,
    Symlink,
    Directory,
}

/// Classify an entry typeflag. The owner-exec bit makes an executable, as in Git.
pub(super) fn classify(flag: u8, mode: u64) -> Result<EntryKind, Refusal> {
    match flag {
        typeflag::REGULAR | typeflag::REGULAR_OLD => Ok(EntryKind::Regular { executable: mode & 0o100 != 0 }),
        typeflag::HARDLINK => Ok(EntryKind::Hardlink),
        typeflag::SYMLINK => Ok(EntryKind::Symlink),
        typeflag::DIRECTORY => Ok(EntryKind::Directory),
        other => Err(Refusal::UnsupportedType(other)),
    }
}
