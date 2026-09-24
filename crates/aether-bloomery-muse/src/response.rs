//! Reading the vendor's reply into a recorded [`TurnResult`].
//!
//! A reply of any kind is a result, never a fault: tokens may have been
//! spent, and a fault carries no blobs. Only no reply at all refuses, and the
//! driver records that refusal as a fault. [`classify`] holds every rule.

use aether_bloomery_kinds::{Detail, Refusal};
use aether_bloomery_program::{Async, Env};
use aether_http::FetchResult;
use serde::Deserialize;

use crate::result::{HttpStatus, TurnOutcome, TurnResult, TurnUsage};

/// Stage the reply body and whatever text it carries, and build the result that cites them.
pub fn record(env: &mut Env<Async>, reply: FetchResult) -> Result<TurnResult, Refusal> {
    let (status, body) = match reply {
        FetchResult::Ok { status, body, .. } => (status, body),
        FetchResult::Err { error, .. } => return Err(Refusal::Refused { reason: Detail::new(format!("{error:?}")) }),
    };
    let code = HttpStatus::new(status).map_err(|_| Refusal::Refused {
        reason: Detail::new(format!("reply status {status} is not an HTTP status")),
    })?;

    let staged_body = env.stage_bytes(&body);
    let outcome = match classify(status, &body) {
        Classified::Completed { text, usage } => TurnOutcome::Completed { text: env.stage_text(&text), usage },
        Classified::Incomplete { text, reason, usage } => {
            TurnOutcome::Incomplete { text: env.stage_text(&text), reason: Detail::new(reason), usage }
        }
        Classified::Declined { refusal, usage } => TurnOutcome::Declined { refusal: env.stage_text(&refusal), usage },
        Classified::Rejected => TurnOutcome::Rejected,
        Classified::Unreadable => TurnOutcome::Unreadable,
    };
    Ok(TurnResult::new(code, staged_body, outcome))
}

/// A reply read into plain values, before any artifact is staged.
#[derive(Debug, PartialEq, Eq)]
enum Classified {
    Completed { text: String, usage: TurnUsage },
    Incomplete { text: String, reason: String, usage: TurnUsage },
    Declined { refusal: String, usage: TurnUsage },
    Rejected,
    Unreadable,
}

/// Every classification rule, in order.
///
/// 1. A non-2xx status is `Rejected`.
/// 2. A body that does not parse as a response is `Unreadable`.
/// 3. A vendor status of `failed` or `cancelled` is `Rejected`.
/// 4. A response without usage is `Unreadable`.
/// 5. Any `refusal` content part is `Declined`.
/// 6. A vendor status of `incomplete` is `Incomplete`; `completed` is `Completed`; any other is `Unreadable`.
///
/// The text is every `output_text` part of every `message` output item,
/// concatenated in order. Reasoning items never contribute.
fn classify(status: u16, body: &[u8]) -> Classified {
    if !(200..300).contains(&status) {
        return Classified::Rejected;
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
    match reply.status.as_str() {
        "completed" => Classified::Completed { text, usage },
        "incomplete" => {
            let reason = reply.incomplete_details.and_then(|details| details.reason).unwrap_or_default();
            Classified::Incomplete { text, reason, usage }
        }
        _ => Classified::Unreadable,
    }
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
    #[serde(other)]
    Other,
}

impl OutputItem {
    fn parts(&self) -> &[ContentPart] {
        match self {
            Self::Message { content } => content,
            Self::Other => &[],
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
    use super::{Classified, classify};
    use crate::result::TurnUsage;

    const COMPLETED: &str = include_str!("../fixtures/completed.json");
    const INCOMPLETE: &str = include_str!("../fixtures/incomplete.json");
    const REFUSAL: &str = include_str!("../fixtures/refusal.json");
    const RATE_LIMITED: &str = include_str!("../fixtures/rate_limited.json");

    #[test]
    fn completed_reply_joins_message_text_and_reads_every_usage_count() {
        // Catches reasoning text leaking into the answer, only the first part kept, and cached or
        // reasoning tokens read from the wrong path.
        assert_eq!(
            classify(200, COMPLETED.as_bytes()),
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
            classify(200, INCOMPLETE.as_bytes()),
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
            classify(200, REFUSAL.as_bytes()),
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
            ("429 error body", 429, RATE_LIMITED, Classified::Rejected),
            ("5xx over a completed body", 500, COMPLETED, Classified::Rejected),
            ("2xx failed", 200, failed, Classified::Rejected),
            ("2xx cancelled", 200, cancelled, Classified::Rejected),
            ("2xx error body", 200, RATE_LIMITED, Classified::Unreadable),
            ("2xx garbage", 200, "<html>bad gateway</html>", Classified::Unreadable),
        ];
        for (label, status, body, expected) in cases {
            assert_eq!(classify(status, body.as_bytes()), expected, "{label}");
        }
    }
}
