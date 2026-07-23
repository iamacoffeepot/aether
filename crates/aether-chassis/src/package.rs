//! Persisted package manifest + store-backed boot (ADR-0163 §1,
//! iamacoffeepot/aether#3967).
//!
//! The shippable form of an aether application is a directory:
//!
//! ```text
//! <package>/
//!   aether-substrate            # the chassis binary
//!   pack/manifest               # the one persisted manifest (this module)
//!   pack/objects/<sha256>       # component wasm + config bytes, immutable
//! ```
//!
//! [`PackageManifest`] supersedes the inline-bytes `bundle_pack` blob as a
//! *persisted, versioned* artifact — the "revisit if it ever becomes a
//! persisted artifact" the bundle-pack module doc named. Where the pack
//! carries wasm + config bytes inline, the manifest references them by
//! content hash into `pack/objects/`; identity is the hash everywhere and a
//! [`name`](PackageEntry::name) is a label, never a key. The chassis boots
//! by resolving those references against the local object store rather than
//! receiving inline bytes.
//!
//! ## Encoding
//!
//! The manifest is a hand-rolled little-endian binary format, mirroring the
//! `bundle_pack` discipline (magic, iterative bounds-checked decode, no new
//! serialization dependency — the workspace owns its wire format, ADR-0118)
//! but adding an explicit [`MANIFEST_VERSION`] byte after the magic so a
//! future layout change is detectable at decode rather than misread. The
//! layout, all integers little-endian:
//!
//! - the 8-byte magic [`MANIFEST_MAGIC`];
//! - the one-byte [`MANIFEST_VERSION`];
//! - the three optional [`ChassisSettings`] (`title`, `window_mode` as
//!   optional strings; `tick_hz` as an optional `u32`);
//! - a `u32` entry count;
//! - then per entry: the object hash (32 raw bytes), the optional config
//!   hash (a presence byte then 32 raw bytes), the optional `name` /
//!   `export` strings, and the optional `replicas` `u32`.
//!
//! Optional scalars/strings are a presence byte (0/1) then the value;
//! strings are a `u32` length then UTF-8 bytes.
//!
//! ## Object resolution
//!
//! Boot resolves each hash against an [`ObjectStores`] — an ordered walk
//! over a list of `pack/objects` layers that holds exactly one entry in
//! this slice (ADR-0163 §1 is single-channel). A later overlay channel
//! (mods, server-pushed content) is a list append resolved first-hit in
//! order, so the walk shape is here but no layering machinery is built now.
//! Object integrity (does the file's content match its hash name) is the
//! platform's job — the store converges the disk toward the manifest by
//! hash — so boot reads the named file without re-hashing it.

use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str;

use aether_substrate::config::ConfigError;

use crate::autoload::{AutoloadComponent, expand_replicas};
use crate::bundle_pack::{ChassisSettings, PackedComponent};

/// The 8-byte magic opening every persisted package manifest.
pub const MANIFEST_MAGIC: &[u8; 8] = b"AEPKGMAN";

/// The on-disk manifest format version, a single byte after the magic.
/// Bumped on any incompatible change to the byte layout; the decoder
/// rejects an unrecognized version ([`ManifestDecodeError::UnsupportedVersion`])
/// rather than misreading newer bytes as v1.
pub const MANIFEST_VERSION: u8 = 1;

/// The `pack/` subdirectory of a package holding the manifest and objects.
const PACK_DIR: &str = "pack";
/// The manifest file within `pack/`.
const MANIFEST_FILE: &str = "manifest";
/// The immutable object directory within `pack/`.
const OBJECTS_DIR: &str = "objects";

/// A sha256 content address — the identity of a package object (ADR-0163
/// §1). Encoded on the wire as its 32 raw bytes; rendered as lowercase hex
/// for the `pack/objects/<hash>` filename. The chassis never hashes bytes
/// itself (integrity is the platform's job) — it parses hashes out of the
/// manifest and reads the named file — so this newtype carries render/parse
/// but no digest computation.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sha256(pub [u8; 32]);

impl Sha256 {
    /// Render as lowercase hex — the `pack/objects/<hash>` filename.
    #[must_use]
    pub fn to_hex(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(self.0.len() * 2);
        for byte in self.0 {
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

    /// Parse a 64-character lowercase-or-uppercase hex string into a hash.
    ///
    /// # Errors
    ///
    /// [`Sha256ParseError::BadLength`] if the string is not 64 hex digits;
    /// [`Sha256ParseError::BadDigit`] if a character is not a hex digit.
    pub fn from_hex(s: &str) -> Result<Self, Sha256ParseError> {
        let bytes = s.as_bytes();
        if bytes.len() != 64 {
            return Err(Sha256ParseError::BadLength(bytes.len()));
        }
        let mut out = [0u8; 32];
        for (index, pair) in bytes.chunks_exact(2).enumerate() {
            let high = hex_digit(pair[0])?;
            let low = hex_digit(pair[1])?;
            out[index] = (high << 4) | low;
        }
        Ok(Self(out))
    }
}

fn hex_digit(byte: u8) -> Result<u8, Sha256ParseError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        other => Err(Sha256ParseError::BadDigit(other)),
    }
}

impl fmt::Display for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sha256({})", self.to_hex())
    }
}

/// A failure parsing a hex string into a [`Sha256`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sha256ParseError {
    /// The string was not exactly 64 hex digits (the found length).
    BadLength(usize),
    /// A character was not a hex digit (the offending byte).
    BadDigit(u8),
}

impl fmt::Display for Sha256ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadLength(len) => write!(f, "sha256 hex must be 64 digits, got {len}"),
            Self::BadDigit(byte) => write!(f, "sha256 hex holds a non-hex byte {byte:#04x}"),
        }
    }
}

impl Error for Sha256ParseError {}

/// A persisted package manifest: the chassis settings the package applies
/// plus its hash-referenced component entries, in autoload order (ADR-0163
/// §1). Reuses [`ChassisSettings`] (title / window mode / tick rate) so a
/// package carries the same three knobs the bundle pack does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageManifest {
    /// Chassis settings applied by the standalone-bundle path (a later
    /// slice); this runtime autoload path consumes only [`entries`](Self::entries).
    pub settings: ChassisSettings,
    /// The component entries, in autoload order.
    pub entries: Vec<PackageEntry>,
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
    /// Optional instance count (issue 2626): fanned out at boot into
    /// `{base}-{index}` instances by [`expand_replicas`].
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
    out
}

fn put_opt_string(out: &mut Vec<u8>, value: Option<&str>) {
    match value {
        Some(s) => {
            let len = u32::try_from(s.len()).expect("manifest string length fits in 32 bits");
            out.push(1);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(s.as_bytes());
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

    fn take_opt_string(&mut self) -> Result<Option<String>, ManifestDecodeError> {
        if self.take_u8()? == 0 {
            return Ok(None);
        }
        let len = self.take_u32()? as usize;
        let bytes = self.take(len)?;
        let s = str::from_utf8(bytes).map_err(|_| ManifestDecodeError::BadUtf8)?;
        Ok(Some(s.to_owned()))
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
/// A [`ManifestDecodeError`] when the magic, version, a length prefix, or a
/// string field doesn't decode — see the variant docs.
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
    Ok(PackageManifest { settings: ChassisSettings { title, window_mode, tick_hz }, entries })
}

/// A source of package objects addressed by content hash. The disk-backed
/// [`ObjectStores`] walk (the store-backed package boot) and the in-memory
/// [`EmbeddedObjectStore`] (the standalone-bundle embed) both implement it, so
/// one [`resolve_entries`] loop resolves a manifest's entries against either
/// source — the bundle bin and the package boot share one decode-and-resolve
/// path over two object sources.
pub trait ObjectSource {
    /// Resolve `hash` to its object bytes.
    ///
    /// # Errors
    ///
    /// [`ObjectError::Missing`] when the source holds no object with this hash;
    /// [`ObjectError::Io`] when reading it fails.
    fn read(&self, hash: &Sha256) -> Result<Vec<u8>, ObjectError>;
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

    /// Append a layer. [`ObjectSource::read`] walks in list order, so an
    /// appended layer is searched after the ones already present. Provided
    /// so a future overlay channel is a list append, not a redesign — no
    /// caller appends in this slice.
    pub fn push(&mut self, store: ObjectStore) {
        self.layers.push(store);
    }
}

impl ObjectSource for ObjectStores {
    /// Resolve `hash` by walking the layers in order and returning the first
    /// that holds it.
    fn read(&self, hash: &Sha256) -> Result<Vec<u8>, ObjectError> {
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

/// The in-memory object source backing the standalone-bundle embed path
/// (ADR-0163 §1): the objects the bundle bin `include_bytes!`es, each keyed by
/// its lowercase-hex hash filename. Resolving through the same
/// [`ObjectSource`] the disk [`ObjectStores`] implements is what lets the
/// bundle bin reuse the package boot's decode-and-resolve path rather than
/// carrying its own inline-bytes format.
pub struct EmbeddedObjectStore<'a> {
    objects: &'a [(&'a str, &'a [u8])],
}

impl<'a> EmbeddedObjectStore<'a> {
    /// An embedded store over the generated `(hex, bytes)` object table.
    #[must_use]
    pub fn new(objects: &'a [(&'a str, &'a [u8])]) -> Self {
        Self { objects }
    }
}

impl ObjectSource for EmbeddedObjectStore<'_> {
    fn read(&self, hash: &Sha256) -> Result<Vec<u8>, ObjectError> {
        let hex = hash.to_hex();
        self.objects
            .iter()
            .find(|(name, _)| *name == hex)
            .map(|(_, bytes)| bytes.to_vec())
            .ok_or(ObjectError::Missing(*hash))
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
/// object-resolving [`package_autoload`] and the settings-carrying
/// standalone-bundle path (a later slice) both start here.
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

/// Read the package rooted at `package_root` into the boot autoload
/// component list — the store-backed twin of
/// [`crate::autoload::boot_manifest_autoload`] (ADR-0163 §1). Decodes
/// `pack/manifest`, resolves each entry's object (and optional config)
/// bytes against the package's `pack/objects` store, then fans out replicas
/// through the shared [`expand_replicas`].
///
/// The manifest's [`ChassisSettings`] are carried by the persisted artifact
/// for the standalone-bundle path (a later slice); this runtime autoload
/// path consumes only the entries, exactly as `boot_manifest_autoload`
/// drops the boot manifest's own chassis settings.
///
/// # Errors
///
/// A hard [`ConfigError`] (ADR-0090 §4: a known knob with a bad value
/// aborts boot loudly) when the manifest is unreadable, doesn't decode, an
/// object is missing, or a `replicas` fan-out is invalid.
pub fn package_autoload(package_root: &Path) -> Result<Vec<AutoloadComponent>, ConfigError> {
    // Two error domains: `PackageError` (file / decode / object resolution)
    // and `ConfigError` (the replica fan-out). Resolve the packs first, map
    // that domain onto the boot fault, then expand replicas.
    let packed = read_and_resolve(package_root)
        .map_err(|e| ConfigError::unparseable("AETHER_PACKAGE", package_root.display().to_string(), e))?;
    let mut components = Vec::new();
    for entry in packed {
        components.extend(expand_replicas(entry)?);
    }
    Ok(components)
}

/// Resolve the package's entries to their loaded [`PackedComponent`]s
/// (object + config bytes pulled from the store), before replica fan-out.
fn read_and_resolve(package_root: &Path) -> Result<Vec<PackedComponent>, PackageError> {
    let manifest = read_manifest(package_root)?;
    let stores = ObjectStores::single(package_root.join(PACK_DIR).join(OBJECTS_DIR));
    resolve_entries(manifest.entries, &stores)
}

/// Resolve a decoded manifest's `entries` against an [`ObjectSource`] into the
/// loaded [`PackedComponent`]s (object + config bytes pulled from the source),
/// preserving entry order — the shared step of the disk-backed
/// [`package_autoload`] and the embedded [`embedded_autoload`].
///
/// # Errors
///
/// [`PackageError::Object`] when an entry's object or config object can't be
/// resolved against the source.
pub fn resolve_entries(
    entries: Vec<PackageEntry>,
    objects: &impl ObjectSource,
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

/// Decode an embedded package manifest and resolve its entries against an
/// embedded object source into the chassis settings plus the boot autoload
/// list (replicas fanned out) — the standalone-bundle twin of
/// [`package_autoload`] (ADR-0163 §1). The bundle bins call this on the
/// `include_bytes!`'d `pack/manifest` bytes and their embedded object table.
///
/// Unlike [`package_autoload`], this returns the manifest's [`ChassisSettings`]
/// alongside the autoload list: the standalone bundle bins apply title / window
/// mode / tick rate before booting (the hub-driven runtime autoload path drops
/// them). An empty `manifest_bytes` is a caller error, not "no package" — the
/// bundle bin guards the empty-embed placeholder before calling.
///
/// # Errors
///
/// A hard [`ConfigError`] (ADR-0090 §4: a known knob with a bad value aborts
/// boot loudly) when the manifest doesn't decode, an object is missing, or a
/// `replicas` fan-out is invalid.
pub fn embedded_autoload(
    manifest_bytes: &[u8],
    objects: &impl ObjectSource,
) -> Result<(ChassisSettings, Vec<AutoloadComponent>), ConfigError> {
    let manifest = decode_manifest(manifest_bytes)
        .map_err(|e| ConfigError::unparseable("embedded package manifest", "<embedded>", e))?;
    let settings = manifest.settings.clone();
    let packed = resolve_entries(manifest.entries, objects)
        .map_err(|e| ConfigError::unparseable("embedded package manifest", "<embedded>", e))?;
    let mut components = Vec::new();
    for entry in packed {
        components.extend(expand_replicas(entry)?);
    }
    Ok((settings, components))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> PackageManifest {
        PackageManifest {
            settings: ChassisSettings { title: Some("hud".to_owned()), window_mode: None, tick_hz: Some(30) },
            entries: vec![PackageEntry {
                object: Sha256([0xab; 32]),
                config: Some(Sha256([0xcd; 32])),
                name: Some("slime".to_owned()),
                export: None,
                replicas: Some(2),
            }],
        }
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
        };
        assert_eq!(decode_manifest(&encode_manifest(&manifest)).expect("decode"), manifest);
    }

    #[test]
    fn round_trip_empty_manifest() {
        let manifest = PackageManifest { settings: ChassisSettings::default(), entries: Vec::new() };
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
        expected.push(1); // MANIFEST_VERSION
        expected.extend_from_slice(&[0x01, 0x03, 0x00, 0x00, 0x00]); // title: present, len 3
        expected.extend_from_slice(b"hud");
        expected.push(0x00); // window_mode: absent
        expected.extend_from_slice(&[0x01, 0x1e, 0x00, 0x00, 0x00]); // tick_hz: present, 30
        expected.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]); // entry count: 1
        expected.extend_from_slice(&[0xab; 32]); // object hash
        expected.push(0x01); // config: present
        expected.extend_from_slice(&[0xcd; 32]); // config hash
        expected.extend_from_slice(&[0x01, 0x05, 0x00, 0x00, 0x00]); // name: present, len 5
        expected.extend_from_slice(b"slime");
        expected.push(0x00); // export: absent
        expected.extend_from_slice(&[0x01, 0x02, 0x00, 0x00, 0x00]); // replicas: present, 2
        assert_eq!(encode_manifest(&sample_manifest()), expected);
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
    fn sha256_hex_round_trips() {
        let hash = Sha256([0x0f; 32]);
        assert_eq!(hash.to_hex().len(), 64);
        assert_eq!(Sha256::from_hex(&hash.to_hex()).expect("parse"), hash);
    }

    #[test]
    fn sha256_from_hex_rejects_bad_input() {
        assert_eq!(Sha256::from_hex("abc"), Err(Sha256ParseError::BadLength(3)));
        let mut sixty_four = "0".repeat(63);
        sixty_four.push('z');
        assert_eq!(Sha256::from_hex(&sixty_four), Err(Sha256ParseError::BadDigit(b'z')));
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
    fn package_autoload_resolves_objects_into_components() {
        // The end-to-end store-backed boot path: write a package directory
        // (manifest + hash-named objects), then prove `package_autoload`
        // decodes the manifest, pulls each object's bytes from the store,
        // and fans out replicas — the bug this catches is an entry resolved
        // against the wrong hash or a replica fan-out that drops an instance.
        let root = scratch_dir("package");
        let objects = root.join(PACK_DIR).join(OBJECTS_DIR);
        fs::create_dir_all(&objects).expect("objects dir");
        let wasm = Sha256([0x44; 32]);
        let cfg = Sha256([0x55; 32]);
        fs::write(objects.join(wasm.to_hex()), [0x00, 0x61, 0x73, 0x6d]).expect("write wasm");
        fs::write(objects.join(cfg.to_hex()), [7, 8, 9]).expect("write cfg");
        let manifest = PackageManifest {
            settings: ChassisSettings::default(),
            entries: vec![PackageEntry {
                object: wasm,
                config: Some(cfg),
                name: Some("handler".to_owned()),
                export: None,
                replicas: Some(2),
            }],
        };
        fs::write(root.join(PACK_DIR).join(MANIFEST_FILE), encode_manifest(&manifest)).expect("write manifest");

        let components = package_autoload(&root).expect("autoload");
        assert_eq!(components.len(), 2, "replicas: 2 fans out to two components");
        for (index, component) in components.iter().enumerate() {
            assert_eq!(component.wasm, vec![0x00, 0x61, 0x73, 0x6d]);
            assert_eq!(component.config, vec![7, 8, 9]);
            assert_eq!(component.name.as_deref(), Some(format!("handler-{index}").as_str()));
        }

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn embedded_autoload_resolves_in_memory_objects_and_settings() {
        // The standalone-bundle embed path: `decode_manifest` over the
        // `include_bytes!`'d manifest, entries resolved against the in-memory
        // `EmbeddedObjectStore` (not disk), settings surfaced, replicas fanned
        // out. The bug this catches is the embed path resolving an entry
        // against the wrong hash, dropping the manifest's chassis settings, or
        // losing a replica instance — the object source is in memory here, so
        // it exercises exactly what the bundle bin runs with no disk store.
        let wasm = Sha256([0x77; 32]);
        let cfg = Sha256([0x88; 32]);
        let manifest = PackageManifest {
            settings: ChassisSettings { title: Some("bundle".to_owned()), window_mode: None, tick_hz: Some(30) },
            entries: vec![PackageEntry {
                object: wasm,
                config: Some(cfg),
                name: Some("probe".to_owned()),
                export: None,
                replicas: Some(2),
            }],
        };
        let manifest_bytes = encode_manifest(&manifest);
        let (wasm_hex, cfg_hex) = (wasm.to_hex(), cfg.to_hex());
        let objects: &[(&str, &[u8])] =
            &[(wasm_hex.as_str(), &[0x00, 0x61, 0x73, 0x6d]), (cfg_hex.as_str(), &[1, 2, 3])];
        let store = EmbeddedObjectStore::new(objects);

        let (settings, components) = embedded_autoload(&manifest_bytes, &store).expect("embedded autoload");
        assert_eq!(settings, manifest.settings, "the manifest's chassis settings surface to the bundle bin");
        assert_eq!(components.len(), 2, "replicas: 2 fans out to two components");
        for (index, component) in components.iter().enumerate() {
            assert_eq!(component.wasm, vec![0x00, 0x61, 0x73, 0x6d]);
            assert_eq!(component.config, vec![1, 2, 3]);
            assert_eq!(component.name.as_deref(), Some(format!("probe-{index}").as_str()));
        }
    }

    #[test]
    fn embedded_autoload_errors_on_missing_object() {
        // A manifest referencing an object the embedded table doesn't hold is a
        // hard boot fault, exactly as the disk path is (ADR-0090 §4).
        let manifest = PackageManifest {
            settings: ChassisSettings::default(),
            entries: vec![PackageEntry {
                object: Sha256([0x99; 32]),
                config: None,
                name: Some("gone".to_owned()),
                export: None,
                replicas: None,
            }],
        };
        let store = EmbeddedObjectStore::new(&[]);
        assert!(embedded_autoload(&encode_manifest(&manifest), &store).is_err(), "missing object must abort boot");
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
