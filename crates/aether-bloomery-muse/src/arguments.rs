//! A call's arguments decoded into its program's input, from the offered
//! input schema alone.
//!
//! Pure over the arguments and the schema. A refusal is a fixed sentence
//! naming the input's kind plus the parser's or the codec's own message, so
//! the same arguments always refuse with the same text.

use core::fmt::Display;

use aether_bloomery_program::ToolSchema;
use aether_codec::encode_storage_schema;
use serde_json::Value;

/// The storage payload of `arguments` decoded as `schema`'s kind, or the text
/// of the refusal to send back as the call's output.
///
/// # Errors
///
/// The refusal text when `arguments` is not JSON, or is JSON the schema does
/// not accept.
pub fn decode(arguments: &str, schema: &ToolSchema) -> Result<Vec<u8>, String> {
    let value: Value = serde_json::from_str(arguments).map_err(|error| refusal(schema, &error))?;
    encode_storage_schema(&value, &schema.schema()).map_err(|error| refusal(schema, &error))
}

fn refusal(schema: &ToolSchema, error: &dyn Display) -> String {
    format!("The arguments do not decode as {}: {error}", schema.kind_name())
}
