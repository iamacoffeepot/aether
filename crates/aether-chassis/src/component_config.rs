//! Authoring a component's init-config as JSON (ADR-0090 + ADR-0028).
//!
//! Init-config reaches a component as opaque bytes: the wire image of the
//! `Config` kind its `#[actor]` block declares. That is the right shape for
//! a machine — the hub encodes JSON once and stages the bytes for
//! `spawn_substrate` — and the wrong shape for a file a person is expected
//! to read, review, and check in. A checked-in depot spec or boot manifest
//! that names a blob of wire bytes is unreviewable, and it silently rots the
//! moment the config kind grows a field.
//!
//! So a manifest entry may name a **JSON** config file instead, and the
//! bytes are produced here: the component's own wasm carries the schema of
//! the `Config` kind it declares (`aether.kinds` + `aether.kinds.inputs`,
//! ADR-0028 / ADR-0033), so the encode is checked against the exact
//! contract the actor about to be instantiated will decode with. A field
//! that does not exist, or a value of the wrong type, fails where the file
//! is read rather than as a decode error inside the guest.
//!
//! This is the same path `aether-mcp` takes for `load_component`'s
//! `config` / `config_path`; it lives here because both the JSON boot
//! manifest ([`crate::boot_manifest`]) and the depot spec that `cargo xtask
//! package` reads need it, and this crate is the one they share.

use std::error::Error;
use std::fmt;

use aether_data::canonical::kind_id_from_parts;
use aether_substrate::actor::wasm::kind_manifest;

/// Why a JSON init-config could not be turned into config bytes.
#[derive(Debug)]
pub enum ConfigJsonError {
    /// The JSON text did not parse.
    Parse(serde_json::Error),
    /// The component's embedded kind manifest could not be read.
    Manifest(String),
    /// The named `export` is not one of the module's actor types.
    UnknownExport(String),
    /// The selected actor declares no `Config` kind, so it takes no
    /// init-config at all.
    NoConfigKind,
    /// The actor names a config kind whose schema is absent from the
    /// module's `aether.kinds` section — a component built with mismatched
    /// sections.
    SchemaAbsent(String),
    /// The JSON did not match the config kind's schema.
    Encode { kind: String, source: aether_codec::EncodeError },
}

impl fmt::Display for ConfigJsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(source) => write!(f, "config JSON does not parse: {source}"),
            Self::Manifest(reason) => write!(f, "read the component's kind manifest: {reason}"),
            Self::UnknownExport(export) => write!(f, "export {export:?} is not declared in the component"),
            Self::NoConfigKind => f.write_str("the component declares no Config kind, so it takes no init-config"),
            Self::SchemaAbsent(kind) => write!(f, "the component declares config kind {kind} but not its schema"),
            Self::Encode { kind, source } => write!(f, "config JSON does not match {kind}: {source}"),
        }
    }
}

impl Error for ConfigJsonError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Parse(source) => Some(source),
            Self::Encode { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Encode `json` into the init-config bytes the actor selected by `export`
/// (or the module's entry actor, when `export` is `None`) decodes at `init`.
///
/// # Errors
///
/// A [`ConfigJsonError`] when the JSON does not parse, the component's kind
/// manifest is unreadable, the export is unknown, the selected actor takes
/// no config, or the JSON does not match the config kind's schema.
pub fn encode_config_json(wasm: &[u8], export: Option<&str>, json: &str) -> Result<Vec<u8>, ConfigJsonError> {
    let value = serde_json::from_str(json).map_err(ConfigJsonError::Parse)?;
    let capabilities = match export {
        Some(export) => kind_manifest::read_actor_inputs_from_bytes(wasm)
            .map_err(ConfigJsonError::Manifest)?
            .into_iter()
            .find(|actor| actor.namespace.as_deref() == Some(export))
            .map(|actor| actor.capabilities)
            .ok_or_else(|| ConfigJsonError::UnknownExport(export.to_owned()))?,
        None => kind_manifest::read_inputs_from_bytes(wasm).map_err(ConfigJsonError::Manifest)?,
    };
    let config = capabilities.config.ok_or(ConfigJsonError::NoConfigKind)?;

    let descriptor = kind_manifest::read_from_bytes(wasm)
        .map_err(ConfigJsonError::Manifest)?
        .into_iter()
        .find(|descriptor| kind_id_from_parts(&descriptor.name, &descriptor.schema) == config.id.0)
        .ok_or_else(|| ConfigJsonError::SchemaAbsent(config.name.clone()))?;

    aether_codec::encode_schema(&value, &descriptor.schema)
        .map_err(|source| ConfigJsonError::Encode { kind: descriptor.name, source })
}
