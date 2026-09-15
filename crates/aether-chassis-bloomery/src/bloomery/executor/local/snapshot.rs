//! The per-base warm target snapshot store (#6047).
//!
//! A lane slot's cargo target directory is warm for whatever the *last*
//! dispatch in that slot built. That is the whole value of the slot layout
//! when consecutive dispatches share a base, and it is worthless when they do
//! not: on 2026-09-15 the same nine-crate closure compiled in four minutes in
//! a slot whose previous build was the same day head and in fourteen minutes
//! in a slot whose previous build was a stranger's tree, and two freshly
//! created slots paid between seven and fourteen.
//!
//! So the warmth a lane starts from stops being "whatever was here" and
//! becomes "the base this dispatch stands on". The store holds one cargo
//! target directory per base commit under `<target_base>/snapshots/<base>`,
//! published from the one build that is always a full workspace build —
//! `verify.base` at seal — and cloned into a slot before the lane's own build
//! runs. On the cache tier the clone is a reflink copy: metadata speed, no
//! bytes moved.
//!
//! # Why the clone is a clone and not a share
//!
//! A lane writes into the target directory it was handed, and cargo does not
//! only ever create-and-rename: fingerprint files, `output`, and the build
//! directory's stamps are truncated in place. A slot that *shared* inodes
//! with the published snapshot would rewrite the snapshot underneath every
//! other slot cloning from it, which is the one thing this store must never
//! allow. Reflink extents are copy-on-write and so are safe; hardlinks are
//! not, which is why [`CloneMode::Auto`] falls back to a plain byte copy
//! rather than to `cp -al`. [`CloneMode::Hardlink`] stays reachable for an
//! operator who has measured their own layout and wants the speed, and is
//! never what the default chooses.
//!
//! # Which snapshot a dispatch is warm against
//!
//! A dispatch names a git commit to check out, not a base: a member Verify
//! checks out that member's captured candidate, and a Construct checks out
//! the checkpoint it resumes. What those commits have in common is the base
//! they were built on top of, and git already knows it — a published
//! snapshot's base is the right warmth for a checkout exactly when it is an
//! **ancestor** of that checkout. So the resolution is `merge-base
//! --is-ancestor` against each published base, newest publish first, and the
//! first hit wins. No base digest has to be plumbed through the work order,
//! and a `verify.base` re-run resolves to its own base (a commit is its own
//! ancestor).
//!
//! The slot records which base it was last warmed from in
//! [`PROVENANCE_FILE`]; a dispatch whose chosen base is the one already
//! recorded leaves the slot alone, because a slot that derives from the base
//! *and has built since* is strictly warmer than the pristine snapshot.

use std::collections::HashSet;
use std::fs;
use std::hash::BuildHasher;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Instant, SystemTime};

use serde::{Deserialize, Serialize};

/// The directory under the lane target base that holds the published
/// snapshots, one subdirectory per base commit.
pub const SNAPSHOTS_DIR: &str = "snapshots";

/// The file inside a cargo target directory naming the base commit it was
/// warmed from — written into a clone as it is staged, so it lands with the
/// tree and cannot describe a directory it is not in.
pub const PROVENANCE_FILE: &str = ".snapshot-source";

/// The file inside a published snapshot carrying its publication instant in
/// Unix milliseconds. Recency orders both the ancestor search and the prune,
/// and a directory mtime does not survive a clone.
pub const PUBLISHED_FILE: &str = ".published";

/// The record a dispatch writes beside its evidence describing how its slot
/// was warmed.
pub const WARMTH_RECORD: &str = "snapshot.json";

/// The prefix a half-built clone wears while it is being staged. Under the
/// snapshots root for a publish, under the target base for a slot warm —
/// same volume as its destination either way, so the rename that completes it
/// is atomic.
const STAGING_PREFIX: &str = ".staging-";

/// The marker an evicted directory wears between the rename that takes it out
/// of the build path and the removal that frees its bytes. The same spelling
/// the janitor's own eviction uses, so the janitor's leftover reclaim clears
/// what a swap could not remove.
const EVICTING_SUFFIX: &str = ".evicting-";

/// How a clone copies a tree.
///
/// The preference an operator states, and the mechanism a clone actually
/// used — [`SlotWarmth::mode`] reports the second, which on a
/// [`Auto`](Self::Auto) host is whichever rung the chain reached.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloneMode {
    /// Reflink if the filesystem supports it, plain copy if it does not.
    ///
    /// The default, and the only setting that is correct without knowing the
    /// host: a reflink clone is copy-on-write, so a lane writing into it
    /// cannot reach the snapshot, and the copy fallback is slow but equally
    /// safe. Hardlinks are deliberately not in this chain — see the module
    /// docs.
    #[default]
    Auto,
    /// Reflink only. A host whose cache tier is known to support it (XFS with
    /// `reflink=1`, APFS, btrfs) states this so a silent fall back to a
    /// multi-hundred-gigabyte byte copy cannot happen; a filesystem that
    /// refuses the reflink leaves the slot as it was and the dispatch runs on
    /// whatever warmth it had.
    Reflink,
    /// Hardlink every file. Fastest on any filesystem and **unsafe unless the
    /// operator has established that nothing in their toolchain truncates a
    /// target file in place** — cargo does, for fingerprints and build
    /// stamps, so a shared inode means a lane rewriting the published
    /// snapshot. Never chosen by [`Auto`](Self::Auto).
    Hardlink,
    /// Plain byte copy. Always safe, always slow; the fallback rung, and the
    /// setting a host states when it wants no surprises.
    Copy,
}

impl CloneMode {
    /// Parse the operator's spelling, defaulting anything unrecognized (the
    /// empty string included) to [`Auto`](Self::Auto) with a warning.
    ///
    /// A misspelled knob must not be a refusal to warm: the snapshot store is
    /// an optimization, and the safe default is exactly what a host that said
    /// nothing gets.
    #[must_use]
    pub fn parse(configured: &str) -> Self {
        match configured.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Self::Auto,
            "reflink" => Self::Reflink,
            "hardlink" => Self::Hardlink,
            "copy" => Self::Copy,
            other => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::executor",
                    mode = other,
                    "snapshot store: unrecognized clone mode; using auto",
                );
                Self::Auto
            }
        }
    }
}

/// What warming a slot did, recorded beside the dispatch's evidence.
///
/// Every field is an observation rather than a judgment: a miss is not a
/// failure, it is a dispatch whose base has no published snapshot, and the
/// lane runs either way.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct SlotWarmth {
    /// Whether this dispatch's slot target came from a published snapshot —
    /// either cloned by this dispatch or already derived from the same base.
    pub hit: bool,
    /// The base commit the slot is warm for, when it is warm for one.
    pub base: Option<String>,
    /// The base the slot recorded before this dispatch, when it recorded one.
    /// Equal to [`base`](Self::base) on the leave-alone path.
    pub previous: Option<String>,
    /// Whether this dispatch actually copied a tree. `false` on a slot that
    /// already derived from the chosen base, which is the common warm case
    /// and costs nothing.
    pub cloned: bool,
    /// Wall-clock milliseconds the clone took. `0` when none ran.
    pub clone_millis: u64,
    /// The mechanism the clone used, when one ran.
    pub mode: Option<CloneMode>,
    /// Why this dispatch is not warm, or why a clone was abandoned. `None` on
    /// a clean hit.
    pub detail: Option<String>,
}

impl SlotWarmth {
    /// A miss naming why.
    fn missed(detail: impl Into<String>, previous: Option<String>) -> Self {
        Self { previous, detail: Some(detail.into()), ..Self::default() }
    }

    /// Persist this record beside the dispatch's evidence.
    ///
    /// Best-effort: a dispatch whose warmth record could not be written still
    /// runs, it just cannot be measured afterwards.
    pub fn record(&self, evidence_dir: &Path) {
        let Ok(mut rendered) = serde_json::to_string_pretty(self) else {
            return;
        };
        rendered.push('\n');
        if let Err(error) =
            fs::create_dir_all(evidence_dir).and_then(|()| fs::write(evidence_dir.join(WARMTH_RECORD), rendered))
        {
            tracing::warn!(
                target: "aether_chassis_bloomery::executor",
                evidence = %evidence_dir.display(),
                %error,
                "snapshot store: could not record how this dispatch's slot was warmed",
            );
        }
    }
}

/// The published store: where the snapshots live and how they are copied.
///
/// Held by the backend and handed to each spawn through
/// [`RunSpec::warm_from`](super::RunSpec::warm_from), so the seam that clones
/// into a slot is the same one that knows the slot's target directory.
#[derive(Clone, Debug)]
pub struct SnapshotStore {
    root: PathBuf,
    mode: CloneMode,
    keep: usize,
}

impl SnapshotStore {
    /// The store under `target_base`, keeping `keep` snapshots past the bases
    /// still in use and cloning with `mode`.
    #[must_use]
    pub fn new(target_base: &Path, keep: usize, mode: CloneMode) -> Self {
        Self { root: target_base.join(SNAPSHOTS_DIR), mode, keep }
    }

    /// Where the snapshots live.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// How many snapshots past the in-use bases a prune keeps.
    #[must_use]
    pub const fn keep(&self) -> usize {
        self.keep
    }

    /// Publish `slot_target` as the snapshot for `base_hex`.
    ///
    /// Clone-then-rename, never a move: the slot target stays exactly where
    /// it is and stays warm for the lane that produced it. A publish that
    /// lands on an existing snapshot for the same base replaces it, because
    /// the newer one was built by a greener run of the same tree.
    ///
    /// # Errors
    /// The staging clone or the rename that completes it failed. The caller
    /// logs and moves on: a publish is an optimization for later dispatches,
    /// never a verdict on the one that produced the tree.
    pub fn publish(&self, slot_target: &Path, base_hex: &str) -> io::Result<CloneMode> {
        if !is_base_hex(base_hex) {
            return Err(io::Error::other(format!("{base_hex} is not a commit id the store can be keyed by")));
        }
        fs::create_dir_all(&self.root)?;
        let staging = staging_path(&self.root, base_hex);
        let mode = match clone_tree(slot_target, &staging, self.mode) {
            Ok(mode) => mode,
            Err(error) => {
                remove_quietly(&staging);
                return Err(error);
            }
        };
        stamp_published(&staging)?;
        // The provenance a clone of this snapshot will carry, written into the
        // snapshot itself so the slot inherits it with the tree rather than
        // from a second write the swap could lose.
        fs::write(staging.join(PROVENANCE_FILE), format!("{base_hex}\n"))?;

        let published = self.dir_for(base_hex);
        let replaced = published.is_dir().then(|| evicting_path(&published)).flatten();
        if let Some(aside) = replaced.as_deref() {
            fs::rename(&published, aside)?;
        }
        match fs::rename(&staging, &published) {
            Ok(()) => {
                if let Some(aside) = replaced.as_deref() {
                    remove_quietly(aside);
                }
                Ok(mode)
            }
            Err(error) => {
                // Put the snapshot that was there back: an empty slot in the
                // store is worse than a stale one, because the next dispatch
                // on this base would rebuild from nothing.
                if let Some(aside) = replaced.as_deref() {
                    let _ = fs::rename(aside, &published);
                }
                remove_quietly(&staging);
                Err(error)
            }
        }
    }

    /// Bring `slot_target` to the best published snapshot for the dispatch
    /// checking out `checkout_hex`, reporting what happened.
    ///
    /// Never fails the caller: every shortfall — no ancestor published, a
    /// clone the filesystem refused, a swap that could not rename — is a miss
    /// the lane runs through on whatever warmth the slot already had. The
    /// only thing this must never do is leave a slot half-cloned, which is
    /// why every clone is staged beside its destination and renamed into
    /// place.
    #[must_use]
    pub fn warm(&self, repo: &Path, slot_target: &Path, checkout_hex: &str) -> SlotWarmth {
        let previous = recorded_provenance(slot_target);
        let Some(base) = self.nearest_published_ancestor(repo, checkout_hex) else {
            return SlotWarmth::missed("no published snapshot is an ancestor of this dispatch's checkout", previous);
        };
        if previous.as_deref() == Some(base.as_str()) {
            return SlotWarmth {
                hit: true,
                base: Some(base),
                previous,
                cloned: false,
                clone_millis: 0,
                mode: None,
                detail: None,
            };
        }

        let started = Instant::now();
        let staging = match staging_path_beside(slot_target) {
            Ok(staging) => staging,
            Err(error) => {
                return SlotWarmth::missed(format!("no staging path beside the slot target: {error}"), previous);
            }
        };
        let mode = match clone_tree(&self.dir_for(&base), &staging, self.mode) {
            Ok(mode) => mode,
            Err(error) => {
                remove_quietly(&staging);
                return SlotWarmth::missed(format!("clone of snapshot {base} failed: {error}"), previous);
            }
        };
        if let Err(error) = fs::write(staging.join(PROVENANCE_FILE), format!("{base}\n")) {
            remove_quietly(&staging);
            return SlotWarmth::missed(
                format!("clone of snapshot {base} could not record its provenance: {error}"),
                previous,
            );
        }
        if let Err(error) = swap_into_place(&staging, slot_target) {
            remove_quietly(&staging);
            return SlotWarmth::missed(format!("clone of snapshot {base} could not take the slot: {error}"), previous);
        }
        SlotWarmth {
            hit: true,
            base: Some(base),
            previous,
            cloned: true,
            clone_millis: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            mode: Some(mode),
            detail: None,
        }
    }

    /// The directory a base's snapshot lives in.
    #[must_use]
    pub fn dir_for(&self, base_hex: &str) -> PathBuf {
        self.root.join(base_hex)
    }

    /// The published base that is the newest ancestor of `checkout_hex`.
    ///
    /// Newest by publication rather than by git distance: the two agree on a
    /// mainline that only moves forward, and publication is one file read
    /// against a `rev-list --count` per candidate.
    fn nearest_published_ancestor(&self, repo: &Path, checkout_hex: &str) -> Option<String> {
        published(&self.root)
            .into_iter()
            .find(|candidate| is_ancestor(repo, &candidate.base, checkout_hex))
            .map(|candidate| candidate.base)
    }
}

/// One published snapshot as the store lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Published {
    /// The base commit this snapshot was built at.
    pub base: String,
    /// When it was published, in Unix milliseconds. `0` for a snapshot whose
    /// stamp is missing or unreadable, which sorts it oldest — the right end
    /// for a directory nothing can date.
    pub published_unix_millis: u64,
}

/// Every published snapshot under `root`, newest first.
///
/// Only well-formed entries: a directory whose name is not a commit id is
/// something else's, and this store never acts on a directory it did not
/// mint.
#[must_use]
pub fn published(root: &Path) -> Vec<Published> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut found: Vec<Published> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let base = path.file_name()?.to_str()?.to_owned();
            (is_base_hex(&base) && path.is_dir())
                .then(|| Published { published_unix_millis: read_published(&path), base })
        })
        .collect();
    found.sort_by(|left, right| {
        right.published_unix_millis.cmp(&left.published_unix_millis).then_with(|| left.base.cmp(&right.base))
    });
    found
}

/// Prune published snapshots down to `in_use` plus the newest `keep`,
/// returning how many directories the store actually returned to the disk —
/// pruned snapshots plus the transient staging and moved-aside directories an
/// earlier pass or an abandoned clone left behind.
///
/// The caller decides *when* — the janitor runs this between blooms or under
/// disk pressure, and never while a bloom walks — and supplies `in_use`, the
/// bases live slot targets currently derive from. Those are kept whatever
/// their age: a base some slot is warm against is a base the next dispatch in
/// that slot will ask for.
///
/// Targets only. This function names directories under the snapshots root and
/// nothing else; it cannot reach a checkout, a session tree, or an evidence
/// directory.
pub fn prune<S: BuildHasher>(root: &Path, keep: usize, in_use: &HashSet<String, S>) -> usize {
    let mut kept = 0;
    let mut removed = reclaim_leftovers(root);
    for candidate in published(root) {
        if in_use.contains(&candidate.base) {
            continue;
        }
        if kept < keep {
            kept += 1;
            continue;
        }
        let path = root.join(&candidate.base);
        tracing::info!(
            target: "aether_chassis_bloomery::janitor",
            base = %candidate.base,
            path = %path.display(),
            "janitor: pruning a target snapshot past the keep bound",
        );
        if evict_dir(&path) {
            removed += 1;
        }
    }
    removed
}

/// Delete the transient directories an earlier pass or an abandoned clone
/// left under the snapshots root, returning how many went.
///
/// A moved-aside prune and a staged clone are both outside the `<base hex>`
/// namespace [`published`] lists, so an unswept one is permanent *and*
/// invisible — a whole target directory's worth of bytes the store cannot
/// see. Runs at the head of every prune rather than inside the keep test,
/// because a store already inside its keep bound is exactly the one that
/// would otherwise hold them forever.
///
/// Strict about what it acts on: only a name this module minted, which is a
/// staging prefix or a base-hex name plus the eviction marker and its stamp.
/// Safe against a live publish's staging directory because the caller's gate
/// already excludes one: a publish happens on a passed `verify.base`, which
/// is an outstanding order, which is not a clear board.
fn reclaim_leftovers(root: &Path) -> usize {
    let Ok(entries) = fs::read_dir(root) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !path.is_dir() || !is_own_leftover(name) {
            continue;
        }
        if fs::remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Whether `name` is one of this module's own transient directory names.
fn is_own_leftover(name: &str) -> bool {
    if let Some(rest) = name.strip_prefix(STAGING_PREFIX) {
        return !rest.is_empty();
    }
    name.split_once(EVICTING_SUFFIX).is_some_and(|(base, stamp)| {
        is_base_hex(base) && !stamp.is_empty() && stamp.bytes().all(|byte| byte.is_ascii_digit())
    })
}

/// The bases the slot targets under `target_base` currently derive from.
///
/// Read off the slots rather than out of the journal: the store is keyed by
/// git commits and the journal's bases are domain digests, and what actually
/// decides whether a snapshot is about to be asked for again is whether a
/// slot is standing on it.
#[must_use]
pub fn bases_in_use(target_base: &Path) -> HashSet<String> {
    let Ok(entries) = fs::read_dir(target_base) else {
        return HashSet::new();
    };
    entries.flatten().filter_map(|entry| recorded_provenance(&entry.path())).collect()
}

/// The base a target directory records as the one it was warmed from.
fn recorded_provenance(target_dir: &Path) -> Option<String> {
    let recorded = fs::read_to_string(target_dir.join(PROVENANCE_FILE)).ok()?;
    let recorded = recorded.trim().to_owned();
    is_base_hex(&recorded).then_some(recorded)
}

/// Whether `ancestor` is an ancestor of (or identical to) `descendant` in the
/// repository at `repo`.
///
/// Absent objects answer `false`: a base whose commit this repository has
/// never fetched cannot be proved to be behind anything, and warming from a
/// snapshot whose relationship to the checkout is unknown is exactly the
/// stranger's-tree start this store exists to end.
fn is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> bool {
    // The identity case is the one `verify.base` re-runs on, and it is true
    // without asking git — which also keeps the store testable outside a
    // repository.
    if ancestor == descendant {
        return true;
    }
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Copy `src` to a fresh `dst`, reporting the mechanism that worked.
///
/// `dst` must not exist: every caller stages into a name nothing else holds,
/// so a half-finished tree is discardable rather than something a lane could
/// build in.
fn clone_tree(src: &Path, dst: &Path, mode: CloneMode) -> io::Result<CloneMode> {
    if !src.is_dir() {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("{} is not a directory", src.display())));
    }
    match mode {
        CloneMode::Reflink => reflink_tree(src, dst).map(|()| CloneMode::Reflink),
        CloneMode::Hardlink => walk_clone(src, dst, Link::Hard).map(|()| CloneMode::Hardlink),
        CloneMode::Copy => walk_clone(src, dst, Link::Copy).map(|()| CloneMode::Copy),
        CloneMode::Auto => match reflink_tree(src, dst) {
            Ok(()) => Ok(CloneMode::Reflink),
            Err(error) => {
                tracing::debug!(
                    target: "aether_chassis_bloomery::executor",
                    src = %src.display(),
                    %error,
                    "snapshot store: no reflink on this filesystem; copying",
                );
                remove_quietly(dst);
                walk_clone(src, dst, Link::Copy).map(|()| CloneMode::Copy)
            }
        },
    }
}

/// A whole-tree reflink through `cp`, which is the only portable way to ask
/// for one: `std::fs` has no copy-on-write copy, and the two spellings that
/// exist are GNU's `--reflink=always` and BSD/macOS's `-c`. Both refuse
/// rather than silently degrading, which is what makes the fallback decision
/// honest.
fn reflink_tree(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    let contents = src.join(".");
    for flavor in [["--reflink=always", "-R"], ["-c", "-R"]] {
        let status = Command::new("cp")
            .args(flavor)
            .arg(&contents)
            .arg(dst)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if status.success() {
            return Ok(());
        }
    }
    Err(io::Error::other("no cp on this host performed a reflink clone"))
}

/// Which mechanism [`walk_clone`] materializes a regular file with.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Link {
    /// Share the inode. Fast, and unsafe for anything cargo rewrites in
    /// place — see the module docs.
    Hard,
    /// Copy the bytes.
    Copy,
}

/// Copy a directory tree with an explicit work stack.
///
/// Iterative rather than recursive because a cargo target directory is
/// millions of files deep in places (`build/*/out` trees especially) and a
/// recursive walk's depth is the tree's, not a bounded parse's.
fn walk_clone(src: &Path, dst: &Path, link: Link) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    let mut stack = vec![(src.to_path_buf(), dst.to_path_buf())];
    while let Some((from, into)) = stack.pop() {
        for entry in fs::read_dir(&from)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let source = entry.path();
            let destination = into.join(entry.file_name());
            if kind.is_dir() {
                fs::create_dir_all(&destination)?;
                stack.push((source, destination));
            } else if kind.is_symlink() {
                clone_symlink(&source, &destination)?;
            } else if link == Link::Hard {
                fs::hard_link(&source, &destination)?;
            } else {
                fs::copy(&source, &destination)?;
            }
        }
    }
    Ok(())
}

/// Reproduce a symlink rather than following it: a target directory carries
/// links into the toolchain and into its own `build` trees, and copying what
/// they point at would both inflate the clone and break the relative ones.
#[cfg(unix)]
fn clone_symlink(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::unix::fs::symlink;

    symlink(fs::read_link(source)?, destination)
}

#[cfg(not(unix))]
fn clone_symlink(source: &Path, destination: &Path) -> io::Result<()> {
    fs::copy(source, destination).map(|_| ())
}

/// Take `staging` into `slot_target`'s place, moving whatever was there aside
/// under the janitor's own eviction marker so the bytes are reclaimed on a
/// later pass rather than in the dispatch's start path.
///
/// The rename is the safety: a `remove_dir_all` of a two-hundred-gigabyte
/// target directory here would put minutes into every warm start, and the
/// janitor already has a retry for exactly this leftover shape.
fn swap_into_place(staging: &Path, slot_target: &Path) -> io::Result<()> {
    let aside = slot_target.is_dir().then(|| evicting_path(slot_target)).flatten();
    if let Some(aside) = aside.as_deref() {
        fs::rename(slot_target, aside)?;
    }
    match fs::rename(staging, slot_target) {
        Ok(()) => Ok(()),
        Err(error) => {
            if let Some(aside) = aside.as_deref() {
                let _ = fs::rename(aside, slot_target);
            }
            Err(error)
        }
    }
}

/// Move a directory out of the way and delete it, reporting whether the bytes
/// are actually gone.
fn evict_dir(path: &Path) -> bool {
    let Some(aside) = evicting_path(path) else {
        return false;
    };
    if fs::rename(path, &aside).is_err() {
        return false;
    }
    if let Err(error) = fs::remove_dir_all(&aside) {
        tracing::warn!(
            target: "aether_chassis_bloomery::janitor",
            path = %aside.display(),
            %error,
            "janitor: a pruned snapshot was moved aside but could not be removed; a later pass retries",
        );
        return false;
    }
    true
}

/// A sibling name for a directory on its way out.
fn evicting_path(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    Some(path.parent()?.join(format!("{name}{EVICTING_SUFFIX}{}", stamp_nanos())))
}

/// A staging name under `root` for a snapshot being published.
fn staging_path(root: &Path, base_hex: &str) -> PathBuf {
    root.join(format!("{STAGING_PREFIX}{base_hex}-{}", stamp_nanos()))
}

/// A staging name beside `slot_target`, on its own volume so the rename that
/// completes the clone is atomic.
fn staging_path_beside(slot_target: &Path) -> io::Result<PathBuf> {
    let name = slot_target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::other(format!("{} has no nameable file name", slot_target.display())))?;
    let parent = slot_target
        .parent()
        .ok_or_else(|| io::Error::other(format!("{} has no parent directory", slot_target.display())))?;
    Ok(parent.join(format!("{STAGING_PREFIX}{name}-{}", stamp_nanos())))
}

/// Stamp a staged snapshot with the instant it was published.
fn stamp_published(staging: &Path) -> io::Result<()> {
    let millis = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX));
    fs::write(staging.join(PUBLISHED_FILE), format!("{millis}\n"))
}

/// The instant a published snapshot carries, or `0` when it carries none.
fn read_published(path: &Path) -> u64 {
    fs::read_to_string(path.join(PUBLISHED_FILE)).ok().and_then(|body| body.trim().parse().ok()).unwrap_or_default()
}

/// Nanoseconds since the epoch, the uniquifier every transient name carries.
fn stamp_nanos() -> u128 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |since| since.as_nanos())
}

/// Whether `candidate` is a git object id the store may be keyed by.
///
/// Lowercase hex of an even length, which is what `git rev-parse` prints and
/// what a directory name can safely be. Strict, because the name is a path
/// segment this module creates and removes directories at.
fn is_base_hex(candidate: &str) -> bool {
    !candidate.is_empty()
        && candidate.len().is_multiple_of(2)
        && candidate.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Remove a staged tree, logging nothing: it is scratch by construction and
/// the caller is already reporting the failure that stranded it.
fn remove_quietly(path: &Path) {
    let _ = fs::remove_dir_all(path);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::HashSet;
    use std::fs;
    use std::path::Path;

    use tempfile::TempDir;

    use super::{
        CloneMode, PROVENANCE_FILE, PUBLISHED_FILE, SNAPSHOTS_DIR, SnapshotStore, bases_in_use, is_base_hex, prune,
        published,
    };

    const BASE: &str = "aaaa1111bbbb2222cccc3333dddd4444eeee5555";
    const OTHER: &str = "1111aaaa2222bbbb3333cccc4444dddd5555eeee";

    // A small stand-in for a cargo target directory: nested dirs, a file that
    // would be rewritten in place, and a symlink.
    fn fake_target(root: &Path) {
        fs::create_dir_all(root.join("debug/deps")).unwrap();
        fs::create_dir_all(root.join("debug/.fingerprint/thing-abc")).unwrap();
        fs::write(root.join("CACHEDIR.TAG"), "Signature: 8a477f597d28d172789f06886806bc55").unwrap();
        fs::write(root.join("debug/deps/libthing.rlib"), vec![7u8; 512]).unwrap();
        fs::write(root.join("debug/.fingerprint/thing-abc/lib-thing.json"), "{\"rustc\":1}").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            symlink("deps/libthing.rlib", root.join("debug/libthing.rlib")).unwrap();
        }
    }

    // Every path under `root`, relative and sorted — the file set two trees
    // are compared by.
    fn listing(root: &Path) -> Vec<String> {
        let mut found = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(next) = stack.pop() {
            for entry in fs::read_dir(&next).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if entry.file_type().unwrap().is_dir() {
                    stack.push(path.clone());
                }
                found.push(path.strip_prefix(root).unwrap().display().to_string());
            }
        }
        found.sort();
        found
    }

    // Publishing stages a copy and leaves the slot's own target where it is —
    // the property that keeps the run that produced the tree warm.
    #[test]
    fn a_publish_copies_the_slot_target_without_moving_it() {
        let dir = TempDir::new().unwrap();
        let slot = dir.path().join("slot-0-target");
        fake_target(&slot);
        let store = SnapshotStore::new(dir.path(), 3, CloneMode::Copy);

        store.publish(&slot, BASE).unwrap();

        assert!(slot.join("debug/deps/libthing.rlib").exists(), "the slot target stays where the lane left it");
        let snapshot = dir.path().join(SNAPSHOTS_DIR).join(BASE);
        assert!(snapshot.join("debug/deps/libthing.rlib").exists());
        assert_eq!(
            fs::read_to_string(snapshot.join(PROVENANCE_FILE)).unwrap().trim(),
            BASE,
            "a snapshot names the base it was built at, so a clone of it inherits the provenance",
        );
        assert!(snapshot.join(PUBLISHED_FILE).exists());
    }

    // Publish, then clone into an empty slot: same file set, provenance
    // recorded, reported as a hit that actually copied.
    #[test]
    fn a_clone_into_an_empty_slot_reproduces_the_snapshot_and_records_its_base() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("slot-0-target");
        fake_target(&source);
        let store = SnapshotStore::new(dir.path(), 3, CloneMode::Copy);
        store.publish(&source, BASE).unwrap();

        let slot = dir.path().join("slot-1-target");
        let warmth = store.warm(dir.path(), &slot, BASE);

        assert!(warmth.hit && warmth.cloned, "an empty slot with a published ancestor is a clone: {warmth:?}");
        assert_eq!(warmth.base.as_deref(), Some(BASE));
        assert_eq!(warmth.mode, Some(CloneMode::Copy));
        assert_eq!(
            listing(&slot),
            listing(&dir.path().join(SNAPSHOTS_DIR).join(BASE)),
            "the clone is the snapshot's file set exactly",
        );
        assert_eq!(fs::read_to_string(slot.join(PROVENANCE_FILE)).unwrap().trim(), BASE);
    }

    // A slot already derived from the chosen base is left alone: it has built
    // since the clone and is warmer than the pristine snapshot.
    #[test]
    fn a_slot_already_on_the_base_keeps_its_own_warmth() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("slot-0-target");
        fake_target(&source);
        let store = SnapshotStore::new(dir.path(), 3, CloneMode::Copy);
        store.publish(&source, BASE).unwrap();

        let slot = dir.path().join("slot-1-target");
        assert!(store.warm(dir.path(), &slot, BASE).cloned, "the empty slot is cloned first");
        fs::write(slot.join("debug/deps/built-since.rlib"), b"later work").unwrap();

        let warmth = store.warm(dir.path(), &slot, BASE);

        assert!(warmth.hit && !warmth.cloned, "a slot on the base is not re-cloned: {warmth:?}");
        assert_eq!(warmth.clone_millis, 0);
        assert!(slot.join("debug/deps/built-since.rlib").exists(), "the warmth the slot earned since survives");
    }

    // The hardlink rung shares inodes, and the plain-copy rung does not. The
    // second is what `Auto` falls back to, and the reason it does.
    #[test]
    fn a_plain_copy_fallback_gives_the_clone_its_own_inodes() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("slot-0-target");
        fake_target(&source);
        let store = SnapshotStore::new(dir.path(), 3, CloneMode::Copy);
        store.publish(&source, BASE).unwrap();
        let snapshot_file =
            dir.path().join(SNAPSHOTS_DIR).join(BASE).join("debug/.fingerprint/thing-abc/lib-thing.json");

        let slot = dir.path().join("slot-1-target");
        assert_eq!(store.warm(dir.path(), &slot, BASE).mode, Some(CloneMode::Copy));
        fs::write(slot.join("debug/.fingerprint/thing-abc/lib-thing.json"), "{\"rustc\":2}").unwrap();

        assert_eq!(
            fs::read_to_string(&snapshot_file).unwrap(),
            "{\"rustc\":1}",
            "a lane truncating a fingerprint in place must not reach the published snapshot",
        );
    }

    // Auto on a filesystem with no reflink still produces a complete clone.
    #[test]
    fn auto_without_reflink_still_completes_the_clone() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("slot-0-target");
        fake_target(&source);
        let store = SnapshotStore::new(dir.path(), 3, CloneMode::Auto);
        store.publish(&source, BASE).unwrap();

        let slot = dir.path().join("slot-1-target");
        let warmth = store.warm(dir.path(), &slot, BASE);

        assert!(warmth.hit && warmth.cloned, "auto warms whether or not the host reflinks: {warmth:?}");
        assert!(matches!(warmth.mode, Some(CloneMode::Reflink | CloneMode::Copy)));
        assert_eq!(listing(&slot), listing(&dir.path().join(SNAPSHOTS_DIR).join(BASE)));
    }

    // A dispatch whose checkout has no published ancestor is a miss that says
    // so, and leaves the slot exactly as it found it.
    #[test]
    fn a_checkout_with_no_published_ancestor_is_a_named_miss() {
        let dir = TempDir::new().unwrap();
        let store = SnapshotStore::new(dir.path(), 3, CloneMode::Copy);
        let slot = dir.path().join("slot-0-target");
        fake_target(&slot);

        let warmth = store.warm(dir.path(), &slot, OTHER);

        assert!(!warmth.hit && !warmth.cloned);
        assert!(warmth.detail.is_some(), "a miss names why");
        assert!(slot.join("debug/deps/libthing.rlib").exists(), "a miss leaves the slot alone");
    }

    // The prune keeps in-use bases whatever their age, keeps the newest N of
    // the rest, and removes only what is left.
    #[test]
    fn a_prune_keeps_the_in_use_bases_and_the_newest_few() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join(SNAPSHOTS_DIR);
        // Oldest first, so the publication stamps order the way the names do.
        let bases: Vec<String> = (0u8..5).map(|index| format!("{:0>40}", format!("{index}a"))).collect();
        for (index, base) in bases.iter().enumerate() {
            let path = root.join(base);
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join(PUBLISHED_FILE), format!("{}\n", 1000 + index)).unwrap();
        }
        let in_use: HashSet<String> = HashSet::from([bases[0].clone()]);

        let removed = prune(&root, 2, &in_use);

        assert_eq!(removed, 2, "five published, one in use, two kept by recency");
        let left: HashSet<String> = published(&root).into_iter().map(|entry| entry.base).collect();
        assert_eq!(
            left,
            HashSet::from([bases[0].clone(), bases[4].clone(), bases[3].clone()]),
            "the in-use base survives being the oldest",
        );
    }

    // A prune that cannot remove a snapshot leaves it moved aside, and a
    // clone abandoned mid-stage leaves a staging directory. Both are outside
    // the name space the store lists, so an unswept one is invisible bytes.
    #[test]
    fn a_prune_clears_the_stores_own_transient_leavings() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join(SNAPSHOTS_DIR);
        for name in [format!("{BASE}.evicting-1757000000000000000"), format!(".staging-{BASE}-1757000000000000000")] {
            fs::create_dir_all(root.join(&name).join("debug")).unwrap();
        }
        // Not this module's: left alone whatever it is.
        fs::create_dir_all(root.join("something-else")).unwrap();

        assert_eq!(prune(&root, 3, &HashSet::new()), 2);
        assert!(root.join("something-else").is_dir(), "the store removes only names it minted");
    }

    // The in-use set is read off the slot targets' own provenance.
    #[test]
    fn the_in_use_bases_are_the_ones_the_slots_record() {
        let dir = TempDir::new().unwrap();
        for (slot, base) in [("slot-0-target", BASE), ("slot-1-target", OTHER), ("slot-2-target", BASE)] {
            let path = dir.path().join(slot);
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join(PROVENANCE_FILE), format!("{base}\n")).unwrap();
        }
        fs::create_dir_all(dir.path().join("slot-3-target")).unwrap();

        assert_eq!(bases_in_use(dir.path()), HashSet::from([BASE.to_owned(), OTHER.to_owned()]));
    }

    // Tripwire: the store's directory names are path segments it creates and
    // removes, so what may be one is exactly lowercase even-length hex.
    #[test]
    fn only_lowercase_even_length_hex_may_name_a_snapshot_directory() {
        assert!(is_base_hex(BASE));
        assert!(!is_base_hex(""), "empty");
        assert!(!is_base_hex("abc"), "odd length");
        assert!(!is_base_hex("ABCD"), "uppercase");
        assert!(!is_base_hex(".."), "a path segment that escapes the root");
        assert!(!is_base_hex("ab/cd"), "a separator");
    }

    // An unrecognized mode is the safe default rather than a refusal to warm.
    #[test]
    fn an_unrecognized_clone_mode_resolves_to_auto() {
        assert_eq!(CloneMode::parse(""), CloneMode::Auto);
        assert_eq!(CloneMode::parse(" Reflink "), CloneMode::Reflink);
        assert_eq!(CloneMode::parse("hardlink"), CloneMode::Hardlink);
        assert_eq!(CloneMode::parse("copy"), CloneMode::Copy);
        assert_eq!(CloneMode::parse("symlink"), CloneMode::Auto);
    }
}
