//! A call's arguments decoded into its program's input, from the offered
//! input schema alone.
//!
//! Pure over the arguments and the schema. A refusal is a fixed sentence
//! naming the input's kind plus either a correction from the scalar walk in
//! [`coerce`] (which also reads a canonical decimal string as the integer
//! the schema wants, and names the variants a unit enum takes) or the
//! parser's or the codec's own message, so the same arguments always refuse
//! with the same text.

mod coerce;

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
    let mut value: Value = serde_json::from_str(arguments).map_err(|error| refusal(schema, &error))?;
    let shape = schema.schema();
    coerce::scalars(&mut value, &shape).map_err(|correction| refusal(schema, &correction))?;
    encode_storage_schema(&value, &shape).map_err(|error| refusal(schema, &error))
}

fn refusal(schema: &ToolSchema, error: &dyn Display) -> String {
    format!("The arguments do not decode as {}: {error}", schema.kind_name())
}

#[cfg(test)]
mod tests {
    use aether_bloomery_workspace::TreePath;
    use aether_data::{Storage, StorageData};

    use super::decode;
    use crate::tools::{GrepArgs, ReadArgs};

    const PREFIX: &str = "The arguments do not decode as muse.tree.read.args: ";

    fn read(arguments: &str) -> Result<Vec<u8>, String> {
        decode(arguments, &aether_bloomery_program::ToolSchema::of::<ReadArgs>())
    }

    fn encoded(args: ReadArgs) -> Vec<u8> {
        ReadArgs::encode_storage(&StorageData::from_value(args)).expect("the args encode")
    }

    fn readme() -> TreePath {
        TreePath::new("README").expect("a path")
    }

    #[test]
    fn integer_strings_decode_to_the_bytes_of_the_integers() {
        // Catches a coercion that lands the wrong value or width.
        let expected = encoded(ReadArgs::new(readme(), Some(3), Some(1)));
        assert_eq!(read(r#"{"path":"README","from_line":"3","lines":"1"}"#), Ok(expected.clone()));
        assert_eq!(read(r#"{"path":"README","from_line":3,"lines":1}"#), Ok(expected));
    }

    #[test]
    fn conforming_and_absent_optionals_are_undisturbed() {
        // Catches a walk that disturbs conforming or absent optional fields.
        assert_eq!(read(r#"{"path":"README"}"#), Ok(encoded(ReadArgs::new(readme(), None, None))));
        assert_eq!(read(r#"{"path":"README","from_line":null}"#), Ok(encoded(ReadArgs::new(readme(), None, None))));
    }

    #[test]
    fn a_mistyped_integer_is_refused_with_how_to_fix_it() {
        // Catches a lenient parse, `str::parse` accepting `+`, and a range check on the wrong width.
        let number = "`from_line` must be a JSON number, a whole number from 0 to 4294967295, e.g. `3`, not";
        for sent in [
            r#""three""#,
            r#""3.5""#,
            r#""-1""#,
            r#""+3""#,
            r#"" 3""#,
            r#""03""#,
            r#""""#,
            r#""4294967296""#,
            "3.5",
            "-1",
            "true",
            "4294967296",
        ] {
            let refused = read(&format!(r#"{{"path":"README","from_line":{sent}}}"#));
            assert_eq!(refused, Err(format!("{PREFIX}{number} `{sent}`")), "{sent}");
        }
    }

    #[test]
    fn a_non_string_at_a_string_field_is_refused_with_the_string_text() {
        // Catches a string field left for the codec's terse refusal.
        let refused = decode(r#"{"pattern": 7}"#, &aether_bloomery_program::ToolSchema::of::<GrepArgs>());
        assert_eq!(
            refused,
            Err("The arguments do not decode as muse.tree.grep.args: `pattern` must be a JSON string, e.g. `\"text\"`, not `7`"
                .to_owned())
        );
    }

    #[test]
    fn non_scalar_refusals_keep_the_parser_and_codec_text() {
        // Catches the walk preempting a refusal that is not about a scalar.
        assert!(read("not json").expect_err("refused").starts_with(PREFIX));
        assert!(read("[]").expect_err("refused").starts_with(PREFIX));
        assert!(!read("[]").expect_err("refused").contains("must be"));
        assert!(
            !read(r#"{"from_line":"3"}"#).expect_err("refused").contains("must be"),
            "a missing field is the codec's"
        );
    }
}
