//! The agent session read off a transcript's tail.
//!
//! A dispatch transcript is NDJSON in one of two dialects: the Anthropic
//! Messages stream the Claude/Codex/Grok arms tee (`assistant` / `user` /
//! `result` events, `xtask/src/transform/messages.rs`) and the `muse exec
//! --json` envelope (`payload.kind` `run_started` / `run_terminal`,
//! `xtask/src/transform/muse/mod.rs`). Either way the run's meters — turns,
//! cost, duration — ride the terminal record, so the viewer reads them off
//! the tail instead of walking the whole transcript.

use serde_json::Value;

use super::super::metrics::{format_duration, format_micro_usd};

/// How many tail lines the summary scan reads. The terminal record is the
/// run's last line; the budget covers a short error tail after it.
pub const TAIL_SCAN: usize = 32;

/// Turns, cost, and time off the transcript's terminal record.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionSummary {
    pub turns: Option<u64>,
    pub cost_micro_usd: Option<u64>,
    pub duration_millis: Option<u64>,
    pub is_error: Option<bool>,
    pub session_id: Option<String>,
}

impl SessionSummary {
    /// `ok  3 turns  $0.42  12s` — absent meters are skipped, never zeroed,
    /// so an unpriced run cannot read as a free one.
    #[must_use]
    pub fn label(&self) -> String {
        let mut parts = vec![self.status_word().to_owned()];
        if let Some(turns) = self.turns {
            parts.push(turns_label(turns));
        }
        if let Some(cost) = self.cost_micro_usd {
            parts.push(format_micro_usd(cost));
        }
        if let Some(millis) = self.duration_millis {
            parts.push(format_duration(millis));
        }
        parts.join("  ")
    }

    fn status_word(&self) -> &'static str {
        if self.is_error == Some(true) {
            "error"
        } else {
            "ok"
        }
    }
}

/// `1 turn` / `3 turns` for a summary label and a collapsed result row.
#[must_use]
pub fn turns_label(turns: u64) -> String {
    if turns == 1 {
        "1 turn".to_owned()
    } else {
        format!("{turns} turns")
    }
}

/// The first terminal record from the tail — the run's last one wins,
/// mirroring the arms' own last-`result` derivation.
#[must_use]
pub fn summarize_tail(lines: &[&str]) -> Option<SessionSummary> {
    lines.iter().rev().find_map(|line| parse_terminal(line))
}

/// The terminal record on one raw line, in either transcript dialect.
#[must_use]
pub fn parse_terminal(raw: &str) -> Option<SessionSummary> {
    let value: Value = serde_json::from_str(raw).ok()?;
    if value.get("type").and_then(Value::as_str) == Some("result") {
        return Some(SessionSummary {
            turns: turns_of(&value),
            cost_micro_usd: cost_micros_of(&value),
            duration_millis: duration_millis_of(&value),
            is_error: value.get("is_error").and_then(Value::as_bool),
            session_id: value.get("session_id").and_then(Value::as_str).map(str::to_owned),
        });
    }
    let payload = value.get("payload")?;
    if payload.get("kind").and_then(Value::as_str) != Some("run_terminal") {
        return None;
    }
    Some(SessionSummary {
        is_error: Some(payload.get("terminal").and_then(Value::as_str) != Some("completed")),
        ..SessionSummary::default()
    })
}

/// `num_turns` on a terminal `result` event.
#[must_use]
pub fn turns_of(value: &Value) -> Option<u64> {
    value.get("num_turns").and_then(Value::as_u64)
}

/// `total_cost_usd` (dollars) as micro-USD for the console's money paint.
/// The JSON number is read decimally, so a `$0.42` cost lands on exactly
/// 420000 micros; absent or unparseable is `None`, never zero.
#[must_use]
pub fn cost_micros_of(value: &Value) -> Option<u64> {
    let number = value.get("total_cost_usd")?.as_number()?;
    if let Some(dollars) = number.as_u64() {
        return dollars.checked_mul(1_000_000);
    }
    if number.as_i64().is_some() {
        return None;
    }
    decimal_micros(&number.to_string())
}

/// `duration_ms` on a terminal `result` event, in the integer spelling the
/// arms emit.
#[must_use]
pub fn duration_millis_of(value: &Value) -> Option<u64> {
    value.get("duration_ms").and_then(Value::as_u64)
}

/// Whole dollars plus six fractional digits, all integer math: `as` casts
/// between float and int are pedantic findings, and a float multiply would
/// put `$0.42` a micro off. An exponent spelling or a seventh fractional
/// digit that is not forgettable fails rather than guessing.
fn decimal_micros(text: &str) -> Option<u64> {
    let (dollars_text, frac_text) = match text.split_once('.') {
        Some((dollars, frac)) => (dollars, frac),
        None => (text, ""),
    };
    if dollars_text.is_empty() || !dollars_text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if frac_text.len() > 6 || !frac_text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let dollars: u64 = dollars_text.parse().ok()?;
    let mut frac = frac_text.to_owned();
    while frac.len() < 6 {
        frac.push('0');
    }
    let frac: u64 = frac.parse().ok()?;
    dollars.checked_mul(1_000_000)?.checked_add(frac)
}

#[cfg(test)]
mod tests {
    use super::{parse_terminal, summarize_tail};

    fn result(line: &str) -> Option<super::SessionSummary> {
        parse_terminal(line)
    }

    #[test]
    fn the_terminal_record_yields_turns_cost_time_and_status() {
        // The plausible bug: the viewer paints the transcript but never the
        // session meters, so turns / cost / duration stay derivable only by
        // opening the raw JSON.
        let summary = result(
            r#"{"type":"result","is_error":false,"num_turns":3,"total_cost_usd":0.42,"duration_ms":12000,"session_id":"s-1"}"#,
        )
        .expect("terminal result parses");
        assert_eq!(summary.turns, Some(3));
        assert_eq!(summary.cost_micro_usd, Some(420_000));
        assert_eq!(summary.duration_millis, Some(12_000));
        assert_eq!(summary.is_error, Some(false));
        assert_eq!(summary.session_id.as_deref(), Some("s-1"));
        let label = summary.label();
        assert!(label.contains("3 turns"), "{label}");
        assert!(label.contains("$0.42"), "{label}");
        assert!(label.contains("12s"), "{label}");
        assert!(label.contains("ok"), "{label}");
    }

    #[test]
    fn the_last_terminal_record_wins() {
        // The plausible bug: a retried tail leaves two `result` events and
        // the viewer reports the first run's meters.
        let summary = summarize_tail(&[
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"one"}]}}"#,
            r#"{"type":"result","is_error":false,"num_turns":1,"total_cost_usd":0.10}"#,
            r#"{"type":"result","is_error":true,"num_turns":4,"total_cost_usd":2.0,"duration_ms":1000}"#,
        ])
        .expect("a terminal record is present");
        assert_eq!(summary.turns, Some(4));
        assert_eq!(summary.cost_micro_usd, Some(2_000_000));
        assert_eq!(summary.is_error, Some(true));
        assert!(summary.label().contains("error"), "{}", summary.label());
    }

    #[test]
    fn a_muse_terminal_yields_status_without_invented_meters() {
        // The plausible bug: the Muse dialect's lack of usage reads as a
        // free instant run instead of as unmeasured.
        let completed = result(
            r#"{"payload_type":"run.terminal.completed","payload":{"kind":"run_terminal","terminal":"completed","text":"VERDICT: pass","reason":null}}"#,
        )
        .expect("muse terminal parses");
        assert_eq!(completed.is_error, Some(false));
        assert_eq!(completed.turns, None);
        assert_eq!(completed.cost_micro_usd, None);
        assert_eq!(completed.label(), "ok");

        let failed = result(
            r#"{"payload_type":"run.terminal.failed","payload":{"kind":"run_terminal","terminal":"failed","text":"","reason":"server_error"}}"#,
        )
        .expect("muse failure parses");
        assert_eq!(failed.is_error, Some(true));
        assert_eq!(failed.label(), "error");
    }

    #[test]
    fn non_terminal_lines_are_not_a_session() {
        assert!(result("not json").is_none());
        assert!(result(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#).is_none());
        assert!(result(r#"{"payload_type":"run.lifecycle.started","payload":{"kind":"run_started"}}"#).is_none());
        assert!(summarize_tail(&[]).is_none());
        assert!(summarize_tail(&["not json"]).is_none());
    }

    #[test]
    fn an_absent_or_unparseable_cost_is_skipped_never_zeroed() {
        // The plausible bug: a missing `total_cost_usd` paints `$0`, so an
        // unpriced run reads as a free one.
        let missing = result(r#"{"type":"result","num_turns":1}"#).expect("parses");
        assert_eq!(missing.cost_micro_usd, None);
        assert!(!missing.label().contains('$'), "{}", missing.label());

        let negative = result(r#"{"type":"result","total_cost_usd":-1}"#).expect("parses");
        assert_eq!(negative.cost_micro_usd, None);

        let whole = result(r#"{"type":"result","total_cost_usd":2}"#).expect("parses");
        assert_eq!(whole.cost_micro_usd, Some(2_000_000));
        assert!(whole.label().contains("$2"), "{}", whole.label());
    }

    #[test]
    fn one_turn_reads_singular() {
        let summary = result(r#"{"type":"result","num_turns":1}"#).expect("parses");
        assert!(summary.label().contains("1 turn"), "{}", summary.label());
        assert!(!summary.label().contains("1 turns"), "{}", summary.label());
    }
}
