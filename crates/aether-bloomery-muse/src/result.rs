//! `muse.turn.result`: what the vendor answered, recorded once.
//!
//! Any reply is a result, because tokens may have been spent: the raw body is
//! always staged, so a classification bug can be corrected later from the
//! record. Only [`crate::response`] builds one.

use core::borrow::Borrow;

use aether_bloomery_kinds::{Detail, OpaqueBytes, Ref, Utf8Text};

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
    /// The model finished its answer.
    Completed { text: Ref<Utf8Text>, usage: TurnUsage },
    /// The model stopped early, for example on the output budget; the partial text is kept.
    Incomplete { text: Ref<Utf8Text>, reason: Detail, usage: TurnUsage },
    /// The model refused; the refusal text is kept instead of an answer.
    Declined { refusal: Ref<Utf8Text>, usage: TurnUsage },
    /// A non-2xx status, or a vendor status of `failed` or `cancelled`. The vendor's error is in the body.
    Rejected,
    /// A 2xx body that does not read as a finished response. The body is kept.
    Unreadable,
}

/// One recorded turn: the status, the raw body, and the outcome read from it.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.turn.result")]
pub struct TurnResult {
    status: HttpStatus,
    body: Ref<OpaqueBytes>,
    outcome: TurnOutcome,
}

impl TurnResult {
    pub(crate) const fn new(status: HttpStatus, body: Ref<OpaqueBytes>, outcome: TurnOutcome) -> Self {
        Self { status, body, outcome }
    }

    /// The reply's HTTP status.
    #[must_use]
    pub const fn status(&self) -> HttpStatus {
        self.status
    }

    /// The raw reply body, always kept.
    #[must_use]
    pub const fn body(&self) -> Ref<OpaqueBytes> {
        self.body
    }

    /// How the vendor answered.
    #[must_use]
    pub const fn outcome(&self) -> &TurnOutcome {
        &self.outcome
    }
}

invariant_errors!(HttpStatusError);
