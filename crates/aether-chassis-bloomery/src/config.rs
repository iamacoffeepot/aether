//! The bloomery chassis knobs: the unit list, the closure byte budget, and
//! the read-cache budget.
//!
//! [`BloomeryConfig`] is the chassis's own derive-`Config` member, resolved off
//! the source stack into [`BloomeryEnv`](crate::chassis::BloomeryEnv) and declared
//! on the builder by [`compose`](aether_substrate::chassis::BootableChassis::compose),
//! so the known-key sweep and `--print-config` list every knob. `build` lowers
//! its value to the typed values the journal owner and the bundle driver spawn
//! over before it stands up the substrate, so a refused knob costs no boot.

use std::collections::HashMap;
use std::fmt::Display;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use aether_bloomery_journal::ReadCacheBudget;
use aether_bloomery_kinds::{ClosureLimit, UnitKey};
use aether_substrate::chassis::error::BootError;

/// The bloomery chassis knobs (issue #6244).
///
/// The `#[derive(aether_substrate::Config)]` emits the env-shaped
/// `BloomeryConfigLayer`, the clap-shaped [`BloomeryOverlay`], the
/// `FromArgvThenEnv` impl, and the inherent `from_env` /
/// `from_argv_then_env` shims — mirrors
/// [`ChassisBootConfig`](aether_chassis::boot::ChassisBootConfig).
#[derive(Clone, Debug, aether_substrate::Config)]
#[config(env_prefix = "AETHER_BLOOMERY", cli_prefix = "bloomery")]
pub struct BloomeryConfig {
    /// The engine's units (ADR-0240 D3): comma-separated `key=root` entries,
    /// such as `primary=/var/lib/aether/journal`.
    ///
    /// Each key is a [`UnitKey`]: a load name of at most
    /// [`UnitKey::MAX_BYTES`] bytes, unique in the list. Each root is the
    /// journal root directory that unit opens and drives: `journal.sqlite`
    /// plus the `blobs` directory of artifact files (ADR-0220). A root may
    /// hold `=` but not `,`. It is created when absent, but its parent
    /// directory must already exist. A path that is a regular file, such as
    /// an old single-file journal, is refused. The engine holds each root's
    /// exclusive lock while it runs, so two engines cannot share one root,
    /// and two entries naming one directory are refused. Required: an unset
    /// or empty list refuses boot with a named error. It stays an `Option`
    /// rather than a mandatory flag so `--describe` / `--print-config`
    /// answer with no units configured (ADR-0155 §4) — they exit in the
    /// shared prelude, before `build`.
    pub units: Option<String>,
    /// Byte budget for one closure read handed to the bundle driver.
    ///
    /// Lowered once through [`ClosureLimit::new`] (ADR-0226 decision 4); a
    /// refused value is a boot error naming the key, never a panic.
    #[config(default = 4_294_967_296u64)]
    pub closure_limit_bytes: u64,
    /// Engine-wide byte budget for the journal owners' read caches: the
    /// members each checked in for closure and artifact reads, kept resident
    /// so a later read skips their files and hashes.
    ///
    /// The total is divided equally among the configured units, each journal
    /// taking the floor share, so the caches together never exceed it. With
    /// one unit, its journal's share is the whole total.
    ///
    /// Charged per check-in allocation, so a cached member counts its whole
    /// slab, and the least recently used slab is evicted first. `0` disables
    /// the cache. The default, 2 GiB, fits one environment plus a source tree
    /// read as one slab.
    #[config(default = 2_147_483_648u64)]
    pub read_cache_bytes: u64,
}

impl Default for BloomeryConfig {
    fn default() -> Self {
        // Matches the unset resolution: no units and the ceiling limit. The
        // `mem::take` lift in `BloomeryChassis::build` needs this, and a derived
        // `Default` would leave `closure_limit_bytes: 0`, which
        // `ClosureLimit::new` refuses — the honest default is stated rather
        // than derived, the way `impl Default for RuntimeConfig` does.
        Self {
            units: None,
            closure_limit_bytes: ClosureLimit::MAX_BYTES,
            read_cache_bytes: ReadCacheBudget::DEFAULT_BYTES,
        }
    }
}

/// One configured unit: its key and the journal root it opens.
#[derive(Debug)]
pub(crate) struct UnitSpec {
    pub(crate) key: UnitKey,
    pub(crate) root: PathBuf,
}

impl BloomeryConfig {
    /// Lower the resolved knobs to what the mount seam spawns over: the units
    /// in config order and the drivers' closure limit. Called at the top of
    /// [`BloomeryChassis::build`](crate::BloomeryChassis), ahead of every boot
    /// side effect.
    ///
    /// # Errors
    ///
    /// Returns [`BootError`] naming `AETHER_BLOOMERY_UNITS` and the offending
    /// key for an unset or empty list, an entry with no `=` or an empty root,
    /// a key [`UnitKey::new`] refuses, a repeated key, or two roots that are
    /// one directory; or naming `AETHER_BLOOMERY_CLOSURE_LIMIT_BYTES` when the
    /// limit falls outside `ClosureLimit`'s accepted range.
    pub(crate) fn to_units_and_limit(&self) -> Result<(Vec<UnitSpec>, ClosureLimit), BootError> {
        let units = lower_units(self.units.as_deref().unwrap_or_default())?;
        let limit = ClosureLimit::new(self.closure_limit_bytes).map_err(|error| {
            BootError::Other(Box::new(io::Error::other(format!(
                "AETHER_BLOOMERY_CLOSURE_LIMIT_BYTES={} is not a usable closure limit: {error}",
                self.closure_limit_bytes,
            ))))
        })?;
        Ok((units, limit))
    }
}

/// The one unit the engine mounts while every unit would share one host
/// budget. #6828 splits the budget and removes this guard and its call.
///
/// # Errors
///
/// Returns [`BootError`] naming `AETHER_BLOOMERY_UNITS` and every configured
/// key when `units` holds anything other than exactly one unit.
pub(crate) fn sole_unit(units: Vec<UnitSpec>) -> Result<UnitSpec, BootError> {
    let [unit] = <[UnitSpec; 1]>::try_from(units).map_err(|units| {
        let keys = units.iter().map(|unit| format!("`{}`", unit.key)).collect::<Vec<_>>().join(", ");
        refuse(format!("{} units are configured ({keys}); this engine mounts exactly one unit", units.len()))
    })?;
    Ok(unit)
}

/// Parse and check the `key=root` list, in config order.
fn lower_units(list: &str) -> Result<Vec<UnitSpec>, BootError> {
    let mut units: Vec<UnitSpec> = Vec::new();
    let mut roots: HashMap<PathBuf, UnitKey> = HashMap::new();
    for entry in list.split(',').map(str::trim).filter(|entry| !entry.is_empty()) {
        let (key, root) = entry.split_once('=').ok_or_else(|| refuse(format!("entry `{entry}` has no `=`")))?;
        let (key, root) = (key.trim(), root.trim());
        let key = UnitKey::new(key).map_err(|error| refuse(format!("unit `{key}`: {error}")))?;
        if root.is_empty() {
            return Err(refuse(format!("unit `{key}` has an empty root")));
        }
        if units.iter().any(|unit| unit.key == key) {
            return Err(refuse(format!("unit `{key}` is named more than once")));
        }

        let root = PathBuf::from(root);
        if let Some(earlier) = roots.insert(canonical_root(&root), key.clone()) {
            return Err(refuse(format!("units `{earlier}` and `{key}` name one root directory ({})", root.display())));
        }
        units.push(UnitSpec { key, root });
    }

    if units.is_empty() {
        return Err(refuse("no unit is configured; the bloomery chassis needs at least one `<KEY>=<ROOT>` entry"));
    }
    Ok(units)
}

/// The directory `root` names, for comparing two entries' roots. An existing
/// root canonicalizes whole; an absent one canonicalizes its parent and keeps
/// its file name. A root for which neither canonicalizes stays lexical, and
/// opening it refuses boot later.
fn canonical_root(root: &Path) -> PathBuf {
    if let Ok(path) = fs::canonicalize(root) {
        return path;
    }
    let (Some(parent), Some(name)) = (root.parent(), root.file_name()) else {
        return root.to_path_buf();
    };
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    fs::canonicalize(parent).map_or_else(|_| root.to_path_buf(), |parent| parent.join(name))
}

/// A unit-list refusal, prefixed with the knob it names.
fn refuse(detail: impl Display) -> BootError {
    BootError::Other(Box::new(io::Error::other(format!("AETHER_BLOOMERY_UNITS / --bloomery-units: {detail}"))))
}

#[cfg(test)]
mod tests {
    use super::{BloomeryConfig, sole_unit};
    use aether_bloomery_journal::ReadCacheBudget;
    use aether_bloomery_kinds::{ClosureLimit, UnitKey};
    use aether_substrate::config::ConfigSources;

    fn with_units(units: Option<String>) -> BloomeryConfig {
        BloomeryConfig { units, ..BloomeryConfig::default() }
    }

    #[test]
    fn default_closure_limit_is_accepted() {
        // Resolving off an empty stack must yield the declared default, and the
        // default must be a limit `ClosureLimit::new` accepts: catches the
        // derive's `default = 4_294_967_296u64` literal drifting above a lowered
        // `ClosureLimit::MAX_BYTES`, which would fail every default boot.
        let mut sources = ConfigSources::new(None);
        let mut config = sources.resolve::<BloomeryConfig>().expect("resolve off an empty stack");
        assert_eq!(config.closure_limit_bytes, ClosureLimit::MAX_BYTES);
        assert_eq!(config.read_cache_bytes, ReadCacheBudget::DEFAULT_BYTES);
        config.units = Some("primary=journal".to_owned());
        let (_, limit) = config.to_units_and_limit().expect("the default limit lowers");
        assert_eq!(limit.get(), ClosureLimit::MAX_BYTES);
    }

    #[test]
    fn unit_list_refusals_name_the_key() {
        // Catches a refusal that goes missing or stops naming the key, and a
        // root comparison on raw strings: `<dir>/src/..` is `<dir>`, whether
        // the root exists or only its parent does.
        let dir = env!("CARGO_MANIFEST_DIR");
        let long = "k".repeat(UnitKey::MAX_BYTES + 1);
        let cases = [
            (None, vec![]),
            (Some(" , ".to_owned()), vec![]),
            (Some("k".to_owned()), vec!["`k`"]),
            (Some("k=".to_owned()), vec!["`k`"]),
            (Some("a:b=/x".to_owned()), vec!["`a:b`"]),
            (Some(format!("{long}=/x")), vec![long.as_str()]),
            (Some("k=/one,k=/two".to_owned()), vec!["`k`"]),
            (Some(format!("a={dir},b={dir}/src/..")), vec!["`a`", "`b`"]),
            (Some(format!("a={dir}/absent,b={dir}/src/../absent")), vec!["`a`", "`b`"]),
        ];
        for (units, keys) in cases {
            let error = with_units(units.clone()).to_units_and_limit().expect_err("the list is refused").to_string();

            assert!(error.contains("AETHER_BLOOMERY_UNITS"), "{units:?}: {error}");
            for key in keys {
                assert!(error.contains(key), "{units:?} names {key}: {error}");
            }
        }
    }

    #[test]
    fn two_distinct_units_lower_in_order_and_the_guard_refuses_them() {
        // Catches lowering that reorders units, and a guard that lets a second
        // unit through before the host budget is split.
        let (units, _) = with_units(Some("b=/b, a=/a".to_owned())).to_units_and_limit().expect("two units lower");
        let keys: Vec<&str> = units.iter().map(|unit| unit.key.as_str()).collect();
        assert_eq!(keys, ["b", "a"]);

        let error = sole_unit(units).expect_err("the guard refuses two units").to_string();
        assert!(error.contains("AETHER_BLOOMERY_UNITS") && error.contains("`b`") && error.contains("`a`"), "{error}");
    }
}
