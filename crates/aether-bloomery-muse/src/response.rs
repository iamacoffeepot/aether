//! Reading the vendor's reply into a recorded [`TurnResult`].
//!
//! A reply of any kind is a result, never a fault: tokens may have been
//! spent, and a fault carries no blobs. A fetch that timed out or failed at the
//! connection is a result too, one with no reply that reads as transient, so the
//! caller may resend the turn. Only a failure with no reply that a resend cannot
//! clear refuses, and the driver records that refusal as a fault. [`classify`]
//! holds every rule for a reply.

use aether_bloomery_kinds::{Detail, Refusal};
use aether_bloomery_program::{Async, Env, ToolSchema, function_name};
use aether_http::{FetchResult, HttpError, HttpHeader};
use serde::Deserialize;

use crate::arguments;
use crate::input::{CallId, FunctionName, OfferedTool, ToolCall, ToolCalls};
use crate::result::{HttpStatus, TurnOutcome, TurnResult, TurnUsage};

/// Stage the reply body and whatever text and call arguments it carries, and build the result that cites them.
///
/// `offered` is the turn's offered tools, and `inputs[i]` is the read input schema of `offered[i]`. A call named for an
/// offered tool decodes its arguments against that tool's schema and cites the staged input, or the staged refusal of
/// the decode. A call named for no offered tool is refused with the staged text `no such tool: <name>`, so the loop
/// answers it and the model can correct itself on the next turn.
pub fn record(
    env: &mut Env<Async>,
    offered: &[OfferedTool],
    inputs: &[ToolSchema],
    reply: FetchResult,
) -> Result<TurnResult, Refusal> {
    let (status, headers, body) = match reply {
        FetchResult::Ok { status, headers, body, .. } => (status, headers, body),
        FetchResult::Err { error, .. } => return unreached(&error),
    };
    let code = HttpStatus::new(status).map_err(|_| Refusal::Refused {
        reason: Detail::new(format!("reply status {status} is not an HTTP status")),
    })?;

    let staged_body = env.stage_bytes(&body);
    let outcome = match classify(status, vendor_verdict(&headers), retry_after_secs(&headers), &body, offered) {
        Classified::Completed { text, usage } => TurnOutcome::Completed { text: env.stage_text(&text), usage },
        Classified::Called { calls, text, usage } => {
            let calls = calls
                .into_iter()
                .map(|ReadCall { call_id, name, tool, arguments }| {
                    let verbatim = env.stage_text(&arguments);
                    let Some(tool) = tool else {
                        let refusal = env.stage_text(&format!("no such tool: {}", name.as_str()));
                        return ToolCall::refused(call_id, name, verbatim, refusal);
                    };
                    let schema = &inputs[tool];
                    match arguments::decode(&arguments, schema) {
                        Ok(payload) => {
                            let input = env.stage_payload(schema.kind_id(), &payload);
                            ToolCall::decoded(call_id, offered[tool].program().clone(), verbatim, input)
                        }
                        Err(reason) => ToolCall::refused(call_id, name, verbatim, env.stage_text(&reason)),
                    }
                })
                .collect();
            let calls = ToolCalls::new(calls).expect("classify checked the call list's rules");
            TurnOutcome::Called { calls, text: env.stage_text(&text), usage }
        }
        Classified::Incomplete { text, reason, usage } => {
            TurnOutcome::Incomplete { text: env.stage_text(&text), reason: Detail::new(reason), usage }
        }
        Classified::Declined { refusal, usage } => TurnOutcome::Declined { refusal: env.stage_text(&refusal), usage },
        Classified::Rejected => TurnOutcome::Rejected,
        Classified::Transient { retry_after_secs } => TurnOutcome::Transient { retry_after_secs },
        Classified::Unreadable => TurnOutcome::Unreadable,
    };
    Ok(TurnResult::received(code, staged_body, outcome))
}

/// Sort a fetch that got no reply.
///
/// A timeout, and every transport failure the adapter folds into `AdapterError` (a refused connection, a reset, DNS,
/// TLS, a body that broke mid-read), is an unreached result: a resend may clear it. An allowlist denial, disabled
/// egress, an invalid URL (which covers a secret refused over cleartext), a body too large, and a closed capability
/// refuse: each is a configuration, policy, or shutdown state no resend clears.
fn unreached(error: &HttpError) -> Result<TurnResult, Refusal> {
    let detail = Detail::new(format!("{error:?}"));
    match error {
        HttpError::Timeout | HttpError::AdapterError(_) => Ok(TurnResult::unreached(detail)),
        HttpError::AllowlistDenied
        | HttpError::Disabled
        | HttpError::InvalidUrl(_)
        | HttpError::BodyTooLarge
        | HttpError::Closed => Err(Refusal::Refused { reason: detail }),
    }
}

/// The vendor's own verdict on a refusal: `x-should-retry` of exactly `true` or `false`.
///
/// The name matches in any case; any other value, or no header, is no verdict.
fn vendor_verdict(headers: &[HttpHeader]) -> Option<bool> {
    match header(headers, "x-should-retry")? {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// The first `Retry-After` header as delta-seconds.
///
/// Only ASCII digits that fit a `u32` count. An HTTP-date is `None`: a
/// program has no clock to turn a date into a delay.
fn retry_after_secs(headers: &[HttpHeader]) -> Option<u32> {
    let value = header(headers, "retry-after")?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

/// The value of the first header named `name`, matched in any case.
fn header<'a>(headers: &'a [HttpHeader], name: &str) -> Option<&'a str> {
    headers.iter().find(|header| header.name.eq_ignore_ascii_case(name)).map(|header| header.value.as_str())
}

/// A reply read into plain values, before any artifact is staged.
#[derive(Debug, PartialEq, Eq)]
enum Classified {
    Completed { text: String, usage: TurnUsage },
    Called { calls: Vec<ReadCall>, text: String, usage: TurnUsage },
    Incomplete { text: String, reason: String, usage: TurnUsage },
    Declined { refusal: String, usage: TurnUsage },
    Rejected,
    Transient { retry_after_secs: Option<u32> },
    Unreadable,
}

/// Every classification rule, in order.
///
/// 1. A non-2xx status with a vendor verdict is `Transient` for `true` and `Rejected` for `false`.
/// 2. A 429 whose error code is `insufficient_quota` is `Rejected`: a billing state no retry clears.
/// 3. Any other 429, and any 503 or 529, is `Transient`.
/// 4. Any other non-2xx status is `Rejected`, including 500, 502, and 504, which may follow billed work.
/// 5. A body that does not parse as a response is `Unreadable`.
/// 6. A vendor status of `failed` or `cancelled` is `Rejected`.
/// 7. A response without usage is `Unreadable`.
/// 8. Any `refusal` content part is `Declined`.
/// 9. A vendor status of `completed` with any `function_call` output item is `Called`, every call in order. A call
///    whose name is exactly the function name of a tool `offered` holds is read with that tool; any other call is
///    read with no tool, so [`record`] refuses it back to the model. It is `Unreadable` instead when a call id is not
///    a valid `CallId` or repeats, a name is not a valid `FunctionName`, or the calls outnumber
///    `ToolCalls::MAX_CALLS`.
/// 10. A vendor status of `incomplete` is `Incomplete`; `completed` is `Completed`; any other is `Unreadable`.
///
/// A `Transient` outcome carries `retry_after_secs` as read. The text is every
/// `output_text` part of every `message` output item, concatenated in order.
/// Reasoning items never contribute. A call's arguments are kept verbatim;
/// [`record`] decodes them after.
fn classify(
    status: u16,
    verdict: Option<bool>,
    retry_after_secs: Option<u32>,
    body: &[u8],
    offered: &[OfferedTool],
) -> Classified {
    if !(200..300).contains(&status) {
        let transient = verdict.unwrap_or_else(|| match status {
            429 => error_code(body).as_deref() != Some("insufficient_quota"),
            503 | 529 => true,
            _ => false,
        });
        return if transient {
            Classified::Transient { retry_after_secs }
        } else {
            Classified::Rejected
        };
    }
    let Ok(reply) = serde_json::from_slice::<Reply>(body) else {
        return Classified::Unreadable;
    };
    if matches!(reply.status.as_str(), "failed" | "cancelled") {
        return Classified::Rejected;
    }
    let Some(usage) = reply.usage.as_ref().map(Usage::record) else {
        return Classified::Unreadable;
    };

    let parts = || reply.output.iter().flat_map(OutputItem::parts);
    let refusals: Vec<&str> = parts().filter_map(ContentPart::refusal).collect();
    if !refusals.is_empty() {
        return Classified::Declined { refusal: refusals.concat(), usage };
    }
    let text: String = parts().filter_map(ContentPart::output_text).collect();
    let calls: Vec<_> = reply.output.into_iter().filter_map(OutputItem::into_call).collect();
    if reply.status == "completed" && !calls.is_empty() {
        return read_calls(calls, offered).map_or(Classified::Unreadable, |calls| Classified::Called {
            calls,
            text,
            usage,
        });
    }
    match reply.status.as_str() {
        "completed" => Classified::Completed { text, usage },
        "incomplete" => {
            let reason = reply.incomplete_details.and_then(|details| details.reason).unwrap_or_default();
            Classified::Incomplete { text, reason, usage }
        }
        _ => Classified::Unreadable,
    }
}

/// One call of a reply, read before any artifact is staged.
#[derive(Debug, PartialEq, Eq)]
struct ReadCall {
    call_id: CallId,
    /// The function name exactly as the model wrote it.
    name: FunctionName,
    /// The index of the offered tool whose function name is `name`, or `None` when no offered tool has it.
    tool: Option<usize>,
    /// The arguments, verbatim.
    arguments: String,
}

/// Each call read against the offered tools, or `None` when any breaks a rule of [`classify`]'s step 9.
///
/// A name matches a tool only when it is exactly that tool's function name, the name the request's definition sent,
/// so a decoded call replays under the name the model wrote.
fn read_calls(calls: Vec<FunctionCall>, offered: &[OfferedTool]) -> Option<Vec<ReadCall>> {
    let names: Vec<Option<String>> = offered.iter().map(|tool| function_name(tool.program()).ok()).collect();
    let calls = calls
        .into_iter()
        .map(|call| {
            let tool = names.iter().position(|name| name.as_deref() == Some(call.name.as_str()));
            Some(ReadCall {
                call_id: CallId::new(call.call_id).ok()?,
                name: FunctionName::new(call.name).ok()?,
                tool,
                arguments: call.arguments,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    ToolCalls::check_ids(calls.iter().map(|call| &call.call_id)).ok()?;
    Some(calls)
}

/// The `error.code` of a vendor error body, when it parses and carries one.
fn error_code(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<ErrorReply>(body).ok()?.error?.code
}

/// The part of a vendor error body the program reads; every other field is ignored.
#[derive(Deserialize)]
struct ErrorReply {
    error: Option<ErrorBody>,
}

#[derive(Deserialize)]
struct ErrorBody {
    code: Option<String>,
}

/// The parts of a vendor response the program reads; every other field is ignored.
#[derive(Deserialize)]
struct Reply {
    status: String,
    #[serde(default)]
    output: Vec<OutputItem>,
    usage: Option<Usage>,
    incomplete_details: Option<IncompleteDetails>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OutputItem {
    Message {
        #[serde(default)]
        content: Vec<ContentPart>,
    },
    FunctionCall(FunctionCall),
    #[serde(other)]
    Other,
}

/// A `function_call` output item: the model asks for one call.
#[derive(Deserialize)]
struct FunctionCall {
    call_id: String,
    name: String,
    arguments: String,
}

impl OutputItem {
    fn parts(&self) -> &[ContentPart] {
        match self {
            Self::Message { content } => content,
            Self::FunctionCall(_) | Self::Other => &[],
        }
    }

    fn into_call(self) -> Option<FunctionCall> {
        match self {
            Self::FunctionCall(call) => Some(call),
            Self::Message { .. } | Self::Other => None,
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentPart {
    OutputText {
        text: String,
    },
    Refusal {
        refusal: String,
    },
    #[serde(other)]
    Other,
}

impl ContentPart {
    fn output_text(&self) -> Option<&str> {
        match self {
            Self::OutputText { text } => Some(text),
            Self::Refusal { .. } | Self::Other => None,
        }
    }

    fn refusal(&self) -> Option<&str> {
        match self {
            Self::Refusal { refusal } => Some(refusal),
            Self::OutputText { .. } | Self::Other => None,
        }
    }
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: u64,
    output_tokens: u64,
    input_tokens_details: Option<InputTokensDetails>,
    output_tokens_details: Option<OutputTokensDetails>,
}

#[derive(Deserialize)]
struct InputTokensDetails {
    cached_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct OutputTokensDetails {
    reasoning_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct IncompleteDetails {
    reason: Option<String>,
}

impl Usage {
    fn record(&self) -> TurnUsage {
        TurnUsage::new(
            self.input_tokens,
            self.input_tokens_details.as_ref().and_then(|details| details.cached_tokens).unwrap_or(0),
            self.output_tokens,
            self.output_tokens_details.as_ref().and_then(|details| details.reasoning_tokens).unwrap_or(0),
        )
    }
}

#[cfg(test)]
mod tests {
    use aether_http::HttpHeader;

    use aether_bloomery_kinds::ProgramName;

    use super::{Classified, ReadCall, classify, retry_after_secs, vendor_verdict};
    use crate::input::tests::offered_tool;
    use crate::input::{CallId, FunctionName, OfferedTool, ToolCalls};
    use crate::result::TurnUsage;

    const COMPLETED: &str = include_str!("../fixtures/completed.json");
    const INCOMPLETE: &str = include_str!("../fixtures/incomplete.json");
    const REFUSAL: &str = include_str!("../fixtures/refusal.json");
    const RATE_LIMITED: &str = include_str!("../fixtures/rate_limited.json");
    const OVERLOADED: &str = include_str!("../fixtures/overloaded.json");
    const INSUFFICIENT_QUOTA: &str = include_str!("../fixtures/insufficient_quota.json");
    const CALLED: &str = include_str!("../fixtures/called.json");

    fn program(name: &str) -> ProgramName {
        ProgramName::new(name).expect("program name")
    }

    fn offer(names: &[&str]) -> Vec<OfferedTool> {
        names.iter().map(|name| offered_tool(program(name))).collect()
    }

    fn one_header(name: &str, value: &str) -> Vec<HttpHeader> {
        vec![HttpHeader { name: name.into(), value: value.into() }]
    }

    #[test]
    fn completed_reply_joins_message_text_and_reads_every_usage_count() {
        // Catches reasoning text leaking into the answer, only the first part kept, and cached or
        // reasoning tokens read from the wrong path.
        assert_eq!(
            classify(200, None, None, COMPLETED.as_bytes(), &[]),
            Classified::Completed {
                text: "A bloomery is a furnace that smelts iron into a bloom.".into(),
                usage: TurnUsage::new(1200, 1024, 340, 300),
            }
        );
    }

    #[test]
    fn incomplete_reply_keeps_partial_text_and_its_reason() {
        // Catches a truncated turn recorded as `Completed`, and a partial text that is dropped.
        assert_eq!(
            classify(200, None, None, INCOMPLETE.as_bytes(), &[]),
            Classified::Incomplete {
                text: "A bloomery is a furnace that".into(),
                reason: "max_output_tokens".into(),
                usage: TurnUsage::new(1200, 0, 64, 58),
            }
        );
    }

    #[test]
    fn refusal_part_declines() {
        // Catches a model refusal recorded as the answer; the reply also leaves out both usage details.
        assert_eq!(
            classify(200, None, None, REFUSAL.as_bytes(), &[]),
            Classified::Declined {
                refusal: "I can't help with that request.".into(),
                usage: TurnUsage::new(900, 0, 12, 0),
            }
        );
    }

    #[test]
    fn every_non_answer_reply_is_rejected_or_unreadable() {
        // Catches a vendor error parsed as success.
        let failed = r#"{"status": "failed", "error": {"code": "server_error", "message": "boom"}, "output": [], "usage": null}"#;
        let cancelled = r#"{"status": "cancelled", "output": [], "usage": null}"#;
        let cases = [
            ("401 error body", 401, RATE_LIMITED, Classified::Rejected),
            ("5xx over a completed body", 500, COMPLETED, Classified::Rejected),
            ("2xx failed", 200, failed, Classified::Rejected),
            ("2xx cancelled", 200, cancelled, Classified::Rejected),
            ("2xx error body", 200, RATE_LIMITED, Classified::Unreadable),
            ("2xx garbage", 200, "<html>bad gateway</html>", Classified::Unreadable),
        ];
        for (label, status, body, expected) in cases {
            assert_eq!(classify(status, None, None, body.as_bytes(), &[]), expected, "{label}");
        }
    }

    #[test]
    fn transient_refusals_are_told_apart_from_permanent_ones() {
        // Catches a permanent refusal marked retryable, a transient refusal recorded as terminal, a
        // gateway error that may follow billed work marked "nothing bought", the `Retry-After` delay
        // dropped, and the vendor's own verdict overruled by the status table.
        let transient = |retry_after_secs| Classified::Transient { retry_after_secs };
        let cases = [
            ("429 rate limit", 429, None, Some(20), RATE_LIMITED, transient(Some(20))),
            ("429 unparseable body", 429, None, None, "<html>slow down</html>", transient(None)),
            ("503 overloaded", 503, None, None, OVERLOADED, transient(None)),
            ("529 overloaded", 529, None, Some(3), OVERLOADED, transient(Some(3))),
            ("429 insufficient quota", 429, None, Some(20), INSUFFICIENT_QUOTA, Classified::Rejected),
            ("500 over an overload body", 500, None, None, OVERLOADED, Classified::Rejected),
            ("502 over an overload body", 502, None, None, OVERLOADED, Classified::Rejected),
            ("504 over an overload body", 504, None, None, OVERLOADED, Classified::Rejected),
            ("2xx rate-limit body", 200, None, Some(20), RATE_LIMITED, Classified::Unreadable),
            ("verdict true over 500", 500, Some(true), Some(5), OVERLOADED, transient(Some(5))),
            ("verdict true over insufficient quota", 429, Some(true), None, INSUFFICIENT_QUOTA, transient(None)),
            ("verdict false over 503", 503, Some(false), Some(5), OVERLOADED, Classified::Rejected),
            ("verdict false over 429 rate limit", 429, Some(false), None, RATE_LIMITED, Classified::Rejected),
            ("verdict over a 2xx is ignored", 200, Some(true), None, RATE_LIMITED, Classified::Unreadable),
        ];
        for (label, status, verdict, retry_after, body, expected) in cases {
            assert_eq!(classify(status, verdict, retry_after, body.as_bytes(), &[]), expected, "{label}");
        }
    }

    #[test]
    fn vendor_verdict_reads_true_or_false_only() {
        // Catches a malformed verdict read as a decision, and a header name matched case-sensitively.
        let cases = [
            ("x-should-retry", "true", Some(true)),
            ("X-Should-Retry", "false", Some(false)),
            ("X-SHOULD-RETRY", "true", Some(true)),
            ("x-should-retry", "yes", None),
            ("x-should-retry", "1", None),
            ("x-should-retry", "TRUE", None),
            ("retry-after", "true", None),
        ];
        for (name, value, expected) in cases {
            assert_eq!(vendor_verdict(&one_header(name, value)), expected, "{name}: {value}");
        }
        assert_eq!(vendor_verdict(&[]), None, "absent");
    }

    #[test]
    fn retry_after_reads_delta_seconds_only() {
        // Catches an HTTP-date or a garbage value mistaken for a delay, and a header name matched
        // case-sensitively.
        let cases = [
            ("Retry-After", "20", Some(20)),
            ("retry-after", "7", Some(7)),
            ("Retry-After", "0", Some(0)),
            ("Retry-After", "4294967295", Some(u32::MAX)),
            ("Retry-After", "Wed, 21 Oct 2026 07:28:00 GMT", None),
            ("Retry-After", "-1", None),
            ("Retry-After", "+1", None),
            ("Retry-After", "1.5", None),
            ("Retry-After", "4294967296", None),
            ("Retry-After", "", None),
            ("x-should-retry", "20", None),
        ];
        for (name, value, expected) in cases {
            assert_eq!(retry_after_secs(&one_header(name, value)), expected, "{name}: {value}");
        }
        assert_eq!(retry_after_secs(&[]), None, "absent");
        let two = [
            HttpHeader { name: "Retry-After".into(), value: "4".into() },
            HttpHeader { name: "retry-after".into(), value: "9".into() },
        ];
        assert_eq!(retry_after_secs(&two), Some(4), "the first header wins");
    }

    fn read(id: &str, name: &str, tool: Option<usize>, arguments: &str) -> ReadCall {
        ReadCall {
            call_id: CallId::new(id).expect("call id"),
            name: FunctionName::new(name).expect("function name"),
            tool,
            arguments: arguments.to_owned(),
        }
    }

    const READ_ARGUMENTS: &str = r#"{"path": "notes/bloomery.md"}"#;
    const TURN_ARGUMENTS: &str = r#"{"endpoint":"https://example.test/v1/responses"}"#;

    #[test]
    fn a_reply_that_asks_for_calls_records_every_call_in_order() {
        // Catches a reply read as `Completed` with its calls dropped, only the first call kept, a call matched to
        // the wrong tool, and arguments rewritten instead of kept verbatim.
        assert_eq!(
            classify(200, None, None, CALLED.as_bytes(), &offer(&["muse.turn", "workspace.read"])),
            Classified::Called {
                calls: vec![
                    read("call_read", "workspace-read", Some(1), READ_ARGUMENTS),
                    read("call_turn", "muse-turn", Some(0), TURN_ARGUMENTS),
                ],
                text: "Reading the notes first.".into(),
                usage: TurnUsage::new(1500, 0, 90, 40),
            }
        );
    }

    #[test]
    fn a_call_to_a_tool_the_turn_did_not_offer_is_read_with_no_tool() {
        // Catches a call to an unoffered name dropped from the list, the whole reply made `Unreadable` over it, and
        // a dotted or respelled name admitted through a lossy map to an offered program.
        let both = offer(&["muse.turn", "workspace.read"]);
        let spaced = CALLED.replace("muse-turn", "Muse Turn");
        let dotted = CALLED.replace("muse-turn", "muse.turn");
        let cases = [
            ("a program not offered", CALLED, offer(&["workspace.read"]), "muse-turn", Some(0)),
            ("no program offered", CALLED, Vec::new(), "muse-turn", None),
            ("a name no program has", spaced.as_str(), both.clone(), "Muse Turn", Some(1)),
            ("a dotted program name", dotted.as_str(), both, "muse.turn", Some(1)),
        ];
        for (label, body, offered, name, read_tool) in cases {
            assert_eq!(
                classify(200, None, None, body.as_bytes(), &offered),
                Classified::Called {
                    calls: vec![
                        read("call_read", "workspace-read", read_tool, READ_ARGUMENTS),
                        read("call_turn", name, None, TURN_ARGUMENTS),
                    ],
                    text: "Reading the notes first.".into(),
                    usage: TurnUsage::new(1500, 0, 90, 40),
                },
                "{label}"
            );
        }
    }

    #[test]
    fn a_call_the_turn_cannot_record_leaves_the_reply_unreadable() {
        // Catches a `ToolCalls` built past its own rule on a repeated or invalid call id or too many calls, and a
        // name past the cap stored as a call's name.
        let both = offer(&["muse.turn", "workspace.read"]);
        let repeated = CALLED.replace("call_turn", "call_read");
        let spaced = CALLED.replace("call_turn", "call turn");
        let too_long = CALLED.replace("muse-turn", &"a".repeat(FunctionName::MAX_BYTES + 1));
        let mut reply: serde_json::Value = serde_json::from_str(CALLED).expect("fixture parses");
        let call = reply["output"][2].clone();
        reply["output"] = (0..=ToolCalls::MAX_CALLS)
            .map(|index| {
                let mut call = call.clone();
                call["call_id"] = format!("call_{index}").into();
                call
            })
            .collect();
        let too_many = reply.to_string();
        let cases = [
            ("a repeated call id", repeated.as_str()),
            ("an invalid call id", spaced.as_str()),
            ("a name past the cap", too_long.as_str()),
            ("more calls than the cap", too_many.as_str()),
        ];
        for (label, body) in cases {
            assert_eq!(classify(200, None, None, body.as_bytes(), &both), Classified::Unreadable, "{label}");
        }
    }
}
