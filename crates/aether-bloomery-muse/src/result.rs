//! `muse.turn.result`: what the vendor answered, recorded once.
//!
//! Any reply is a result, because tokens may have been spent: the raw body is
//! always staged, so a classification bug can be corrected later from the
//! record. A fetch that got no reply for a reason a resend may clear is a
//! result too, one that reads as transient. Only [`crate::response`] builds one.

use core::borrow::Borrow;

use aether_bloomery_kinds::Detail;
use aether_data::{OpaqueBytes, Ref, Utf8Text};

use crate::input::{Reasoning, ToolCalls};

/// Why [`HttpStatus::new`] or decode refused a status code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpStatusError {
    /// Outside `100..=599`.
    OutOfRange,
}

impl HttpStatusError {
    const fn reason(self) -> &'static str {
        match self {
            Self::OutOfRange => "out-of-range",
        }
    }
}

/// An HTTP status code in `100..=599`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
pub struct HttpStatus(u16);

impl HttpStatus {
    /// Accept a status code in `100..=599`.
    ///
    /// # Errors
    ///
    /// [`HttpStatusError::OutOfRange`] for any other code.
    pub fn new(code: u16) -> Result<Self, HttpStatusError> {
        Self::check(code)?;
        Ok(Self(code))
    }

    /// The status code.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }

    // `#[storage(validate)]` calls `check(&inner)`; `Borrow` takes that
    // reference and `new`'s owned value alike.
    fn check(code: impl Borrow<u16>) -> Result<(), HttpStatusError> {
        if (100..=599).contains(code.borrow()) {
            Ok(())
        } else {
            Err(HttpStatusError::OutOfRange)
        }
    }
}

/// Token counts exactly as the vendor reported them.
///
/// A verbatim record: it claims no relation between the counts. A count the
/// reply leaves out of its details is recorded as zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Storage)]
#[allow(clippy::struct_field_names)] // aether-suppression-request: the fields mirror the vendor's usage counts by name, which is what makes the record verbatim
pub struct TurnUsage {
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
}

impl TurnUsage {
    pub(crate) const fn new(
        input_tokens: u64,
        cached_input_tokens: u64,
        output_tokens: u64,
        reasoning_tokens: u64,
    ) -> Self {
        Self { input_tokens, cached_input_tokens, output_tokens, reasoning_tokens }
    }

    /// Input tokens the turn was billed for.
    #[must_use]
    pub const fn input_tokens(&self) -> u64 {
        self.input_tokens
    }

    /// Input tokens the vendor served from its prompt cache.
    #[must_use]
    pub const fn cached_input_tokens(&self) -> u64 {
        self.cached_input_tokens
    }

    /// Output tokens the turn produced.
    #[must_use]
    pub const fn output_tokens(&self) -> u64 {
        self.output_tokens
    }

    /// Output tokens spent on reasoning.
    #[must_use]
    pub const fn reasoning_tokens(&self) -> u64 {
        self.reasoning_tokens
    }
}

/// How the vendor answered.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
pub enum TurnOutcome {
    /// The model finished its answer with a reply without calls, which the
    /// loop nudges back for another turn.
    ///
    /// `reasoning` is the reply's reasoning items in reply order, resent ahead of the reply.
    Completed { reasoning: Vec<Reasoning>, text: Ref<Utf8Text>, usage: TurnUsage },
    /// The model finished by asking for one or more calls, each to a program the turn offered. Any message text
    /// the reply also carried is kept.
    ///
    /// `reasoning` is the reply's reasoning items in reply order, resent ahead of the reply.
    Called { reasoning: Vec<Reasoning>, calls: ToolCalls, text: Ref<Utf8Text>, usage: TurnUsage },
    /// The model stopped early, for example on the output budget; the partial text is kept.
    Incomplete { text: Ref<Utf8Text>, reason: Detail, usage: TurnUsage },
    /// The model refused; the refusal text is kept instead of an answer.
    Declined { refusal: Ref<Utf8Text>, usage: TurnUsage },
    /// A non-transient non-2xx status, or a vendor status of `failed` or `cancelled`. The vendor's error is in the body.
    Rejected,
    /// The vendor refused for now (rate limit or overload), or the fetch got no reply for a reason a resend may
    /// clear (a timeout or a connection failure); a new request may succeed.
    ///
    /// `retry_after_secs` is the vendor's `Retry-After` delay in seconds, when it sent one as a number.
    Transient { retry_after_secs: Option<u32> },
    /// A 2xx body that does not read as a finished response, or asks for a call this turn cannot record. The
    /// body is kept.
    Unreadable,
}

/// One recorded turn: the vendor's reply, or the reason the fetch got none.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.turn.result")]
pub struct TurnResult {
    reply: Reply,
}

/// What a turn's fetch got back. Private, so only [`crate::response`] builds a result.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
enum Reply {
    /// The vendor answered; the status, raw body, and outcome read from it.
    Received { status: HttpStatus, body: Ref<OpaqueBytes>, outcome: TurnOutcome },
    /// The fetch got no reply for a reason a resend may clear (a timeout or a connection failure).
    Unreached { error: Detail },
}

/// The outcome of a turn that got no reply: transient, with no delay from the vendor.
const UNREACHED: TurnOutcome = TurnOutcome::Transient { retry_after_secs: None };

impl TurnResult {
    pub(crate) const fn received(status: HttpStatus, body: Ref<OpaqueBytes>, outcome: TurnOutcome) -> Self {
        Self { reply: Reply::Received { status, body, outcome } }
    }

    pub(crate) const fn unreached(error: Detail) -> Self {
        Self { reply: Reply::Unreached { error } }
    }

    /// The reply's HTTP status, when the vendor answered.
    #[must_use]
    pub const fn status(&self) -> Option<HttpStatus> {
        match &self.reply {
            Reply::Received { status, .. } => Some(*status),
            Reply::Unreached { .. } => None,
        }
    }

    /// The raw reply body, always kept when the vendor answered.
    #[must_use]
    pub const fn body(&self) -> Option<Ref<OpaqueBytes>> {
        match &self.reply {
            Reply::Received { body, .. } => Some(*body),
            Reply::Unreached { .. } => None,
        }
    }

    /// Why the fetch got no reply, when it got none.
    #[must_use]
    pub const fn error(&self) -> Option<&Detail> {
        match &self.reply {
            Reply::Received { .. } => None,
            Reply::Unreached { error } => Some(error),
        }
    }

    /// How the vendor answered. A turn that got no reply reads as `Transient` with no `retry_after_secs`.
    #[must_use]
    pub const fn outcome(&self) -> &TurnOutcome {
        match &self.reply {
            Reply::Received { outcome, .. } => outcome,
            Reply::Unreached { .. } => &UNREACHED,
        }
    }
}

invariant_errors!(HttpStatusError);
