//! Persisted package manifest + store-backed boot (ADR-0163 §1,
//! iamacoffeepot/aether#3967).
//!
//! The shippable form of an aether application is a directory:
//!
//! ```text
//! <package>/
//!   aether-desktop              # the chassis binary
//!   pack/manifest               # the one persisted manifest (this module)
//!   pack/objects/<sha256>       # boot wasm + config bytes and named objects, immutable
//!   pack/assets/…               # the shipped asset tree, verbatim
//! ```
//!
//! `pack/assets` is the depot's read-only asset tree, and it is deliberately
//! **not** content-addressed: a component reaches an asset by mailing
//! `aether.fs.read { addr: { namespace: "assets", … } }` with the path an
//! author wrote, so the shipped tree has to keep those paths. Objects are
//! addressed by hash because the manifest names them; assets are addressed
//! by path because the running program does. Boot roots the `assets`
//! namespace here via [`package_assets_root`].
//!
//! `pack/objects` also holds what a running engine reads. The manifest's
//! [`named`](PackageManifest::named) table lists the objects the package
//! ships that boot does not load, each under a [`NamespacePath`], and boot
//! hands that table and [`package_objects_root`] to the read-only `objects`
//! file namespace. An actor mails `aether.fs.read { addr: { namespace:
//! "objects", path: <path> } }` and publishes the blob it is answered
//! with; the namespace reads the file named for the row's sha256. The
//! namespace checks at boot that every named object is present at its
//! recorded length, so a truncated install fails to boot. The read itself
//! is unverified, as boot's is.
//!
//! [`PackageManifest`] is the *persisted, versioned* shipping artifact: where
//! the JSON boot-manifest ([`crate::boot_manifest`]) names component files by
//! path, the package manifest references wasm + config bytes by content hash
//! into `pack/objects/`; identity is the hash everywhere and a
//! [`name`](PackageEntry::name) is a label, never a key. The chassis boots
//! by resolving those references against the local object store rather than
//! receiving inline bytes.
//!
//! ## Encoding
//!
//! The manifest is a hand-rolled little-endian binary format (magic,
//! iterative bounds-checked decode, no new serialization dependency — the
//! workspace owns its wire format, ADR-0118), with an explicit
//! [`MANIFEST_VERSION`] byte after the magic so a future layout change is
//! detectable at decode rather than misread. The layout, all integers
//! little-endian:
//!
//! - the 8-byte magic [`MANIFEST_MAGIC`];
//! - the one-byte [`MANIFEST_VERSION`];
//! - the four optional [`ChassisSettings`] (`title`, `window_mode` as
//!   optional strings; `tick_hz` as an optional `u32`; `clear_color` as an
//!   optional string — v2 appended it after `tick_hz`);
//! - a `u32` entry count;
//! - then per entry: the object hash (32 raw bytes), the optional config
//!   hash (a presence byte then 32 raw bytes), the optional `name` /
//!   `export` strings, and the optional `replicas` `u32`;
//! - a `u32` named-object count (v3 appended the table after the entries);
//! - then per named object, in strictly ascending path order: the path (a
//!   string with no presence byte), the object hash (32 raw bytes), and
//!   the size as a `u64`.
//!
//! Optional scalars/strings are a presence byte (0/1) then the value;
//! strings are a `u32` length then UTF-8 bytes. A named path is a
//! [`NamespacePath`], and a path that is malformed, repeated, or out of
//! order is a decode error, so one manifest has one byte image.
//!
//! ## Object resolution
//!
//! Boot resolves each hash against an [`ObjectStores`] — an ordered walk
//! over a list of `pack/objects` layers that holds exactly one entry in
//! this slice (ADR-0163 §1 is single-channel; the overlay-channel seam is
//! this ordered store list). A later overlay channel (mods, server-pushed
//! content) is a list append resolved first-hit in order, so the walk shape
//! is here but no layering machinery is built now.
//! Object integrity (does the file's content match its hash name) is the
//! platform's job — the store converges the disk toward the manifest by
//! hash — so boot reads the named file without re-hashing it.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str;

pub use aether_fs::{NamedObject, NamespacePath, NamespacePathError, Sha256, Sha256ParseError};
use aether_substrate::config::ConfigError;

use crate::autoload::{AutoloadComponent, expand_replicas};
use crate::boot_manifest::{ChassisSettings, PackedComponent};

/// The 8-byte magic opening every persisted package manifest.
pub const MANIFEST_MAGIC: &[u8; 8] = b"AEPKGMAN";

/// The on-disk manifest format version, a single byte after the magic.
/// Bumped on any incompatible change to the byte layout; the decoder
/// rejects an unrecognized version ([`ManifestDecodeError::UnsupportedVersion`])
/// rather than misreading newer bytes as v1.
pub const MANIFEST_VERSION: u8 = 3;

/// The `pack/` subdirectory of a package holding the manifest and objects.
const PACK_DIR: &str = "pack";
/// The manifest file within `pack/`.
const MANIFEST_FILE: &str = "manifest";
/// The immutable object directory within `pack/`.
const OBJECTS_DIR: &str = "objects";
/// The shipped asset tree within `pack/` — the root of the depot's `assets`
/// namespace.
const ASSETS_DIR: &str = "assets";

/// The `assets` namespace root a depot carries, or `None` when the package
/// ships no asset tree (`cargo xtask package` without `--assets`).
///
/// Returned rather than applied: precedence belongs to the boot path, which
/// slots this below argv/env/file and above the compiled default the way a
/// manifest's tick cadence and window mode are slotted (issue 4001), so an
/// operator's `AETHER_ASSETS_DIR` still wins over a shipped depot.
///
/// A `pack/assets` that exists but is a file rather than a directory is not
/// an asset tree, so it reads as absent — the fs cap would fail to root
/// there and the depot is better off with the ordinary default.
#[must_use]
pub fn package_assets_root(package_root: &Path) -> Option<PathBuf> {
    let assets = package_root.join(PACK_DIR).join(ASSETS_DIR);

    assets.is_dir().then_some(assets)
}

/// A package's object store, `<package>/pack/objects`: the directory boot
/// resolves manifest hashes against and the `objects` file namespace reads
/// a package's named objects from at run time.
///
/// Always a path, never absent: every package has an object store, and one
/// whose directory is missing holds no objects. Returned rather than
/// applied, as [`package_assets_root`] is, so the boot path keeps the
/// precedence.
#[must_use]
pub fn package_objects_root(package_root: &Path) -> PathBuf {
    package_root.join(PACK_DIR).join(OBJECTS_DIR)
}

/// A persisted package manifest: the chassis settings the package applies,
/// its hash-referenced component entries in autoload order, and the named
/// objects it ships for a running engine to read by path (ADR-0163 §1).
/// Reuses [`ChassisSettings`] (title / window mode / tick rate) so a
/// package carries the same three knobs the JSON boot manifest does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageManifest {
    /// Chassis settings (title / window mode / tick rate) the depot boot path
    /// applies BELOW argv/env, ABOVE the compiled defaults (issue 4001);
    /// surfaced alongside [`entries`](Self::entries) by [`package_autoload`].
    pub settings: ChassisSettings,
    /// The component entries, in autoload order.
    pub entries: Vec<PackageEntry>,
    /// The objects the package ships that boot checks for and does not
    /// load, by the path a running engine reads each at. Empty means none.
    pub named: BTreeMap<NamespacePath, NamedObject>,
}

/// One component entry in a [`PackageManifest`]: the object it loads plus
/// the optional config object and the load labels. Every byte payload is
/// referenced by hash into `pack/objects/`; the `Option` label fields are
/// the same ones `aether.component.load` carries (ADR-0096).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageEntry {
    /// The component wasm object — `pack/objects/<object>`.
    pub object: Sha256,
    /// Optional init-config object — `pack/objects/<config>` (ADR-0090).
    pub config: Option<Sha256>,
    /// Optional load name (`aether.component.load`'s `name`).
    pub name: Option<String>,
    /// Optional export selector (ADR-0096).
    pub export: Option<String>,
    /// Optional instance count (issue 2626): [`expand_replicas`] turns this
    /// into N counter-keyed instance spawns after one `Publish` of the
    /// module (issue #7155); `name` is the one instance's key for an
    /// unreplicated entry and is refused together with `replicas`.
    pub replicas: Option<u32>,
}

/// Encode `manifest` into the persisted `pack/manifest` bytes.
///
/// # Panics
///
/// Panics if a string field or the entry count exceeds 32 bits of length —
/// unreachable for any real package.
#[must_use]
pub fn encode_manifest(manifest: &PackageManifest) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MANIFEST_MAGIC);
    out.push(MANIFEST_VERSION);
    put_opt_string(&mut out, manifest.settings.title.as_deref());
    put_opt_string(&mut out, manifest.settings.window_mode.as_deref());
    put_opt_u32(&mut out, manifest.settings.tick_hz);
    put_opt_string(&mut out, manifest.settings.clear_color.as_deref());
    let count = u32::try_from(manifest.entries.len()).expect("package entry count fits in 32 bits");
    out.extend_from_slice(&count.to_le_bytes());
    for entry in &manifest.entries {
        out.extend_from_slice(&entry.object.0);
        match &entry.config {
            Some(config) => {
                out.push(1);
                out.extend_from_slice(&config.0);
            }
            None => out.push(0),
        }
        put_opt_string(&mut out, entry.name.as_deref());
        put_opt_string(&mut out, entry.export.as_deref());
        put_opt_u32(&mut out, entry.replicas);
    }
    let named = u32::try_from(manifest.named.len()).expect("package named-object count fits in 32 bits");
    out.extend_from_slice(&named.to_le_bytes());
    for (path, object) in &manifest.named {
        put_string(&mut out, path.as_str());
        out.extend_from_slice(&object.sha256.0);
        out.extend_from_slice(&object.size.to_le_bytes());
    }
    out
}

fn put_string(out: &mut Vec<u8>, value: &str) {
    let len = u32::try_from(value.len()).expect("manifest string length fits in 32 bits");
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(value.as_bytes());
}

fn put_opt_string(out: &mut Vec<u8>, value: Option<&str>) {
    match value {
        Some(s) => {
            out.push(1);
            put_string(out, s);
        }
        None => out.push(0),
    }
}

fn put_opt_u32(out: &mut Vec<u8>, value: Option<u32>) {
    match value {
        Some(n) => {
            out.push(1);
            out.extend_from_slice(&n.to_le_bytes());
        }
        None => out.push(0),
    }
}

/// A failure decoding a persisted package manifest. The manifest is
/// produced by the package build tooling (ADR-0163 §Consequences), so these
/// indicate a corrupt or version-skewed artifact, mapped to a hard boot
/// fault by [`package_autoload`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestDecodeError {
    /// The bytes don't open with [`MANIFEST_MAGIC`].
    BadMagic,
    /// The version byte is one this build doesn't understand (the value).
    UnsupportedVersion(u8),
    /// A length prefix or fixed field points past the end of the bytes.
    Truncated,
    /// A string field holds invalid UTF-8.
    BadUtf8,
    /// A named object's path is not a [`NamespacePath`] (the rule it broke).
    BadObjectPath(NamespacePathError),
    /// A named object's path is not strictly greater than the one before
    /// it: the table is unsorted or repeats a path.
    ObjectsOutOfOrder,
}

impl fmt::Display for ManifestDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic => write!(f, "package manifest does not start with {MANIFEST_MAGIC:?}"),
            Self::UnsupportedVersion(v) => {
                write!(f, "package manifest version {v} is not supported (this build reads v{MANIFEST_VERSION})")
            }
            Self::Truncated => write!(f, "package manifest is truncated"),
            Self::BadUtf8 => write!(f, "package manifest string field holds invalid UTF-8"),
            Self::BadObjectPath(source) => write!(f, "package manifest names an object at a bad path: {source}"),
            Self::ObjectsOutOfOrder => {
                write!(f, "package manifest named objects are not in strictly ascending path order")
            }
        }
    }
}

impl Error for ManifestDecodeError {}

/// Byte-slice reader for the iterative (non-recursive) manifest decode.
struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], ManifestDecodeError> {
        if len > self.rest.len() {
            return Err(ManifestDecodeError::Truncated);
        }
        let (head, tail) = self.rest.split_at(len);
        self.rest = tail;
        Ok(head)
    }

    fn take_u8(&mut self) -> Result<u8, ManifestDecodeError> {
        Ok(self.take(1)?[0])
    }

    fn take_u32(&mut self) -> Result<u32, ManifestDecodeError> {
        let raw = self.take(4)?;
        Ok(u32::from_le_bytes(raw.try_into().expect("4-byte slice")))
    }

    fn take_sha256(&mut self) -> Result<Sha256, ManifestDecodeError> {
        let raw = self.take(32)?;
        Ok(Sha256(raw.try_into().expect("32-byte slice")))
    }

    fn take_opt_sha256(&mut self) -> Result<Option<Sha256>, ManifestDecodeError> {
        if self.take_u8()? == 0 {
            return Ok(None);
        }
        Ok(Some(self.take_sha256()?))
    }

    fn take_u64(&mut self) -> Result<u64, ManifestDecodeError> {
        let raw = self.take(8)?;
        Ok(u64::from_le_bytes(raw.try_into().expect("8-byte slice")))
    }

    fn take_string(&mut self) -> Result<&'a str, ManifestDecodeError> {
        let len = self.take_u32()? as usize;
        str::from_utf8(self.take(len)?).map_err(|_| ManifestDecodeError::BadUtf8)
    }

    fn take_opt_string(&mut self) -> Result<Option<String>, ManifestDecodeError> {
        if self.take_u8()? == 0 {
            return Ok(None);
        }
        Ok(Some(self.take_string()?.to_owned()))
    }

    fn take_opt_u32(&mut self) -> Result<Option<u32>, ManifestDecodeError> {
        if self.take_u8()? == 0 {
            return Ok(None);
        }
        Ok(Some(self.take_u32()?))
    }
}

/// Decode `bytes` into a [`PackageManifest`].
///
/// # Errors
///
/// A [`ManifestDecodeError`] when the magic, version, a length prefix, a
/// string field, or the named-object table doesn't decode — see the variant
/// docs.
pub fn decode_manifest(bytes: &[u8]) -> Result<PackageManifest, ManifestDecodeError> {
    let mut reader = Reader { rest: bytes };
    if reader.take(MANIFEST_MAGIC.len())? != MANIFEST_MAGIC {
        return Err(ManifestDecodeError::BadMagic);
    }
    let version = reader.take_u8()?;
    if version != MANIFEST_VERSION {
        return Err(ManifestDecodeError::UnsupportedVersion(version));
    }
    let title = reader.take_opt_string()?;
    let window_mode = reader.take_opt_string()?;
    let tick_hz = reader.take_opt_u32()?;
    let clear_color = reader.take_opt_string()?;
    let count = reader.take_u32()?;
    // No `with_capacity(count)`: `count` is untrusted file input, so a bogus
    // large count must not preallocate — `take` fails fast when the body is
    // short of it.
    let mut entries = Vec::new();
    for _ in 0..count {
        let object = reader.take_sha256()?;
        let config = reader.take_opt_sha256()?;
        let name = reader.take_opt_string()?;
        let export = reader.take_opt_string()?;
        let replicas = reader.take_opt_u32()?;
        entries.push(PackageEntry { object, config, name, export, replicas });
    }
    // The count is untrusted here too, and the table is built row by row.
    // Each key goes through the one checked constructor, and a key that is
    // not strictly greater than the last is refused, so a repeat cannot
    // collapse silently and one table has one byte image.
    let named_count = reader.take_u32()?;
    let mut named = BTreeMap::new();
    for _ in 0..named_count {
        let path = NamespacePath::new(reader.take_string()?).map_err(ManifestDecodeError::BadObjectPath)?;
        let sha256 = reader.take_sha256()?;
        let size = reader.take_u64()?;
        if !follows_last(&named, &path) {
            return Err(ManifestDecodeError::ObjectsOutOfOrder);
        }
        named.insert(path, NamedObject { sha256, size });
    }
    Ok(PackageManifest { settings: ChassisSettings { title, window_mode, tick_hz, clear_color }, entries, named })
}

/// Whether `path` sorts strictly after every key already in `named`.
fn follows_last(named: &BTreeMap<NamespacePath, NamedObject>, path: &NamespacePath) -> bool {
    named.last_key_value().is_none_or(|(last, _)| last < path)
}

/// One package object source: the `pack/objects` directory of one package
/// layer. Objects are immutable, hash-named files read straight off disk.
pub struct ObjectStore {
    objects_dir: PathBuf,
}

impl ObjectStore {
    /// An object store over `objects_dir` (a `pack/objects` directory).
    #[must_use]
    pub fn new(objects_dir: PathBuf) -> Self {
        Self { objects_dir }
    }

    fn object_path(&self, hash: &Sha256) -> PathBuf {
        self.objects_dir.join(hash.to_hex())
    }

    /// Read the object's bytes, or `Ok(None)` when this layer doesn't hold
    /// it (so the caller can walk to the next layer). Any error other than
    /// "not found" surfaces as `Err`.
    fn read(&self, hash: &Sha256) -> io::Result<Option<Vec<u8>>> {
        match fs::read(self.object_path(hash)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// The ordered list of object stores boot resolves manifest references
/// against (ADR-0163 §1). Exactly one layer in this slice; a later overlay
/// channel is a [`push`](Self::push) append resolved first-hit by
/// [`read`](Self::read), so the walk exists without any layering machinery.
pub struct ObjectStores {
    layers: Vec<ObjectStore>,
}

impl ObjectStores {
    /// A single-layer store over one `pack/objects` directory — the
    /// single-channel package of this slice.
    #[must_use]
    pub fn single(objects_dir: PathBuf) -> Self {
        Self { layers: vec![ObjectStore::new(objects_dir)] }
    }

    /// Append a layer. [`read`](Self::read) walks in list order, so an
    /// appended layer is searched after the ones already present. Provided
    /// so a future overlay channel is a list append, not a redesign — no
    /// caller appends in this slice.
    pub fn push(&mut self, store: ObjectStore) {
        self.layers.push(store);
    }

    /// Resolve `hash` by walking the layers in order and returning the first
    /// that holds it.
    ///
    /// # Errors
    ///
    /// [`ObjectError::Missing`] when no layer holds the object with this hash;
    /// [`ObjectError::Io`] when a layer errors reading it.
    pub fn read(&self, hash: &Sha256) -> Result<Vec<u8>, ObjectError> {
        for layer in &self.layers {
            match layer.read(hash) {
                Ok(Some(bytes)) => return Ok(bytes),
                Ok(None) => {}
                Err(source) => return Err(ObjectError::Io { hash: *hash, source }),
            }
        }
        Err(ObjectError::Missing(*hash))
    }
}

/// A failure resolving an object against the package store.
#[derive(Debug)]
pub enum ObjectError {
    /// No store layer holds the object with this hash.
    Missing(Sha256),
    /// A store layer errored reading the object.
    Io { hash: Sha256, source: io::Error },
}

impl fmt::Display for ObjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(hash) => write!(f, "package object {hash} is not in the store"),
            Self::Io { hash, source } => write!(f, "read package object {hash}: {source}"),
        }
    }
}

impl Error for ObjectError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Missing(_) => None,
            Self::Io { source, .. } => Some(source),
        }
    }
}

/// A failure reading a package into its autoload component list — the file
/// I/O, decode, and object-resolution faults [`package_autoload`] maps to a
/// hard boot [`ConfigError`].
#[derive(Debug)]
pub enum PackageError {
    /// The `pack/manifest` file could not be read off disk.
    ReadManifest { path: PathBuf, source: io::Error },
    /// The `pack/manifest` bytes did not decode.
    Decode { path: PathBuf, source: ManifestDecodeError },
    /// An entry's object (or config object) could not be resolved.
    Object(ObjectError),
}

impl fmt::Display for PackageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadManifest { path, source } => {
                write!(f, "read package manifest from {}: {source}", path.display())
            }
            Self::Decode { path, source } => {
                write!(f, "decode package manifest at {}: {source}", path.display())
            }
            Self::Object(source) => write!(f, "{source}"),
        }
    }
}

impl Error for PackageError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ReadManifest { source, .. } => Some(source),
            Self::Decode { source, .. } => Some(source),
            Self::Object(source) => Some(source),
        }
    }
}

/// Read the persisted manifest of the package rooted at `package_root`
/// (`<package_root>/pack/manifest`) into a [`PackageManifest`] — the
/// object-resolving [`package_autoload`] starts here.
///
/// # Errors
///
/// [`PackageError::ReadManifest`] / [`PackageError::Decode`] when the file
/// is unreadable or its bytes don't decode.
pub fn read_manifest(package_root: &Path) -> Result<PackageManifest, PackageError> {
    let path = package_root.join(PACK_DIR).join(MANIFEST_FILE);
    let bytes = fs::read(&path).map_err(|source| PackageError::ReadManifest { path: path.clone(), source })?;
    decode_manifest(&bytes).map_err(|source| PackageError::Decode { path, source })
}

/// What a package gives boot: its chassis settings, the components boot
/// loads, and the named objects a running engine reads by path.
pub struct PackageBoot {
    /// The manifest's chassis settings.
    pub settings: ChassisSettings,
    /// The boot autoload list, replicas fanned out.
    pub components: Vec<AutoloadComponent>,
    /// The manifest's named objects, unchanged: the `objects` file
    /// namespace checks each is present and reads them.
    pub named: BTreeMap<NamespacePath, NamedObject>,
}

/// Read the package rooted at `package_root` into its [`ChassisSettings`],
/// the boot autoload component list, and its table of named objects
/// (ADR-0163 §1). Decodes `pack/manifest`, resolves each entry's object (and
/// optional config) bytes against the package's `pack/objects` store, then
/// fans out replicas through the shared [`expand_replicas`]. The named
/// objects are passed through as decoded: none is read or located here, and
/// the `objects` file namespace checks each when the chassis composes it.
///
/// Returns the manifest's [`ChassisSettings`] alongside the autoload list
/// (issue 4001): the depot boot path (`--package` / `AETHER_PACKAGE`) applies
/// title / window mode / tick rate BELOW argv/env, ABOVE the compiled defaults,
/// so a shipped package comes up titled and in its window mode while an
/// operator's `AETHER_WINDOW_*` / `--window-*` still overrides it.
/// `boot_manifest_autoload` — the hub-driven JSON channel — deliberately keeps
/// dropping its manifest's settings.
///
/// # Errors
///
/// A hard [`ConfigError`] (ADR-0090 §4: a known knob with a bad value
/// aborts boot loudly) when the manifest is unreadable, doesn't decode, an
/// object is missing, or a `replicas` fan-out is invalid.
pub fn package_autoload(package_root: &Path) -> Result<PackageBoot, ConfigError> {
    // Two error domains: `PackageError` (file / decode / object resolution)
    // and `ConfigError` (the replica fan-out). Resolve the packs first, map
    // that domain onto the boot fault, then expand replicas.
    let boot_fault =
        |error: PackageError| ConfigError::unparseable("AETHER_PACKAGE", package_root.display().to_string(), error);
    let PackageManifest { settings, entries, named } = read_manifest(package_root).map_err(boot_fault)?;
    let components = resolve_entries(entries, &ObjectStores::single(package_objects_root(package_root)))
        .map_err(boot_fault)?
        .into_iter()
        .map(expand_replicas)
        .collect::<Result<_, _>>()?;
    Ok(PackageBoot { settings, components, named })
}

/// Resolve a decoded manifest's `entries` against an [`ObjectStores`] into the
/// loaded [`PackedComponent`]s (object + config bytes pulled from the store),
/// preserving entry order — the object-reading step of [`package_autoload`].
///
/// # Errors
///
/// [`PackageError::Object`] when an entry's object or config object can't be
/// resolved against the store.
pub fn resolve_entries(
    entries: Vec<PackageEntry>,
    objects: &ObjectStores,
) -> Result<Vec<PackedComponent>, PackageError> {
    let mut packed = Vec::with_capacity(entries.len());
    for entry in entries {
        let wasm = objects.read(&entry.object).map_err(PackageError::Object)?;
        let config = match entry.config {
            Some(hash) => objects.read(&hash).map_err(PackageError::Object)?,
            None => Vec::new(),
        };
        packed.push(PackedComponent { wasm, config, name: entry.name, export: entry.export, replicas: entry.replicas });
    }
    Ok(packed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> PackageManifest {
        PackageManifest {
            settings: ChassisSettings {
                title: Some("hud".to_owned()),
                window_mode: None,
                tick_hz: Some(30),
                clear_color: Some("f6f2e9".to_owned()),
            },
            entries: vec![PackageEntry {
                object: Sha256([0xab; 32]),
                config: Some(Sha256([0xcd; 32])),
                name: Some("slime".to_owned()),
                export: None,
                replicas: Some(2),
            }],
            named: BTreeMap::from([(path("modules/a.wasm"), NamedObject { sha256: Sha256([0xef; 32]), size: 258 })]),
        }
    }

    fn path(text: &str) -> NamespacePath {
        NamespacePath::new(text).expect("test setup: a well-formed path")
    }

    #[test]
    fn round_trip_preserves_settings_and_entries() {
        // The hand-rolled encoder + decoder (not a derive) must be exact
        // inverses over the full field set — the bug this catches is an
        // encode/decode field-order or presence-byte mismatch that drops or
        // scrambles a manifest field.
        let manifest = PackageManifest {
            settings: ChassisSettings {
                title: Some("pkg".to_owned()),
                window_mode: Some("windowed:800x600".to_owned()),
                tick_hz: None,
                clear_color: Some("3f4b61".to_owned()),
            },
            entries: vec![
                PackageEntry {
                    object: Sha256([1; 32]),
                    config: Some(Sha256([2; 32])),
                    name: Some("a".to_owned()),
                    export: Some("alt".to_owned()),
                    replicas: Some(3),
                },
                PackageEntry { object: Sha256([9; 32]), config: None, name: None, export: None, replicas: None },
            ],
            named: BTreeMap::new(),
        };
        assert_eq!(decode_manifest(&encode_manifest(&manifest)).expect("decode"), manifest);
    }

    #[test]
    fn round_trip_empty_manifest() {
        let manifest =
            PackageManifest { settings: ChassisSettings::default(), entries: Vec::new(), named: BTreeMap::new() };
        assert_eq!(decode_manifest(&encode_manifest(&manifest)).expect("decode"), manifest);
    }

    #[test]
    fn encoded_bytes_match_pinned_layout() {
        // Tripwire: the persisted `pack/manifest` byte layout is a shipped
        // on-disk format. Any drift in field order, presence bytes, integer
        // endianness, or the magic/version header breaks every package
        // already built against the old layout, so the exact bytes are
        // pinned here — a change must be a deliberate `MANIFEST_VERSION`
        // bump, not an accident.
        let mut expected = Vec::new();
        expected.extend_from_slice(b"AEPKGMAN"); // magic
        expected.push(3); // MANIFEST_VERSION
        expected.extend_from_slice(&[0x01, 0x03, 0x00, 0x00, 0x00]); // title: present, len 3
        expected.extend_from_slice(b"hud");
        expected.push(0x00); // window_mode: absent
        expected.extend_from_slice(&[0x01, 0x1e, 0x00, 0x00, 0x00]); // tick_hz: present, 30
        expected.extend_from_slice(&[0x01, 0x06, 0x00, 0x00, 0x00]); // clear_color: present, len 6
        expected.extend_from_slice(b"f6f2e9");
        expected.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]); // entry count: 1
        expected.extend_from_slice(&[0xab; 32]); // object hash
        expected.push(0x01); // config: present
        expected.extend_from_slice(&[0xcd; 32]); // config hash
        expected.extend_from_slice(&[0x01, 0x05, 0x00, 0x00, 0x00]); // name: present, len 5
        expected.extend_from_slice(b"slime");
        expected.push(0x00); // export: absent
        expected.extend_from_slice(&[0x01, 0x02, 0x00, 0x00, 0x00]); // replicas: present, 2
        expected.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]); // named-object count: 1
        expected.extend_from_slice(&[0x0e, 0x00, 0x00, 0x00]); // path: len 14, no presence byte
        expected.extend_from_slice(b"modules/a.wasm");
        expected.extend_from_slice(&[0xef; 32]); // named object hash
        expected.extend_from_slice(&[0x02, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]); // size: 258
        assert_eq!(encode_manifest(&sample_manifest()), expected);
    }

    #[test]
    fn decode_refuses_a_malformed_or_unsorted_named_table() {
        // The hand-written decode must admit only what the path constructor
        // admits, and only one ordering of a table: the bugs are a path the
        // adapter would refuse reaching the table, and a repeated path
        // collapsing into one row without a word.
        let table = |paths: &[&str]| {
            let empty = PackageManifest { named: BTreeMap::new(), ..sample_manifest() };
            let mut bytes = encode_manifest(&empty);
            bytes.truncate(bytes.len() - 4);
            bytes.extend_from_slice(&u32::try_from(paths.len()).expect("a short table").to_le_bytes());
            for text in paths {
                put_string(&mut bytes, text);
                bytes.extend_from_slice(&[0x77; 32]);
                bytes.extend_from_slice(&5u64.to_le_bytes());
            }
            bytes
        };

        assert_eq!(decode_manifest(&table(&["a", "b"])).expect("an ascending table decodes").named.len(), 2);
        assert_eq!(
            decode_manifest(&table(&["modules/A.wasm"])),
            Err(ManifestDecodeError::BadObjectPath(NamespacePathError::Byte(b'A')))
        );
        assert_eq!(decode_manifest(&table(&["b", "a"])), Err(ManifestDecodeError::ObjectsOutOfOrder));
        assert_eq!(decode_manifest(&table(&["a", "a"])), Err(ManifestDecodeError::ObjectsOutOfOrder));
    }

    #[test]
    fn decode_rejects_bad_magic() {
        let mut bytes = encode_manifest(&sample_manifest());
        bytes[0] ^= 0xff;
        assert_eq!(decode_manifest(&bytes), Err(ManifestDecodeError::BadMagic));
    }

    #[test]
    fn decode_rejects_unsupported_version() {
        // A future format bumps the version byte; this build must refuse it
        // loudly rather than misread the newer bytes as v1.
        let mut bytes = encode_manifest(&sample_manifest());
        bytes[MANIFEST_MAGIC.len()] = MANIFEST_VERSION + 1;
        assert_eq!(decode_manifest(&bytes), Err(ManifestDecodeError::UnsupportedVersion(MANIFEST_VERSION + 1)),);
    }

    #[test]
    fn decode_rejects_truncation_at_every_length() {
        // Chopping the encoded sample anywhere short of its full length must
        // error — the bounds-checked reader never reads past the slice.
        let bytes = encode_manifest(&sample_manifest());
        for len in 0..bytes.len() {
            assert!(decode_manifest(&bytes[..len]).is_err(), "decode of {len}-byte prefix unexpectedly succeeded");
        }
    }

    #[test]
    fn object_stores_walk_first_hit_then_missing() {
        // The ordered walk returns the first layer holding the object and
        // errors Missing when no layer does — the resolution logic ADR-0163
        // §1's single-channel-today, list-append-later store owns.
        let dir = scratch_dir("objects");
        let first = dir.join("first");
        let second = dir.join("second");
        fs::create_dir_all(&first).expect("first dir");
        fs::create_dir_all(&second).expect("second dir");
        let shared = Sha256([0x11; 32]);
        let only_second = Sha256([0x22; 32]);
        let absent = Sha256([0x33; 32]);
        fs::write(first.join(shared.to_hex()), b"from-first").expect("write first");
        fs::write(second.join(shared.to_hex()), b"from-second").expect("write second-shared");
        fs::write(second.join(only_second.to_hex()), b"only-second").expect("write second-only");

        let mut stores = ObjectStores::single(first);
        stores.push(ObjectStore::new(second));
        assert_eq!(stores.read(&shared).expect("shared"), b"from-first"); // first layer wins
        assert_eq!(stores.read(&only_second).expect("only second"), b"only-second"); // walks on
        assert!(matches!(stores.read(&absent), Err(ObjectError::Missing(_))));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn package_autoload_resolves_objects_and_surfaces_settings() {
        // The end-to-end store-backed boot path: write a package directory
        // (manifest + hash-named objects), then prove `package_autoload`
        // decodes the manifest, surfaces its chassis settings, pulls each
        // object's bytes from the store, and fans out replicas. Two bugs this
        // catches: an entry resolved against the wrong hash or a replica
        // fan-out that drops an instance, AND the depot boot path dropping the
        // manifest's chassis settings (issue 4001 — the settings must reach the
        // caller so the chassis can apply title / window mode / tick rate).
        let root = scratch_dir("package");
        let objects = root.join(PACK_DIR).join(OBJECTS_DIR);
        fs::create_dir_all(&objects).expect("objects dir");
        let wasm = Sha256([0x44; 32]);
        let cfg = Sha256([0x55; 32]);
        fs::write(objects.join(wasm.to_hex()), [0x00, 0x61, 0x73, 0x6d]).expect("write wasm");
        fs::write(objects.join(cfg.to_hex()), [7, 8, 9]).expect("write cfg");
        let settings = ChassisSettings {
            title: Some("depot".to_owned()),
            window_mode: Some("windowed:640x480".to_owned()),
            tick_hz: Some(120),
            clear_color: Some("f6f2e9".to_owned()),
        };
        let manifest = PackageManifest {
            settings: settings.clone(),
            entries: vec![PackageEntry {
                object: wasm,
                config: Some(cfg),
                name: None,
                export: Some("handler".to_owned()),
                replicas: Some(2),
            }],
            named: BTreeMap::from([(path("modules/late.wasm"), NamedObject { sha256: Sha256([0x56; 32]), size: 7 })]),
        };
        fs::write(root.join(PACK_DIR).join(MANIFEST_FILE), encode_manifest(&manifest)).expect("write manifest");

        let PackageBoot { settings: returned_settings, components, named } = package_autoload(&root).expect("autoload");
        assert_eq!(returned_settings, settings, "the manifest's chassis settings surface to the depot boot path");
        assert_eq!(named, manifest.named, "the named objects reach boot as the manifest lists them");
        let [component] = components.as_slice() else {
            panic!("one entry is one module: {} components", components.len());
        };
        assert_eq!(component.keys, vec![None, None], "replicas: 2 is two counter keys");
        assert_eq!(component.namespace.as_deref(), Some("handler"));
        assert_eq!(component.wasm, vec![0x00, 0x61, 0x73, 0x6d]);
        assert_eq!(component.config, vec![7, 8, 9]);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn package_autoload_errors_on_missing_object() {
        // A manifest referencing an object the store doesn't hold is a hard
        // boot fault (ADR-0090 §4), not a silent skip.
        let root = scratch_dir("missing-object");
        fs::create_dir_all(root.join(PACK_DIR).join(OBJECTS_DIR)).expect("objects dir");
        let manifest = PackageManifest {
            settings: ChassisSettings::default(),
            entries: vec![PackageEntry {
                object: Sha256([0x66; 32]),
                config: None,
                name: Some("gone".to_owned()),
                export: None,
                replicas: None,
            }],
            named: BTreeMap::new(),
        };
        fs::write(root.join(PACK_DIR).join(MANIFEST_FILE), encode_manifest(&manifest)).expect("write manifest");

        assert!(package_autoload(&root).is_err(), "missing object must abort boot");

        fs::remove_dir_all(&root).ok();
    }

    /// A per-test scratch directory under the system temp dir, unique per
    /// call so concurrent test threads never collide.
    fn scratch_dir(tag: &str) -> PathBuf {
        use std::env;
        use std::process;
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!("aether-package-{tag}-{}-{seq}", process::id()));
        fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }
}
