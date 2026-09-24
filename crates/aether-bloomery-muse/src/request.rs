//! The one `Fetch` a turn sends: the whole conversation, stateless.
//!
//! Pure over the input and its read texts, so the request the recorded
//! closure describes is testable without the invocation machinery. The
//! program sets exactly one header and no credential.

use aether_http::{Fetch, HttpHeader, HttpMethod};
use serde::Serialize;

use crate::input::{ReasoningEffort, Role, TurnInput};

/// How long the HTTP capability waits for the vendor before it answers `Timeout`.
const TURN_TIMEOUT_MILLIS: u32 = 180_000;

#[derive(Serialize)]
struct Body<'a> {
    model: &'a str,
    store: bool,
    max_output_tokens: u32,
    reasoning: Reasoning,
    input: Vec<Item<'a>>,
}

#[derive(Serialize)]
struct Reasoning {
    effort: &'static str,
}

#[derive(Serialize)]
struct Item<'a> {
    role: &'static str,
    content: [Part<'a>; 1],
}

#[derive(Serialize)]
struct Part<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
}

impl<'a> Item<'a> {
    const fn new(role: Role, text: &'a str) -> Self {
        let (role, kind) = match role {
            Role::Developer => ("developer", "input_text"),
            Role::User => ("user", "input_text"),
            Role::Assistant => ("assistant", "output_text"),
        };
        Self { role, content: [Part { kind, text }] }
    }
}

const fn effort(reasoning: ReasoningEffort) -> &'static str {
    match reasoning {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
    }
}

/// Build the turn's request. `texts[i]` is the read text of `input.items()[i]`.
pub fn fetch(input: &TurnInput, texts: &[String]) -> Fetch {
    let body = Body {
        model: input.model().as_str(),
        store: false,
        max_output_tokens: input.max_output_tokens().get(),
        reasoning: Reasoning { effort: effort(input.reasoning()) },
        input: input.items().iter().zip(texts).map(|(item, text)| Item::new(item.role(), text)).collect(),
    };

    Fetch {
        request_id: 1,
        url: input.endpoint().as_str().into(),
        method: HttpMethod::Post,
        headers: vec![HttpHeader { name: "Content-Type".into(), value: "application/json".into() }],
        body: serde_json::to_vec(&body).expect("a body of strings and integers always serializes"),
        timeout_ms: Some(TURN_TIMEOUT_MILLIS),
    }
}

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::Ref;
    use aether_http::{HttpHeader, HttpMethod};
    use serde_json::json;

    use super::{TURN_TIMEOUT_MILLIS, fetch};
    use crate::input::{Endpoint, ModelName, OutputBudget, ReasoningEffort, Role, TurnInput, TurnItem, TurnItems};

    #[test]
    fn request_resends_every_item_in_order_with_store_off() {
        // Catches dropped or reordered items, the wrong part type on assistant items, `store` left on, a
        // conversation handle, an extra header, and the wrong method, URL, or timeout.
        let texts = ["Be brief.", "What is a bloom?", "A flowering.", "And a bloomery?"].map(String::from);
        let roles = [Role::Developer, Role::User, Role::Assistant, Role::User];
        let items = roles.iter().zip(&texts).map(|(&role, text)| TurnItem::new(role, Ref::of_text(text))).collect();
        let input = TurnInput::new(
            Endpoint::new("https://example.test/v1/responses").expect("endpoint"),
            ModelName::new("muse-spark-1.3").expect("model"),
            TurnItems::new(items).expect("items"),
            OutputBudget::new(512).expect("budget"),
            ReasoningEffort::Medium,
        );

        let request = fetch(&input, &texts);

        assert_eq!(request.url, "https://example.test/v1/responses");
        assert_eq!(request.method, HttpMethod::Post);
        assert_eq!(request.timeout_ms, Some(TURN_TIMEOUT_MILLIS));
        assert_eq!(
            request.headers,
            vec![HttpHeader { name: "Content-Type".into(), value: "application/json".into() }],
            "exactly one header, and no credential"
        );
        let body: serde_json::Value = serde_json::from_slice(&request.body).expect("body is JSON");
        assert_eq!(
            body,
            json!({
                "model": "muse-spark-1.3",
                "store": false,
                "max_output_tokens": 512,
                "reasoning": { "effort": "medium" },
                "input": [
                    { "role": "developer", "content": [{ "type": "input_text", "text": "Be brief." }] },
                    { "role": "user", "content": [{ "type": "input_text", "text": "What is a bloom?" }] },
                    { "role": "assistant", "content": [{ "type": "output_text", "text": "A flowering." }] },
                    { "role": "user", "content": [{ "type": "input_text", "text": "And a bloomery?" }] },
                ],
            })
        );
    }
}
