//! A declared program rendered as a responses-API function tool.
//!
//! A tool is a program to run: its name is the program's name, its
//! description is the program's `///` doc, and its parameters are the JSON
//! Schema of the JSON `aether-codec` accepts for the program's input, with
//! each field's and variant's `///` doc attached. Rendering needs the
//! program's crate linked: the input's schema is read from its type, not
//! from the declaration record.

use alloc::string::String;
use core::error::Error as StdError;
use core::fmt;

use aether_codec::{JsonSchemaError, json_schema};
use aether_data::{Schema, StaticSchema, require_documented};
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

#[cfg(test)]
mod tests {
    use alloc::format;

    use super::{ToolDefinitionError, function_name, program_name};
    use crate::kinds::ProgramName;

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
}
