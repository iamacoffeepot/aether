//! A declared program rendered as a responses-API function tool.
//!
//! A tool is a program to run: its name is the program's name, its
//! description is the program's `///` doc, and its parameters are the JSON
//! Schema of the JSON `aether-codec` accepts for the program's input, with
//! each field's and variant's `///` doc attached. Rendering needs the
//! program's crate linked: the input's schema is read from its type, not
//! from the declaration record.
//!
//! [`ToolSchema`] is the same type's schema as a stored value, so a program
//! that links none of a tool's types can still decode its arguments and
//! render its result: the caller, which links them, stores one for the
//! tool's input and one for its result and cites both.

use alloc::string::String;
use alloc::vec::Vec;
use core::error::Error as StdError;
use core::fmt;

use aether_codec::{JsonSchemaError, json_schema};
use aether_data::wire::{decode_from_slice, encode_to_vec};
use aether_data::{KindId, Schema, SchemaType, StaticSchema, Storage, require_documented, storage_kind_id_from_name};
use serde_json::{Value, json};

use crate::Program;
use crate::kinds::ProgramName;

/// The longest function name the responses API accepts, in bytes.
pub const MAX_FUNCTION_NAME_BYTES: usize = 64;

/// Why a program could not be rendered as a tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolDefinitionError {
    /// The program's `NAME` is not a valid [`ProgramName`]. `#[program]`
    /// refuses this at compile time; only a hand-written impl reaches it.
    InvalidName,
    /// The mapped function name is longer than [`MAX_FUNCTION_NAME_BYTES`].
    NameTooLong,
    /// The input's schema has no JSON Schema form.
    Schema(JsonSchemaError),
}

impl fmt::Display for ToolDefinitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName => f.write_str("the program NAME is not a valid program name"),
            Self::NameTooLong => {
                write!(f, "the program's function name is longer than {MAX_FUNCTION_NAME_BYTES} bytes")
            }
            Self::Schema(error) => write!(f, "the program input has no JSON Schema form: {error}"),
        }
    }
}

impl StdError for ToolDefinitionError {}

impl From<JsonSchemaError> for ToolDefinitionError {
    fn from(error: JsonSchemaError) -> Self {
        Self::Schema(error)
    }
}

/// Render `P` as a responses-API function tool:
/// `{ "type": "function", "name", "description", "parameters", "strict": false }`.
///
/// `strict` is off because the parameters use `oneOf` for enum variants and
/// leave `Option` fields out of `required`, which strict mode refuses; the
/// codec's decode stays the authority on what the program accepts.
///
/// An input with an undocumented field or variant does not compile here,
/// the same check `#[program]` runs, so a hand-written `impl Program` is
/// refused too.
///
/// # Errors
///
/// [`ToolDefinitionError`] when the name is invalid or too long, or the
/// input's schema has no JSON Schema form.
pub fn tool_definition<P: Program>() -> Result<Value, ToolDefinitionError>
where
    P::Input: Schema,
{
    const {
        require_documented(
            StaticSchema::<P::Input>::SCHEMA,
            StaticSchema::<P::Input>::DOC_NODE,
            "a program input must be a struct whose fields and variants all carry a `///` doc",
        );
    }
    let program = ProgramName::new(P::NAME).map_err(|_| ToolDefinitionError::InvalidName)?;
    let parameters = json_schema(&<P::Input as Schema>::SCHEMA, &<P::Input as Schema>::DOC_NODE)?;
    Ok(json!({
        "type": "function",
        "name": function_name(&program)?,
        "description": P::DOC,
        "parameters": parameters,
        "strict": false,
    }))
}

/// The function name for `program`: its dots mapped to dashes, the
/// characters the API allows. A program name's segments never hold a dash,
/// so the mapping is injective and [`program_name`] inverts it.
///
/// # Errors
///
/// [`ToolDefinitionError::NameTooLong`] past [`MAX_FUNCTION_NAME_BYTES`].
pub fn function_name(program: &ProgramName) -> Result<String, ToolDefinitionError> {
    let function = program.as_str().replace('.', "-");
    if function.len() > MAX_FUNCTION_NAME_BYTES {
        return Err(ToolDefinitionError::NameTooLong);
    }
    Ok(function)
}

/// The program a function name calls, or `None` when it names none.
#[must_use]
pub fn program_name(function: &str) -> Option<ProgramName> {
    ProgramName::new(function.replace('-', ".")).ok()
}

/// One storage kind's schema as data: the kind's name and its
/// [`SchemaType`], for a reader that links no Rust type of the kind.
///
/// Built only by [`Self::of`], from the type itself. The schema is stored as
/// its `aether_data::wire` bytes and re-parsed on every decode, so a stored
/// schema that does not parse is refused.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "bloomery.program.tool_schema")]
pub struct ToolSchema {
    /// The storage kind's name; its id is the name's hash.
    kind: String,
    /// The kind's schema.
    schema: SchemaBytes,
}

impl ToolSchema {
    /// The schema of storage kind `K`.
    ///
    /// # Panics
    ///
    /// Never for a schema the compiler can hold: only a length past the
    /// wire's `u32` ceiling fails to encode.
    #[must_use]
    pub fn of<K: Storage + Schema>() -> Self {
        let bytes = encode_to_vec(&K::SCHEMA).expect("a static schema's lengths fit the wire's u32 ceiling");
        Self { kind: K::NAME.into(), schema: SchemaBytes(bytes) }
    }

    /// The storage kind's name.
    #[must_use]
    pub fn kind_name(&self) -> &str {
        &self.kind
    }

    /// The storage kind's id, which heads every stored value of it.
    #[must_use]
    pub fn kind_id(&self) -> KindId {
        storage_kind_id_from_name(&self.kind)
    }

    /// The kind's schema.
    ///
    /// # Panics
    ///
    /// Never: the bytes parsed when this value was built or decoded.
    #[must_use]
    pub fn schema(&self) -> SchemaType {
        decode_from_slice(&self.schema.0).expect("the schema bytes parsed when this value was built or decoded")
    }
}

/// Why a stored [`ToolSchema`]'s schema bytes were refused on decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SchemaBytesError {
    /// The bytes are not one whole wire-encoded `SchemaType`.
    Unparsed,
}

impl aether_data::Invariant for SchemaBytesError {
    fn reason(&self) -> &'static str {
        match self {
            Self::Unparsed => "unparsed-schema",
        }
    }
}

/// A [`SchemaType`]'s wire bytes that parse as one.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
struct SchemaBytes(Vec<u8>);

impl SchemaBytes {
    // `#[storage(validate)]` calls `check(&inner)` on every decode path.
    fn check(bytes: &[u8]) -> Result<(), SchemaBytesError> {
        decode_from_slice::<SchemaType>(bytes).map(drop).map_err(|_| SchemaBytesError::Unparsed)
    }
}

#[cfg(test)]
mod tests {
    use alloc::format;
    use alloc::vec;

    use aether_data::{Storage, StorageData};

    use super::{SchemaBytes, ToolDefinitionError, ToolSchema, function_name, program_name};
    use crate::kinds::{ProgramName, Tree};

    #[test]
    fn function_names_map_back_to_their_program() {
        // Catches a lossy mapping that would resolve a model's call to the wrong program.
        for name in ["muse.turn", "a1.b_2.c", "test.fixture.read_large_2"] {
            let program = ProgramName::new(name).expect("valid program name");
            let function = function_name(&program).expect("short enough");
            assert!(!function.contains('.'), "{function}");
            assert_eq!(program_name(&function), Some(program));
        }
        assert_eq!(program_name("not a program"), None);
    }

    #[test]
    fn a_function_name_past_the_api_limit_is_refused() {
        // Catches a name the API would reject being offered as a tool.
        let fits = ProgramName::new(format!("a.{}", "b".repeat(62))).expect("valid program name");
        assert_eq!(function_name(&fits).map(|name| name.len()), Ok(64));
        let long = ProgramName::new(format!("a.{}", "b".repeat(63))).expect("valid program name");
        assert_eq!(function_name(&long), Err(ToolDefinitionError::NameTooLong));
    }

    #[test]
    fn a_stored_tool_schema_whose_schema_does_not_parse_refuses_on_decode() {
        // Catches a dropped `#[storage(validate)]`, which would let a schema no reader can parse in through the
        // journal and panic the reader that trusts it.
        let stored = |schema: ToolSchema| ToolSchema::encode_storage(&StorageData::from_value(schema)).expect("encode");
        let decoded = |bytes: &[u8]| ToolSchema::decode_storage(bytes).map(|data| data.value);

        let valid = ToolSchema::of::<Tree>();
        assert_eq!(decoded(&stored(valid.clone())).ok(), Some(valid), "a parsed schema decodes");
        let unparsed = ToolSchema { kind: "bloomery.tree".into(), schema: SchemaBytes(vec![0xff; 4]) };
        assert!(decoded(&stored(unparsed)).is_err(), "an unparsed schema refuses");
    }
}
