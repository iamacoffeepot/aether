//! The boundary between Aether's wire vocabulary and the client's.
//!
//! Two walks live here, and they are deliberately a pair:
//!
//! - [`translate()`] turns an `aether_data::SchemaType` into a JSON Schema
//!   2020-12 document, at registration time. It decides what Aether shapes
//!   are *admissible* as a public tool contract.
//! - [`validate_client_value()`] checks a borrowed client JSON value against
//!   that same admitted subset, at request time, **before** `encode_schema`
//!   sees it.
//!
//! The second walk is not redundant with the codec. `aether-codec`
//! intentionally has a broader compatibility domain: its unit arm discards a
//! value without requiring null, its typed-identifier arm accepts a numeric
//! compatibility form, and its `F32` arm casts an `f64` without a
//! finite-range check. A public boundary that advertises a schema must reject
//! what that schema does not describe, so the validator enforces the
//! descriptor and `encode_schema` remains the only JSON-to-wire conversion.
//! No `schemars` and no general JSON Schema validator participates.
//!
//! Both walks are iterative with an explicit work stack and a budget. That is
//! the repository rule for a load-bearing walk over data that is not
//! structurally bounded by a small input file: `SchemaType` is public and
//! serializable, so a hand-authored tree can be arbitrarily deep, and a
//! client value is attacker-shaped by definition.

mod translate;
mod validate;
mod vocabulary;

#[cfg(test)]
mod tests;

pub use translate::{JSON_SCHEMA_DIALECT, translate, translate_tool_schema};
pub use validate::validate_client_value;

use std::error::Error;
use std::fmt;

/// Default nesting levels a schema tree may reach before translation
/// refuses it.
pub const DEFAULT_MAXIMUM_SCHEMA_DEPTH: usize = 128;
/// Default schema nodes a tree may contain before translation refuses it.
pub const DEFAULT_MAXIMUM_SCHEMA_NODES: usize = 16_384;

/// The bounds both walks run under.
///
/// Crossing either is an error, never a truncation: a partially translated
/// schema would advertise a contract the codec does not enforce, and a
/// partially validated value would reach `encode_schema` unchecked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchemaBudget {
    /// Maximum nesting depth, counting the root as depth 1.
    pub maximum_depth: usize,
    /// Maximum number of nodes visited.
    pub maximum_nodes: usize,
}

impl Default for SchemaBudget {
    fn default() -> Self {
        Self { maximum_depth: DEFAULT_MAXIMUM_SCHEMA_DEPTH, maximum_nodes: DEFAULT_MAXIMUM_SCHEMA_NODES }
    }
}

/// Why a `SchemaType` is not admissible as a public tool contract.
///
/// Every variant is a registration-time refusal — it becomes
/// `RegisterToolResult::Err`, not a runtime failure, so an inadmissible
/// descriptor never enters the catalog and no call can later discover that
/// the server admitted a shape it cannot execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaError {
    /// The tree is deeper than the budget allows.
    DepthExceeded { maximum: usize },
    /// The tree has more nodes than the budget allows.
    NodesExceeded { maximum: usize },
    /// Two fields of one `Struct` share a name. The derive cannot produce
    /// this; a hand-authored tree can, and it would make the JSON object
    /// ambiguous.
    DuplicateStructField { name: String },
    /// Two variants of one `Enum` share a name.
    DuplicateEnumVariant { name: String },
    /// Two variants of one `Enum` share a discriminant.
    DuplicateEnumDiscriminant { discriminant: u32 },
    /// The map's key type has no faithful, client-usable property-name
    /// representation.
    UnsupportedMapKey { reason: &'static str },
    /// The `TypeId` names a type this boundary cannot describe, so neither
    /// the translator nor the codec can state its JSON semantics.
    UnknownTypeId { type_id: u64 },
    /// A tool schema root that is neither `Unit` nor `Struct`. The 2025-06-18
    /// tool contract requires an object-shaped root.
    NonObjectRoot,
}

impl fmt::Display for SchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DepthExceeded { maximum } => write!(f, "schema nesting deeper than {maximum} levels"),
            Self::NodesExceeded { maximum } => write!(f, "schema larger than {maximum} nodes"),
            Self::DuplicateStructField { name } => write!(f, "duplicate struct field `{name}`"),
            Self::DuplicateEnumVariant { name } => write!(f, "duplicate enum variant `{name}`"),
            Self::DuplicateEnumDiscriminant { discriminant } => {
                write!(f, "duplicate enum discriminant `{discriminant}`")
            }
            Self::UnsupportedMapKey { reason } => write!(f, "unsupported map key: {reason}"),
            Self::UnknownTypeId { type_id } => write!(f, "unknown typed-identifier type `{type_id}`"),
            Self::NonObjectRoot => f.write_str("tool schema root must be a unit or named-field struct"),
        }
    }
}

impl Error for SchemaError {}

/// Why a client value does not conform to the admitted schema.
///
/// `path` is the dotted and indexed location inside the request value, so a
/// `-32602 Invalid params` response can name the offending member without
/// echoing the payload back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    /// Location of the offending value, e.g. `input.items[3].count`.
    pub path: String,
    /// What was expected there.
    pub reason: String,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.reason)
    }
}

impl Error for ValidationError {}
