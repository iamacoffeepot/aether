//! One wasm + chassis bundle per tree, shared by every checkout that would
//! build the identical one (#6052).
//!
//! [`super::freshness`] already answers "would this build produce the artifacts
//! already sitting in *this* target directory". It cannot answer the question a
//! bloom actually asks, which is "has any slot on this host already built them".
//! Eleven verify slots run against one candidate tree, each in its own target
//! directory, and each pays the same twenty-odd cargo cross-builds to arrive at
//! byte-identical wasm — 443 to 872 seconds of it, cold. sccache does not cover
//! the case: a change in a crate upstream of the components changes the inputs
//! of every downstream wasm crate, so those compilations are real misses.
//!
//! So the freshness key doubles as a content address. The first slot to build a
//! tree publishes its artifacts under that key into a directory on the host's
//! cache tier; every later slot on the same key copies them in and skips the
//! build. The knob is `AETHER_DIST_CACHE_DIR`, and the cache is off when the
//! host does not name one — an operator checkout and a CI runner should not
//! silently start writing bundles to disk.
//!
//! Two properties carry the concurrency, and neither needs a lock file:
//!
//! - **Publishing is a rename.** An entry is filled in a staging directory and
//!   renamed into place under its key, so a reader sees a whole entry or no
//!   entry. Two publishers racing on one key both fill staging trees and one of
//!   them loses the rename (the directory it would land on is non-empty); the
//!   loser drops its staging tree and the winner's entry stands. Both built the
//!   same inputs, so which one wins does not matter.
//! - **Restoring copies, it never links.** A hardlinked artifact shares its
//!   inode with the cache, and the next `rustc` to write that path in the slot's
//!   target directory truncates it in place — poisoning the entry for every
//!   other slot mid-read. `fs::copy` already reflinks where the filesystem
//!   supports it (`copy_file_range` on Linux, `fcopyfile` on APFS), so the
//!   copy-on-write the issue asked for is what a same-filesystem restore gets
//!   anyway, without the aliasing.
//!
//! Every uncertainty resolves toward building, the same way freshness does. An
//! entry that cannot be read, a recorded size that does not match the file on
//! disk, a key the entry does not claim as its own: each is a miss, never a
//! restore. The failure this refuses to have is a gate judging a candidate
//! against wasm built from something else.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, fs, process};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::freshness::BuildKey;
use crate::cargo::write_json_pretty;

/// The variable the host points at its cache tier. Unset or empty is off.
const CACHE_ROOT_ENV: &str = "AETHER_DIST_CACHE_DIR";

/// How many published bundles the cache keeps, the one just published included.
/// A bundle carries every component's wasm plus the chassis binaries, so the
/// retention is the knob that decides the cache's footprint; eight covers a
/// day's rebases and rebuilds without letting a week accumulate.
const KEEP_ENTRIES: usize = 8;

/// What one entry records about itself, beside the artifacts it carries.
const ENTRY_MANIFEST: &str = "entry.json";

/// The subtree an entry mirrors the target directory into.
const ARTIFACT_DIR: &str = "artifacts";

/// Rewritten on every hit so the entry's directory mtime tracks last use, which
/// is what [`BundleCache::prune`] retains by. Not an artifact, so it is never
/// restored into a checkout.
const LAST_USED: &str = ".last-used";

/// Prefix of a directory being filled, and of one being deleted. Both are
/// dot-prefixed so they can never collide with a hex key.
const STAGING_PREFIX: &str = ".staging-";
const EVICTING_PREFIX: &str = ".evicting-";

/// How old a staging or evicting directory has to be before a later publisher
/// sweeps it as abandoned. Far longer than any publish takes, so a slow
/// concurrent publisher is never swept out from under itself; short enough that
/// a killed lane does not leave a bundle on the cache tier forever.
const STALE_TEMP: Duration = Duration::from_hours(6);

/// The marker line prefix `dist` prints so its caller can record which side of
/// the cache the run landed on without re-deriving the key.
const MARKER: &str = "dist: bundle-cache ";

/// Where a `dist` run's artifacts came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CacheStatus {
    /// The shared cache had this tree's bundle and it was copied in.
    Hit,
    /// The shared cache did not have it (or could not be trusted with it), so
    /// this run built it — and published it for the next slot.
    Miss,
    /// This checkout's own target directory already held the bundle, so the
    /// shared cache was never consulted. A warm slot re-running the same tree.
    Fresh,
}

impl CacheStatus {
    /// The word the evidence records and the marker line carries.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
            Self::Fresh => "fresh",
        }
    }

    fn parse(word: &str) -> Option<Self> {
        match word {
            "hit" => Some(Self::Hit),
            "miss" => Some(Self::Miss),
            "fresh" => Some(Self::Fresh),
            _ => None,
        }
    }
}

/// The line `dist` prints to name the side of the cache it landed on.
pub(super) fn marker(status: CacheStatus) -> String {
    format!("{MARKER}{}", status.as_str())
}

/// The status a `dist` child reported in its captured `output`, or `None` when
/// it reported none — the cache is off, or the tree could not be keyed, or the
/// captured output is not a `dist` run's at all.
///
/// The last marker wins: a captured stream can hold more than one `dist` run,
/// and the one that decided the artifacts the caller is about to use is the one
/// that ran last.
pub fn parse_status(output: &str) -> Option<CacheStatus> {
    output.lines().rev().find_map(|line| CacheStatus::parse(line.trim().strip_prefix(MARKER)?.trim()))
}

/// What an entry claims to carry: the key it was published under, restated
/// inside the entry so a directory moved or renamed by hand cannot masquerade
/// as another key's bundle, and every artifact's size.
///
/// Size rather than a content digest: the entry's files are written into a
/// staging tree nobody else can see and renamed into place whole, so the
/// failure a restore has to catch is a *truncated* or missing file — an entry
/// being evicted under a reader, or a publish that died mid-copy — and a length
/// catches exactly that, without hashing hundreds of megabytes on every hit.
#[derive(Serialize, Deserialize)]
struct EntryManifest {
    key: String,
    /// Path relative to the target directory (forward slashes, so an entry is
    /// readable from any host) → the artifact's length in bytes.
    files: BTreeMap<String, u64>,
}

/// The host's shared bundle cache.
pub(super) struct BundleCache {
    root: PathBuf,
}

impl BundleCache {
    /// The cache the host named, or `None` when it named none.
    #[allow(clippy::disallowed_methods)] // the host's cache tier is an external var, not cap config.
    pub(super) fn from_host() -> Option<Self> {
        env::var_os(CACHE_ROOT_ENV)
            .map(PathBuf::from)
            .filter(|root| !root.as_os_str().is_empty())
            .map(|root| Self { root })
    }

    /// Copy the bundle published under `key` into `target_dir`, reporting
    /// whether every `artifacts` path was restored.
    ///
    /// All of them or none of them is not something this can promise — a copy
    /// can fail on the last file — so a partial restore reports `false` and
    /// leaves the caller to build over what landed. What it does promise is
    /// that a `true` means every named artifact is present at the length the
    /// publisher recorded.
    pub(super) fn restore(&self, key: &BuildKey, target_dir: &Path, artifacts: &[PathBuf]) -> bool {
        let entry = self.root.join(key.hex());
        let Some(recorded) = read_entry(&entry, key) else {
            return false;
        };
        let Some(wanted) = relative_paths(target_dir, artifacts) else {
            return false;
        };
        for relative in &wanted {
            let Some(size) = recorded.files.get(relative) else {
                return false;
            };
            if !restore_one(&entry.join(ARTIFACT_DIR).join(relative), &target_dir.join(relative), *size) {
                return false;
            }
        }
        touch(&entry);
        true
    }

    /// Publish the artifacts this run just built under `key`, then prune.
    ///
    /// Best effort throughout: a cache that cannot be written costs the next
    /// slot a build it would have skipped, which is the direction this errs in
    /// anyway. It never fails the `dist` run that produced the artifacts.
    pub(super) fn publish(&self, key: &BuildKey, target_dir: &Path, artifacts: &[PathBuf]) {
        if let Err(error) = self.try_publish(key, target_dir, artifacts) {
            eprintln!(
                "dist: could not publish the bundle to {} ({error:#}); the next slot on this tree will rebuild",
                self.root.display()
            );
        }
        self.prune(key);
    }

    fn try_publish(&self, key: &BuildKey, target_dir: &Path, artifacts: &[PathBuf]) -> Result<()> {
        let wanted = relative_paths(target_dir, artifacts)
            .context("the built artifacts do not all sit under the target directory")?;
        fs::create_dir_all(&self.root).with_context(|| format!("create {}", self.root.display()))?;

        let staging = self.root.join(format!("{STAGING_PREFIX}{}", unique_suffix()));
        let filled = fill(&staging, key, target_dir, &wanted);
        if filled.is_err() {
            let _ = fs::remove_dir_all(&staging);
            return filled;
        }
        // The rename is the publish. It lands only when nothing holds the key
        // yet, so a publisher that loses the race to another slot building the
        // same inputs simply drops what it staged.
        if fs::rename(&staging, self.root.join(key.hex())).is_err() {
            let _ = fs::remove_dir_all(&staging);
        }
        Ok(())
    }

    /// Hold the cache to [`KEEP_ENTRIES`] published bundles, newest by last use,
    /// and sweep the temporary directories an interrupted publisher left behind.
    ///
    /// `keep` — the key the caller just published or restored — is excluded from
    /// the ranking and counted as one of the retained entries, so a prune can
    /// never evict the bundle the run that triggered it is standing on.
    fn prune(&self, keep: &BuildKey) {
        let Ok(listing) = fs::read_dir(&self.root) else {
            return;
        };
        let mut published: Vec<(SystemTime, PathBuf)> = Vec::new();
        for entry in listing.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.starts_with(STAGING_PREFIX) || name.starts_with(EVICTING_PREFIX) {
                if last_touched(&path).is_some_and(|at| at.elapsed().is_ok_and(|age| age > STALE_TEMP)) {
                    let _ = fs::remove_dir_all(&path);
                }
                continue;
            }
            if name == keep.hex() || !path.is_dir() {
                continue;
            }
            published.push((last_touched(&path).unwrap_or(UNIX_EPOCH), path));
        }
        published.sort_by_key(|(touched, _)| Reverse(*touched));
        for (_, path) in published.into_iter().skip(KEEP_ENTRIES.saturating_sub(1)) {
            self.evict(&path);
        }
    }

    /// Retire one entry: rename it out of its key first, so a slot that resolves
    /// the key from here on misses cleanly rather than finding a directory being
    /// emptied under it.
    fn evict(&self, entry: &Path) {
        let retired = self.root.join(format!("{EVICTING_PREFIX}{}", unique_suffix()));
        let _ = match fs::rename(entry, &retired) {
            Ok(()) => fs::remove_dir_all(&retired),
            Err(_) => fs::remove_dir_all(entry),
        };
    }
}

/// Mirror every artifact into `staging` and record what landed.
fn fill(staging: &Path, key: &BuildKey, target_dir: &Path, wanted: &[String]) -> Result<()> {
    fs::create_dir_all(staging).with_context(|| format!("create {}", staging.display()))?;

    let mut files = BTreeMap::new();
    for relative in wanted {
        let source = target_dir.join(relative);
        let destination = staging.join(ARTIFACT_DIR).join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let size = fs::copy(&source, &destination)
            .with_context(|| format!("copy {} -> {}", source.display(), destination.display()))?;
        files.insert(relative.clone(), size);
    }
    write_json_pretty(&staging.join(ENTRY_MANIFEST), &EntryManifest { key: key.hex().to_owned(), files })
}

/// The entry's own record of itself, when it reads and claims `key`.
fn read_entry(entry: &Path, key: &BuildKey) -> Option<EntryManifest> {
    let recorded: EntryManifest = serde_json::from_slice(&fs::read(entry.join(ENTRY_MANIFEST)).ok()?).ok()?;
    (recorded.key == key.hex()).then_some(recorded)
}

/// Copy one artifact out of the cache, reporting whether what landed is the
/// length the publisher recorded — on both sides, so an entry truncated since
/// it was published is refused before it is copied anywhere.
fn restore_one(source: &Path, destination: &Path, size: u64) -> bool {
    if !fs::metadata(source).is_ok_and(|meta| meta.len() == size) {
        return false;
    }
    destination.parent().is_some_and(|parent| fs::create_dir_all(parent).is_ok())
        && fs::copy(source, destination).is_ok_and(|copied| copied == size)
}

/// Every artifact's path relative to the target directory, rendered with
/// forward slashes. `None` when one of them sits outside that directory or
/// cannot be rendered as text — neither is something this can file under a key.
fn relative_paths(target_dir: &Path, artifacts: &[PathBuf]) -> Option<Vec<String>> {
    artifacts.iter().map(|artifact| relative_path(target_dir, artifact)).collect()
}

fn relative_path(target_dir: &Path, artifact: &Path) -> Option<String> {
    let parts: Option<Vec<&str>> =
        artifact.strip_prefix(target_dir).ok()?.components().map(|part| part.as_os_str().to_str()).collect();
    Some(parts?.join("/"))
}

/// Bump the entry's directory mtime by replacing the marker inside it, so the
/// prune's ranking is last use rather than publication.
fn touch(entry: &Path) {
    let marker = entry.join(LAST_USED);
    let _ = fs::remove_file(&marker);
    let _ = fs::write(&marker, b"");
}

fn last_touched(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|meta| meta.modified()).ok()
}

/// A name no concurrent publisher on this host can also pick.
fn unique_suffix() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_nanos());
    format!("{}-{nanos}", process::id())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::env::temp_dir;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::thread::sleep;
    use std::time::Duration;

    use super::{
        ARTIFACT_DIR, BuildKey, BundleCache, CacheStatus, ENTRY_MANIFEST, KEEP_ENTRIES, LAST_USED, STAGING_PREFIX,
        marker, parse_status, unique_suffix,
    };

    /// A stub bundle: the shapes a real one has — a wasm beside the profile
    /// root, one under `examples/`, and a host binary in the sibling profile
    /// directory — so the relative-path handling is exercised over a tree with
    /// depth rather than a flat list.
    const BUNDLE: [(&str, &[u8]); 3] = [
        ("wasm32-unknown-unknown/debug/aether_kit.wasm", b"component"),
        ("wasm32-unknown-unknown/debug/examples/trap_script.wasm", b"behavior"),
        ("debug/aether-headless", b"chassis"),
    ];

    fn temp_root(tag: &str) -> PathBuf {
        let root = temp_dir().join(format!("aether-dist-cache-{tag}-{}", unique_suffix()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    /// Write `bundle` into a target-directory-shaped tree and return the
    /// absolute artifact paths, the way `dist` hands them to the cache.
    fn stage_target(target: &Path, bundle: &[(&str, &[u8])]) -> Vec<PathBuf> {
        bundle
            .iter()
            .map(|(relative, bytes)| {
                let path = target.join(relative);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, bytes).unwrap();
                path
            })
            .collect()
    }

    /// The same paths without the bytes — what a fresh slot asks the cache for.
    fn wanted_in(target: &Path, bundle: &[(&str, &[u8])]) -> Vec<PathBuf> {
        bundle.iter().map(|(relative, _)| target.join(relative)).collect()
    }

    #[test]
    fn a_published_bundle_restores_byte_for_byte_into_a_fresh_target_tree() {
        // Tripwire: this is the whole feature — a slot that built nothing ends
        // up with the artifacts a slot that did build produced, at the same
        // paths under its own target directory. A restore that flattened the
        // tree, dropped the `examples/` nesting, or copied the wrong bytes
        // would hand the scenario tests wasm they cannot find or cannot trust.
        let root = temp_root("restore");
        let cache = BundleCache { root: root.join("cache") };
        let key = BuildKey::from_hex("aaaa1111");

        let builder = root.join("builder-target");
        cache.publish(&key, &builder, &stage_target(&builder, &BUNDLE));

        let fresh = root.join("fresh-target");
        assert!(cache.restore(&key, &fresh, &wanted_in(&fresh, &BUNDLE)), "the published key restores");
        for (relative, bytes) in BUNDLE {
            assert_eq!(fs::read(fresh.join(relative)).unwrap(), bytes, "{relative} restored with its own bytes");
        }

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_entry_that_lost_bytes_is_a_miss_rather_than_a_stale_restore() {
        // Tripwire: an entry can be truncated under a reader — a publish that
        // died mid-copy, an eviction racing a restore, a full cache volume. The
        // recorded length is the only thing standing between that and a gate
        // judging a candidate against a half-written wasm, so a restore that
        // trusted the file's presence alone would be silently wrong.
        let root = temp_root("truncated");
        let cache = BundleCache { root: root.join("cache") };
        let key = BuildKey::from_hex("bbbb2222");

        let builder = root.join("builder-target");
        cache.publish(&key, &builder, &stage_target(&builder, &BUNDLE));

        let entry = cache.root.join(key.hex()).join(ARTIFACT_DIR).join(BUNDLE[1].0);
        fs::write(&entry, b"tru").unwrap();

        let fresh = root.join("fresh-target");
        assert!(!cache.restore(&key, &fresh, &wanted_in(&fresh, &BUNDLE)), "a short artifact is never restored");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_entry_filed_under_another_key_is_never_served_for_this_one() {
        // Tripwire: the key is a directory name, and directory names get moved,
        // copied and rsynced by hand on a cache tier. The entry restating its
        // own key is what keeps a bundle from being served for inputs it was
        // not built from — the one failure this whole mechanism must not have.
        let root = temp_root("mislabelled");
        let cache = BundleCache { root: root.join("cache") };
        let built_under = BuildKey::from_hex("cccc3333");

        let builder = root.join("builder-target");
        cache.publish(&built_under, &builder, &stage_target(&builder, &BUNDLE));
        fs::rename(cache.root.join(built_under.hex()), cache.root.join("dddd4444")).unwrap();

        let fresh = root.join("fresh-target");
        let asked_for = BuildKey::from_hex("dddd4444");
        assert!(!cache.restore(&asked_for, &fresh, &wanted_in(&fresh, &BUNDLE)), "the entry disowns the key");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_racing_publisher_leaves_the_standing_entry_whole() {
        // Tripwire: eleven slots build one tree, and more than one can finish
        // before the first publishes. A publisher that wrote into the key
        // directly would let a reader see a half-filled entry; one that cleared
        // the key first would delete an entry being read. The staging-then-
        // rename is what makes the loser a no-op — and it must also not leave
        // its staging tree behind, or the cache tier grows a bundle per race.
        let root = temp_root("race");
        let cache = BundleCache { root: root.join("cache") };
        let key = BuildKey::from_hex("eeee5555");

        let first = root.join("first-target");
        cache.publish(&key, &first, &stage_target(&first, &BUNDLE));

        // A second slot finishing the same inputs, byte-for-byte distinct so
        // the assertion can tell whose entry survived.
        let losing: Vec<(&str, &[u8])> = BUNDLE.iter().map(|(relative, _)| (*relative, b"racer".as_slice())).collect();
        let second = root.join("second-target");
        cache.publish(&key, &second, &stage_target(&second, &losing));

        let fresh = root.join("fresh-target");
        assert!(cache.restore(&key, &fresh, &wanted_in(&fresh, &BUNDLE)), "the standing entry still restores");
        for (relative, bytes) in BUNDLE {
            assert_eq!(fs::read(fresh.join(relative)).unwrap(), bytes, "{relative} is wholly the winner's");
        }
        assert!(
            !fs::read_dir(&cache.root)
                .unwrap()
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().starts_with(STAGING_PREFIX)),
            "the losing publisher dropped what it staged",
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn publishing_prunes_to_the_retention_bound_and_never_the_new_key() {
        // Tripwire: the cache holds whole bundles — every component's wasm plus
        // the chassis binaries — so an unbounded one fills the cache tier and
        // takes every lane on the host down with it. And the entry a prune must
        // never take is the one the publisher that triggered it just wrote,
        // which is the entry its own slot is about to restore from.
        let root = temp_root("prune");
        let cache = BundleCache { root: root.join("cache") };

        let mut last = BuildKey::from_hex("0");
        for index in 0..u32::try_from(KEEP_ENTRIES).unwrap() + 3 {
            last = BuildKey::from_hex(&format!("{index:08x}"));
            let target = root.join(format!("target-{index}"));
            cache.publish(&last, &target, &stage_target(&target, &BUNDLE));
        }

        let kept: Vec<String> = fs::read_dir(&cache.root)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(kept.len(), KEEP_ENTRIES, "the cache is bounded: {kept:?}");
        assert!(kept.contains(&last.hex().to_owned()), "the key just published survives its own prune: {kept:?}");

        let fresh = root.join("fresh-target");
        assert!(cache.restore(&last, &fresh, &wanted_in(&fresh, &BUNDLE)), "and it is whole");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_caller_reads_the_last_status_the_child_printed() {
        // Tripwire: the verify gate stamps `prepare_cache` from this, over a
        // captured stream that can carry more than one dist run and a great
        // deal of cargo output that is not a marker. Matching the first marker,
        // or matching loosely, would attribute one run's cache outcome to
        // another's evidence.
        let captured = format!(
            "   Compiling aether-kit v0.1.0\n{}\ndist: 21 component(s) -> dist/manifest.json\n{}\n",
            marker(CacheStatus::Miss),
            marker(CacheStatus::Hit),
        );
        assert_eq!(parse_status(&captured), Some(CacheStatus::Hit), "the run that decided the artifacts is the last");
        assert_eq!(parse_status("dist: bundle-cache elsewhere\n"), None, "an unknown word is not a status");
        assert_eq!(parse_status("   Compiling xtask v0.1.0\n"), None, "a run with the cache off reports none");
    }

    #[test]
    fn an_entry_missing_an_artifact_the_caller_needs_is_a_miss() {
        // Tripwire: the artifact set moves — a new component crate, a new
        // behavior fixture, the chassis bins that `--no-bins` drops. An entry
        // published before the set grew carries fewer files than the caller
        // asks for, and serving it would leave the scenario tests looking for
        // wasm that was never restored.
        let root = temp_root("partial");
        let cache = BundleCache { root: root.join("cache") };
        let key = BuildKey::from_hex("ffff6666");

        let builder = root.join("builder-target");
        cache.publish(&key, &builder, &stage_target(&builder, &BUNDLE[..1]));

        let fresh = root.join("fresh-target");
        assert!(cache.restore(&key, &fresh, &wanted_in(&fresh, &BUNDLE[..1])), "what it carries restores");
        assert!(!cache.restore(&key, &fresh, &wanted_in(&fresh, &BUNDLE)), "a set it does not carry is a miss");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unreadable_cache_root_is_a_miss_and_not_a_failure() {
        // Tripwire: the host's cache tier can be absent, unmounted, or not yet
        // created on the first run of the day. Every one of those has to land
        // on "build it", because the alternative is a lane that fails before it
        // compiles a byte over a directory that is not the candidate's fault.
        let root = temp_root("absent");
        let cache = BundleCache { root: root.join("never-created") };
        let key = BuildKey::from_hex("77778888");

        let fresh = root.join("fresh-target");
        assert!(!cache.restore(&key, &fresh, &wanted_in(&fresh, &BUNDLE)), "an absent cache misses");
        assert!(!fresh.join(BUNDLE[0].0).exists(), "and leaves nothing behind in the checkout");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_artifact_outside_the_target_directory_is_never_filed() {
        // Tripwire: an entry is keyed by target-relative path, so a path that
        // does not sit under the target directory has no name inside the entry.
        // Filing it under its absolute path would bake one host's directory
        // layout into a bundle every other host then restores from.
        let root = temp_root("outside");
        let cache = BundleCache { root: root.join("cache") };
        let key = BuildKey::from_hex("9999aaaa");

        let builder = root.join("builder-target");
        let mut artifacts = stage_target(&builder, &BUNDLE);
        artifacts.push(root.join("elsewhere.wasm"));
        fs::write(root.join("elsewhere.wasm"), b"stray").unwrap();
        cache.publish(&key, &builder, &artifacts);

        assert!(!cache.root.join(key.hex()).exists(), "nothing was published at all");
        assert!(
            !cache.restore(&key, &root.join("fresh-target"), &wanted_in(&builder, &BUNDLE)),
            "and the key stays a miss",
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_hit_refreshes_the_entry_the_prune_ranks_by() {
        // Tripwire: retention is by last use, not by publication. An entry the
        // lane restores from every hour must outlive eight newer ones that were
        // published and never read again — otherwise the bundle with the
        // highest hit rate is the first one evicted.
        let root = temp_root("touch");
        let cache = BundleCache { root: root.join("cache") };
        let hot = BuildKey::from_hex("babababa");

        let builder = root.join("builder-target");
        cache.publish(&hot, &builder, &stage_target(&builder, &BUNDLE));
        let published_at = fs::metadata(cache.root.join(hot.hex())).unwrap().modified().unwrap();

        sleep(Duration::from_millis(50));
        let fresh = root.join("fresh-target");
        assert!(cache.restore(&hot, &fresh, &wanted_in(&fresh, &BUNDLE)));

        let used_at = fs::metadata(cache.root.join(hot.hex())).unwrap().modified().unwrap();
        assert!(used_at > published_at, "a hit moves the entry to the front of the retention order");
        assert!(
            !cache.root.join(hot.hex()).join(ARTIFACT_DIR).join(LAST_USED).exists()
                && cache.root.join(hot.hex()).join(ENTRY_MANIFEST).exists(),
            "the use marker lives beside the artifacts, never among them",
        );

        let _ = fs::remove_dir_all(&root);
    }
}
