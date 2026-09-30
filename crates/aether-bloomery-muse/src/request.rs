//! The one `Fetch` a turn sends: the offered tools and the whole
//! conversation, stateless.
//!
//! Pure over the input and its read and rendered texts, so the request the recorded
//! closure describes is testable without the invocation machinery. The
//! program sets exactly one header and no credential.

use std::borrow::Cow;

use aether_bloomery_kinds::{Detail, ProgramName, Refusal};
use aether_bloomery_program::function_name;
use aether_http::{Fetch, HttpHeader, HttpMethod};
use serde::Serialize;
use serde_json::Value;

use crate::input::{OfferedTool, ReasoningEffort, Role, TurnInput, TurnItem};

/// How long the HTTP capability waits for the vendor before it answers `Timeout`.
const TURN_TIMEOUT_MILLIS: u32 = 180_000;

#[derive(Serialize)]
struct Body<'a> {
    model: &'a str,
    store: bool,
    max_output_tokens: u32,
    reasoning: Reasoning,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<Value>,
    input: Vec<Item<'a>>,
}

#[derive(Serialize)]
struct Reasoning {
    effort: &'static str,
}

/// One responses-API input item. A message carries no `type`, as before
/// tools; a replayed call and its output carry theirs.
#[derive(Serialize)]
#[serde(untagged)]
enum Item<'a> {
    Message {
        role: &'static str,
        content: [Part<'a>; 1],
    },
    FunctionCall {
        #[serde(rename = "type")]
        kind: &'static str,
        call_id: &'a str,
        name: Cow<'a, str>,
        arguments: &'a str,
    },
    FunctionCallOutput {
        #[serde(rename = "type")]
        kind: &'static str,
        call_id: &'a str,
        output: &'a str,
    },
}

#[derive(Serialize)]
struct Part<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
}

impl<'a> Item<'a> {
    /// The request item for `item`, which sends `text`.
    fn new(item: &'a TurnItem, text: &'a str) -> Result<Self, Refusal> {
        Ok(match item {
            TurnItem::Message { role, .. } => {
                let (role, kind) = match role {
                    Role::Developer => ("developer", "input_text"),
                    Role::User => ("user", "input_text"),
                    Role::Assistant => ("assistant", "output_text"),
                };
                Self::Message { role, content: [Part { kind, text }] }
            }
            TurnItem::Call(call) => Self::FunctionCall {
                kind: "function_call",
                call_id: call.call_id().as_str(),
                name: call.name().map_err(|error| refused(format!("call {}: {error}", call.call_id().as_str())))?,
                arguments: text,
            },
            TurnItem::CallOutput { call_id, .. } => {
                Self::FunctionCallOutput { kind: "function_call_output", call_id: call_id.as_str(), output: text }
            }
        })
    }
}

const fn effort(reasoning: ReasoningEffort) -> &'static str {
    match reasoning {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
    }
}

fn refused(reason: String) -> Refusal {
    Refusal::Refused { reason: Detail::new(reason) }
}

/// The function name the request sends for `program`.
fn function(program: &ProgramName) -> Result<String, Refusal> {
    function_name(program).map_err(|error| refused(format!("program {}: {error}", program.as_str())))
}

/// The offered definition as sent: `text` must parse as a JSON object whose
/// `name` is `tool`'s function name, and is spliced in as written.
fn definition(tool: &OfferedTool, text: &str) -> Result<Value, Refusal> {
    let program = tool.program().as_str();
    let definition: Value = serde_json::from_str(text)
        .ok()
        .filter(Value::is_object)
        .ok_or_else(|| refused(format!("the definition offered for {program} is not a JSON object")))?;
    let expected = function(tool.program())?;
    if definition.get("name").and_then(Value::as_str) != Some(expected.as_str()) {
        return Err(refused(format!("the definition offered for {program} is not named {expected}")));
    }
    Ok(definition)
}

/// Build the turn's request. `texts[i]` is the text `input.items()[i]` sends
/// (see [`crate::render`]) and `definitions[j]` the read definition of
/// `input.tools()[j]`.
///
/// # Errors
///
/// `Refusal::Refused` when an offered definition is not a JSON object named
/// for its program, or a program's function name is too long: the input was
/// built wrong, and nothing is fetched.
pub fn fetch(input: &TurnInput, texts: &[String], definitions: &[String]) -> Result<Fetch, Refusal> {
    let body = Body {
        model: input.model().as_str(),
        store: false,
        max_output_tokens: input.max_output_tokens().get(),
        reasoning: Reasoning { effort: effort(input.reasoning()) },
        tools: input
            .tools()
            .iter()
            .zip(definitions)
            .map(|(tool, text)| definition(tool, text))
            .collect::<Result<_, _>>()?,
        input: input.items().iter().zip(texts).map(|(item, text)| Item::new(item, text)).collect::<Result<_, _>>()?,
    };

    Ok(Fetch {
        request_id: 1,
        url: input.endpoint().as_str().into(),
        method: HttpMethod::Post,
        headers: vec![HttpHeader { name: "Content-Type".into(), value: "application/json".into() }],
        body: serde_json::to_vec(&body).expect("a body of strings, integers, and parsed JSON always serializes"),
        timeout_ms: Some(TURN_TIMEOUT_MILLIS),
    })
}

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{ErasedRef, ProgramName, Ref, Refusal};
    use aether_data::KindId;
    use aether_http::{HttpHeader, HttpMethod};
    use serde_json::json;

    use super::{TURN_TIMEOUT_MILLIS, fetch};
    use crate::input::tests::offered_tool;
    use crate::input::{
        CallId, Endpoint, FunctionName, ModelName, OfferedTools, OutputBudget, ReasoningEffort, Role, ToolCall,
        ToolOutput, TurnInput, TurnItem, TurnItems,
    };

    fn input(tools: OfferedTools, items: Vec<TurnItem>) -> TurnInput {
        TurnInput::new(
            Endpoint::new("https://example.test/v1/responses").expect("endpoint"),
            ModelName::new("muse-spark-1.3").expect("model"),
            tools,
            TurnItems::new(items).expect("items"),
            OutputBudget::new(512).expect("budget"),
            ReasoningEffort::Medium,
        )
    }

    fn program(name: &str) -> ProgramName {
        ProgramName::new(name).expect("program name")
    }

    fn offered(names: &[&str]) -> OfferedTools {
        OfferedTools::new(names.iter().map(|name| offered_tool(program(name))).collect()).expect("tools")
    }

    #[test]
    fn request_resends_every_item_in_order_with_store_off() {
        // Catches dropped or reordered items, the wrong part type on assistant items, `store` left on, a
        // conversation handle, an extra header, and the wrong method, URL, or timeout.
        let texts = ["Be brief.", "What is a bloom?", "A flowering.", "And a bloomery?"].map(String::from);
        let roles = [Role::Developer, Role::User, Role::Assistant, Role::User];
        let items = roles.iter().zip(&texts).map(|(&role, text)| TurnItem::message(role, Ref::of_text(text))).collect();
        let input = input(OfferedTools::default(), items);

        let request = fetch(&input, &texts, &[]).expect("a plain turn builds");

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

    #[test]
    fn request_offers_each_tool_and_replays_calls_beside_their_outputs() {
        // Catches unsent or reordered tools, a replayed call under the program name instead of its function name, a
        // refused call's name normalized or re-derived instead of sent as the model wrote it, replay items in the
        // wrong shape, and `store` left on.
        let definitions = [
            json!({ "type": "function", "name": "workspace-read", "parameters": { "type": "object" } }),
            json!({ "type": "function", "name": "muse-turn", "description": "One turn.", "strict": false }),
        ];
        let definition_texts = definitions.iter().map(ToString::to_string).collect::<Vec<_>>();
        let texts =
            ["Read the notes.", r#"{"path":"notes.md"}"#, "a bloom", "{}", "no such tool: Muse Turn"].map(String::from);
        let id = |id: &str| CallId::new(id).expect("call id");
        let decoded = ToolCall::decoded(
            id("call_1"),
            program("workspace.read"),
            Ref::of_text(&texts[1]),
            ErasedRef::new(KindId(1), Ref::of_text("input").digest()),
        );
        let unoffered = ToolCall::refused(
            id("call_2"),
            FunctionName::new("Muse Turn").expect("name"),
            Ref::of_text(&texts[3]),
            Ref::of_text(&texts[4]),
        );
        let output = |call: &str, text: &str| TurnItem::CallOutput {
            call_id: id(call),
            output: ToolOutput::Refused(Ref::of_text(text)),
        };
        let items = vec![
            TurnItem::message(Role::User, Ref::of_text(&texts[0])),
            TurnItem::Call(decoded),
            output("call_1", &texts[2]),
            TurnItem::Call(unoffered),
            output("call_2", &texts[4]),
        ];
        let input = input(offered(&["workspace.read", "muse.turn"]), items);

        let request = fetch(&input, &texts, &definition_texts).expect("a well-offered turn builds");

        let body: serde_json::Value = serde_json::from_slice(&request.body).expect("body is JSON");
        assert_eq!(
            body,
            json!({
                "model": "muse-spark-1.3",
                "store": false,
                "max_output_tokens": 512,
                "reasoning": { "effort": "medium" },
                "tools": definitions,
                "input": [
                    { "role": "user", "content": [{ "type": "input_text", "text": "Read the notes." }] },
                    {
                        "type": "function_call",
                        "call_id": "call_1",
                        "name": "workspace-read",
                        "arguments": r#"{"path":"notes.md"}"#,
                    },
                    { "type": "function_call_output", "call_id": "call_1", "output": "a bloom" },
                    { "type": "function_call", "call_id": "call_2", "name": "Muse Turn", "arguments": "{}" },
                    { "type": "function_call_output", "call_id": "call_2", "output": "no such tool: Muse Turn" },
                ],
            })
        );
    }

    #[test]
    fn a_definition_not_named_for_its_program_refuses_before_any_fetch() {
        // Catches an input that offers one program under another's definition, or a definition that is not a
        // function object at all.
        let texts = ["hi".to_owned()];
        let items = vec![TurnItem::message(Role::User, Ref::of_text(&texts[0]))];
        let cases = [
            ("another program's name", json!({ "type": "function", "name": "muse-turn" }).to_string()),
            ("the dotted program name", json!({ "type": "function", "name": "workspace.read" }).to_string()),
            ("no name", json!({ "type": "function" }).to_string()),
            ("not an object", r#"["workspace-read"]"#.to_owned()),
            ("not JSON", "workspace-read".to_owned()),
        ];
        for (label, text) in cases {
            let definitions = [text];
            let input = input(offered(&["workspace.read"]), items.clone());
            assert!(matches!(fetch(&input, &texts, &definitions), Err(Refusal::Refused { .. })), "{label} refuses");
        }
    }
}
