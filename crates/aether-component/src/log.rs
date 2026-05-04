//! ADR-0060 guest-side logging via mail sink.
//!
//! `MailSubscriber` implements `tracing::Subscriber` so existing
//! `tracing::warn!` / `error!` / `info!` calls in component code emit
//! mail to the substrate-owned `aether.log` mailbox. The chassis
//! sink decodes and re-emits through the host-side `tracing` subscriber
//! so events land in `engine_logs` (ADR-0023).
//!
//! The macro short-circuits at `enabled()` for events below the
//! configured max level (default `INFO`) — debug/trace calls in tight
//! loops cost a vtable dispatch and return, no FFI. v1 hardcodes the
//! level; per-component dynamic filtering rides on a future
//! `aether.control.set_log_level` mail (ADR-0060 §Default level and
//! per-component filter).
//!
//! Spans are no-ops in v1. The wire kind carries pre-formatted message
//! strings, so structured span context is not propagated; flat events
//! (`tracing::warn!(...)`) are the supported shape.
//!
//! Production builds that need truly-zero-cost trace/debug elimination
//! set `tracing/release_max_level_info` (or similar) in their own
//! `Cargo.toml` — the `STATIC_MAX_LEVEL` check happens at the macro
//! call site before the subscriber is consulted.

use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use alloc::string::{String, ToString};

use aether_kinds::LogEvent;
use tracing::{
    Event, Level, Metadata, Subscriber,
    field::{Field, Visit},
    span,
};

use crate::resolve_mailbox;

/// Hard cap on the mail payload's `message` field. Protects the queue
/// from a misbehaving component flooding multi-megabyte log frames; the
/// `" [truncated]"` suffix tells a `engine_logs` reader the original was
/// longer.
const MAX_MESSAGE_BYTES: usize = 4096;
const TRUNCATED_SUFFIX: &str = " [truncated]";

/// Default max level — events strictly above this (numerically lower
/// in `tracing::Level`) are dropped at `enabled()`. `Level::INFO`
/// emits info / warn / error and skips debug / trace, matching the
/// substrate's `log_capture` default (ADR-0060 §Default level).
const MAX_LEVEL: Level = Level::INFO;

pub struct MailSubscriber {
    next_span: AtomicU64,
}

impl MailSubscriber {
    pub const fn new() -> Self {
        MailSubscriber {
            next_span: AtomicU64::new(1),
        }
    }
}

impl Default for MailSubscriber {
    fn default() -> Self {
        Self::new()
    }
}

impl Subscriber for MailSubscriber {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        *metadata.level() <= MAX_LEVEL
    }

    fn new_span(&self, _attrs: &span::Attributes<'_>) -> span::Id {
        // Spans are observed but not transmitted in v1. Hand out a
        // monotonic id so `tracing` invariants hold.
        let id = self.next_span.fetch_add(1, Ordering::Relaxed);
        span::Id::from_u64(id.max(1))
    }

    fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
    fn enter(&self, _: &span::Id) {}
    fn exit(&self, _: &span::Id) {}

    fn event(&self, event: &Event<'_>) {
        let metadata = event.metadata();
        let level = level_to_u8(*metadata.level());
        let target = metadata.target().to_string();

        let mut visitor = MessageBuilder::new();
        event.record(&mut visitor);
        let message = visitor.finish();

        let payload = LogEvent {
            level,
            target,
            message,
        };
        resolve_mailbox::<LogEvent>("aether.log").send(&crate::WASM_TRANSPORT, &payload);
    }
}

fn level_to_u8(level: Level) -> u8 {
    match level {
        Level::TRACE => 0,
        Level::DEBUG => 1,
        Level::INFO => 2,
        Level::WARN => 3,
        Level::ERROR => 4,
    }
}

/// Walks an `Event`'s fields and renders them in fields-first order:
/// `key1=val1 key2=val2 message_body`. Matches `tracing-subscriber`'s
/// default fmt layer so a reader of `engine_logs` sees the same shape
/// whether the event was emitted host- or guest-side.
struct MessageBuilder {
    fields: String,
    message: String,
}

impl MessageBuilder {
    fn new() -> Self {
        Self {
            fields: String::new(),
            message: String::new(),
        }
    }

    fn finish(mut self) -> String {
        if !self.fields.is_empty() && !self.message.is_empty() {
            self.fields.push(' ');
        }
        self.fields.push_str(&self.message);
        truncate(self.fields)
    }

    fn append_field(&mut self, name: &str, separator: &str, value: core::fmt::Arguments<'_>) {
        if !self.fields.is_empty() {
            self.fields.push(' ');
        }
        let _ = write!(&mut self.fields, "{}{}{}", name, separator, value);
    }
}

impl Visit for MessageBuilder {
    fn record_debug(&mut self, field: &Field, value: &dyn core::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(&mut self.message, "{:?}", value);
        } else {
            self.append_field(field.name(), "=", format_args!("{:?}", value));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            self.append_field(field.name(), "=", format_args!("{}", value));
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.append_field(field.name(), "=", format_args!("{}", value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.append_field(field.name(), "=", format_args!("{}", value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.append_field(field.name(), "=", format_args!("{}", value));
    }
}

fn truncate(mut s: String) -> String {
    if s.len() <= MAX_MESSAGE_BYTES {
        return s;
    }
    let mut cap = MAX_MESSAGE_BYTES.saturating_sub(TRUNCATED_SUFFIX.len());
    while cap > 0 && !s.is_char_boundary(cap) {
        cap -= 1;
    }
    s.truncate(cap);
    s.push_str(TRUNCATED_SUFFIX);
    s
}

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Installs `MailSubscriber` as `tracing`'s global default. Idempotent;
/// repeated calls (e.g. across `replace_component` cycles in the same
/// linear memory) are a no-op.
///
/// Called by the `export!` macro before the user's `Component::init`
/// runs so logging from inside `init` works.
pub fn install_global_default() {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = tracing::dispatcher::set_global_default(tracing::dispatcher::Dispatch::new(
        MailSubscriber::new(),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_mapping() {
        assert_eq!(level_to_u8(Level::TRACE), 0);
        assert_eq!(level_to_u8(Level::DEBUG), 1);
        assert_eq!(level_to_u8(Level::INFO), 2);
        assert_eq!(level_to_u8(Level::WARN), 3);
        assert_eq!(level_to_u8(Level::ERROR), 4);
    }

    #[test]
    fn truncate_preserves_short_messages() {
        let s = String::from("short message");
        let out = truncate(s);
        assert_eq!(out, "short message");
    }

    #[test]
    fn truncate_appends_suffix_when_oversize() {
        let s = "a".repeat(MAX_MESSAGE_BYTES + 100);
        let out = truncate(s);
        assert!(out.ends_with(TRUNCATED_SUFFIX));
        assert!(out.len() <= MAX_MESSAGE_BYTES);
    }

    #[test]
    fn truncate_respects_char_boundary() {
        // Force the naive cut inside a multi-byte char.
        let mut s = String::with_capacity(MAX_MESSAGE_BYTES + 4);
        for _ in 0..(MAX_MESSAGE_BYTES / 4 + 5) {
            s.push('🦀'); // 4 bytes per char
        }
        let out = truncate(s);
        // Round-tripping back through `String::from_utf8` would panic
        // on a torn boundary; the assertion is implicit in not panicking
        // and producing a valid suffix.
        assert!(out.ends_with(TRUNCATED_SUFFIX));
    }

    // The Visit/format path is exercised end-to-end by the substrate
    // integration test (sink decode roundtrip) — `tracing::field::Field`
    // can't be hand-constructed without a real `FieldSet` + callsite, so
    // unit-testing `MessageBuilder` in isolation buys little over what
    // the integration coverage already proves.
}
