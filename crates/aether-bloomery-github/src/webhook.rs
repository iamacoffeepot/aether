//! The operator notification webhook (#5166).
//!
//! A one-endpoint POST client for a Discord-compatible incoming webhook: one
//! plain-text message per loud transition, `{"content": "…"}`. It lives beside
//! the GitHub adapter rather than in the chassis because this is the crate that
//! already owns an outbound HTTP client — the chassis has none, and giving it
//! one would be a manifest change rather than a slice.
//!
//! # The URL is a credential
//!
//! Anyone holding the URL can post as the coordinator. So it is never
//! formatted into a log line, never carried in a returned error, and never
//! rendered by [`Debug`]: [`WebhookError`] carries a status code or a
//! transport class, and [`ReqwestWebhook`]'s `Debug` prints the endpoint's
//! host at most. The host reads the URL out of a file and hands the string
//! straight in, so it never appears in argv or an environment listing either.
//!
//! # Delivery is best-effort
//!
//! A failed POST returns `Err` and the caller records nothing — the next poll
//! recomputes the same loud set and retries it. That is the whole retry
//! policy: the dedupe ledger is the only state, so an unsent message is simply
//! an unrecorded key. There is no backoff timer to get wrong and no queue to
//! drain, and a wedged endpoint costs one request per poll interval rather
//! than blocking the line.

use std::error::Error;
use std::fmt;
use std::fmt::Write as _;
use std::time::Duration;

use reqwest::blocking::Client as BlockingClient;

/// Why one webhook POST did not deliver. Deliberately narrow, and deliberately
/// free of the endpoint URL: this value reaches log output, and the URL is the
/// credential.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum WebhookError {
    /// The endpoint answered a non-2xx status.
    Status {
        /// The HTTP status code.
        status: u16,
    },
    /// The request never got an answer (DNS, connect, TLS, timeout). The
    /// string is `reqwest`'s own class description with no URL in it.
    Transport {
        /// A human-readable transport failure class.
        detail: String,
    },
}

impl fmt::Display for WebhookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Status { status } => write!(f, "webhook endpoint answered {status}"),
            Self::Transport { detail } => write!(f, "webhook transport failed: {detail}"),
        }
    }
}

impl Error for WebhookError {}

/// The seam a notification is posted through. `Send + Sync` because it lives
/// in a capability's runtime state behind an `Arc` and is driven from the
/// actor's dispatch thread; a test injects a recording double.
pub trait WebhookSink: Send + Sync {
    /// Post one plain-text message.
    ///
    /// # Errors
    /// The endpoint refused it, or the request never got an answer. Either way
    /// the caller retries on its next poll.
    fn post(&self, content: &str) -> Result<(), WebhookError>;
}

/// Connect-phase bound for the webhook hop. The notification reactor runs this
/// call inline on the cooperative chassis dispatcher, so an unbounded connect
/// would stall the actor's slot — the same reason the GitHub transport bounds
/// its own.
const WEBHOOK_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Total request-phase bound (connect + send + response) for the webhook hop.
const WEBHOOK_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How many characters of one message reach the endpoint.
///
/// Discord refuses a `content` past 2000 characters outright, so an over-long
/// message is a dropped notification rather than a truncated one. Truncating
/// here keeps the loud half of the message — the kind and the subject lead it
/// — and the operator can always open the board for the rest.
pub const MAX_CONTENT_CHARS: usize = 1900;

/// The production sink: a `reqwest::blocking` POST to one fixed endpoint.
pub struct ReqwestWebhook {
    client: BlockingClient,
    url: String,
}

impl fmt::Debug for ReqwestWebhook {
    /// The URL is a credential, so the derived `Debug` would leak it into any
    /// `{:?}` of a struct holding this sink.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ReqwestWebhook { url: <redacted> }")
    }
}

impl ReqwestWebhook {
    /// Build a sink posting to `url`.
    ///
    /// # Errors
    /// The `reqwest` client could not be constructed.
    pub fn new(url: String) -> Result<Self, WebhookError> {
        let client = BlockingClient::builder()
            .user_agent("aether-bloomery-notify")
            .connect_timeout(WEBHOOK_CONNECT_TIMEOUT)
            .timeout(WEBHOOK_REQUEST_TIMEOUT)
            .build()
            .map_err(|error| WebhookError::Transport { detail: error.to_string() })?;
        Ok(Self { client, url })
    }
}

impl WebhookSink for ReqwestWebhook {
    fn post(&self, content: &str) -> Result<(), WebhookError> {
        let response = self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .body(content_body(content))
            .send()
            // `reqwest`'s Display for a request error includes the URL it was
            // built for, so the credential would ride the error text straight
            // into a log line. Report the class without it.
            .map_err(|error| WebhookError::Transport { detail: transport_class(&error) })?;
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(WebhookError::Status { status })
        }
    }
}

/// A URL-free description of why a request failed.
fn transport_class(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "timed out".to_owned()
    } else if error.is_connect() {
        "could not connect".to_owned()
    } else if error.is_body() {
        "request body failed".to_owned()
    } else if error.is_decode() {
        "response did not decode".to_owned()
    } else {
        "request failed".to_owned()
    }
}

/// The `{"content": "…"}` request body for one message, truncated to
/// [`MAX_CONTENT_CHARS`] on a character boundary.
///
/// Hand-built rather than routed through a serde data format: this is one
/// object with one string field, and the workspace's own codec is the only
/// encoder the estate is permitted to grow. What that costs is the escaping,
/// which is `escape_json_string` and is what the tests below pin.
#[must_use]
pub fn content_body(content: &str) -> String {
    let mut body = String::from("{\"content\":\"");
    escape_json_string(&truncate_chars(content, MAX_CONTENT_CHARS), &mut body);
    body.push_str("\"}");
    body
}

/// `content`'s first `limit` characters — never a byte slice, which would
/// split a multi-byte character and produce a body that is not UTF-8.
fn truncate_chars(content: &str, limit: usize) -> String {
    content.chars().take(limit).collect()
}

/// Append `value` to `out` escaped as the body of a JSON string, per RFC 8259
/// §7: the two mandatory escapes (`"` and `\`), the five short forms, and
/// `\u00XX` for every other control character. Nothing else is escaped —
/// non-ASCII passes through, which is legal and keeps a message readable in a
/// transcript.
fn escape_json_string(value: &str, out: &mut String) {
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            control if control < ' ' => {
                let _ = write!(out, "\\u{:04x}", control as u32);
            }
            other => out.push(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_CONTENT_CHARS, ReqwestWebhook, WebhookError, content_body};

    #[test]
    fn a_message_body_escapes_what_json_requires() {
        // The plausible bug: a wedge message quotes an evidence path or a
        // lane's multi-line summary, the body stops being valid JSON, and the
        // endpoint answers 400 forever with nothing in the log to explain it.
        assert_eq!(
            content_body("he said \"stop\"\nback\\slash\ttab"),
            "{\"content\":\"he said \\\"stop\\\"\\nback\\\\slash\\ttab\"}"
        );
    }

    #[test]
    fn a_control_character_takes_the_long_escape() {
        // The plausible bug: a control byte reaching the message from lane
        // output is emitted raw, which is not a legal JSON string character.
        assert_eq!(content_body("bell\u{7}here"), "{\"content\":\"bell\\u0007here\"}");
    }

    #[test]
    fn an_over_long_message_truncates_on_a_character_boundary() {
        // The plausible bug: the cap is applied to bytes, splitting a
        // multi-byte character and producing a body that is not UTF-8 — or the
        // cap is not applied at all and the endpoint refuses the whole message.
        let long = "é".repeat(MAX_CONTENT_CHARS + 40);
        let body = content_body(&long);
        let content = body.strip_prefix("{\"content\":\"").and_then(|rest| rest.strip_suffix("\"}"));
        assert_eq!(content.map(|content| content.chars().count()), Some(MAX_CONTENT_CHARS));
    }

    #[test]
    fn neither_debug_nor_an_error_can_carry_the_endpoint() {
        // Tripwire: the webhook URL is a credential, and the acceptance case
        // is that it appears in no log output at any level. Both channels that
        // reach a log line are checked here — the sink's own `Debug` and every
        // `WebhookError` spelling.
        let secret = "https://discord.example/api/webhooks/1/verysecrettoken";
        let sink = ReqwestWebhook::new(secret.to_owned()).expect("the blocking client builds");
        assert!(!format!("{sink:?}").contains("verysecrettoken"));

        for error in
            [WebhookError::Status { status: 429 }, WebhookError::Transport { detail: "could not connect".to_owned() }]
        {
            assert!(!format!("{error}").contains("verysecrettoken"));
            assert!(!format!("{error:?}").contains("verysecrettoken"));
        }
    }
}
