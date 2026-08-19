//! Intent first line from the artifact route.

use std::str::from_utf8;

use serde_json::Value;

use crate::dto::DecodedArtifact;

/// First line of the intent's words, or `None` when the artifact has no prose.
#[must_use]
pub fn first_line(body: &DecodedArtifact) -> Option<String> {
    if let Some(value) = &body.value
        && let Some(line) = words_first_line(value)
    {
        return Some(line);
    }
    body.bytes.as_deref().and_then(bytes_first_line)
}

fn words_first_line(value: &Value) -> Option<String> {
    match value.get("words")? {
        Value::Array(items) => {
            let bytes: Option<Vec<u8>> =
                items.iter().map(|item| item.as_u64().and_then(|number| u8::try_from(number).ok())).collect();
            bytes_first_line(&bytes?)
        }
        Value::String(text) => text.lines().next().map(str::to_owned),
        _ => None,
    }
}

fn bytes_first_line(bytes: &[u8]) -> Option<String> {
    let text = from_utf8(bytes).ok()?;
    text.lines().next().filter(|line| !line.is_empty()).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::first_line;
    use crate::dto::DecodedArtifact;
    use serde_json::json;

    #[test]
    fn first_line_reads_statement_words_not_the_digest() {
        // The plausible bug: the list prints the intent digest (or the whole
        // JSON) instead of the first prose line the artifact route decoded.
        let body = DecodedArtifact {
            value: Some(json!({"words": b"ship the store\nmore".as_slice(), "parents": []})),
            ..DecodedArtifact::default()
        };
        assert_eq!(first_line(&body).as_deref(), Some("ship the store"));

        let raw = DecodedArtifact { bytes: Some(b"only line".to_vec()), ..DecodedArtifact::default() };
        assert_eq!(first_line(&raw).as_deref(), Some("only line"));
    }
}
