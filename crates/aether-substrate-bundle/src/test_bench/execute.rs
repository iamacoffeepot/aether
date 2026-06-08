//! `TestBench::execute` — a declarative op sequence over the
//! settlement-gated `TestBench` primitives (issue 868).
//!
//! Every `test_bench`-driven test is a small state machine: load a
//! component, advance, send a mail, advance, capture, send cleanup.
//! Each step calls a separate [`TestBench`] method, and each method
//! is independently responsible for waiting on its causal chain to
//! settle (ADR-0080 §6). That per-method settlement glue has been a
//! recurring flake source (issues 834 / 836 / 838 / 860): when a
//! method forgot to wait for the chain it kicked off, parallel CI
//! surfaced the race, and the fix was a one-off patch to the
//! offending method.
//!
//! [`TestBench::execute`] centralizes the sequencing: it takes a
//! labelled list of [`BenchOp`]s, dispatches each through the
//! matching settlement-gated primitive, blocks on settlement, then
//! proceeds. When the next timing race surfaces it gets fixed once,
//! inside the op→primitive mapping, rather than N times across the
//! per-method trapdoors. This is the typed-Rust successor to the
//! retired `aether-scenario` YAML `Script` + `Vec<Step>`.

use std::collections::HashMap;
use std::error;
use std::fmt;

use aether_data::{Kind, KindId};
use aether_kinds::MailEnvelope;

use super::bench::{TestBench, TestBenchError};

/// One atomic step in a [`TestBench::execute`] sequence. Each variant
/// resolves via an existing settlement-gated primitive on
/// [`TestBench`]; the sequencer waits for the op's causal chain to
/// drain before proceeding to the next step.
///
/// Build ops with the typed constructors ([`BenchOp::send_mail`],
/// [`BenchOp::send_and_await`], [`BenchOp::advance`],
/// [`BenchOp::capture`]) — they encode the payload from a typed kind
/// via [`Kind::encode_into_bytes`], so callers never hand-encode.
///
/// Recipients are mailbox *names* (`"aether.fs"`, `"aether.component"`,
/// a loaded component's trampoline address) — mailbox ids are
/// one-way name hashes, so every send resolves by name.
pub enum BenchOp {
    /// Run `ticks` complete frames. Build with [`BenchOp::advance`].
    Advance { ticks: u32 },
    /// Fire-and-settle a mail; no reply is awaited. Build with the
    /// typed [`BenchOp::send_mail`].
    SendMail {
        recipient: String,
        kind: KindId,
        payload: Vec<u8>,
    },
    /// Send a mail and block until a reply arrives, stashing the raw
    /// reply bytes. Build with the typed [`BenchOp::send_and_await`].
    /// Covers component load / replace / drop and the `aether.fs`
    /// read / write / delete / list round trips uniformly — decode
    /// the stored bytes downstream with [`ExecutionResult::reply`].
    SendAndAwait {
        recipient: String,
        kind: KindId,
        payload: Vec<u8>,
    },
    /// Capture the current frame as PNG bytes. Build with
    /// [`BenchOp::capture`]. Does not dispatch a tick — sequence a
    /// [`BenchOp::Advance`] before it if the world must move first.
    Capture,
    /// Capture with pre/after mail bundles dispatched atomically
    /// around the readback (the `CaptureFrame` shape, ADR-0020): `pre`
    /// lands *before* the readback so its effects appear in the PNG,
    /// `after` runs *after* (cleanup). Build with
    /// [`BenchOp::capture_with_mails`]. Use this rather than
    /// decomposing into separate `SendMail` + `Capture` ops when the
    /// pre-mail's geometry must land in the same frame as the
    /// readback.
    CaptureWithMails {
        pre: Vec<MailEnvelope>,
        after: Vec<MailEnvelope>,
    },
}

impl BenchOp {
    /// Run `ticks` complete frames.
    #[must_use]
    pub fn advance(ticks: u32) -> Self {
        Self::Advance { ticks }
    }

    /// Capture the current frame.
    #[must_use]
    pub fn capture() -> Self {
        Self::Capture
    }

    /// Capture with pre/after mail bundles dispatched atomically
    /// around the readback. See [`BenchOp::CaptureWithMails`].
    #[must_use]
    pub fn capture_with_mails(pre: Vec<MailEnvelope>, after: Vec<MailEnvelope>) -> Self {
        Self::CaptureWithMails { pre, after }
    }

    /// Fire-and-settle a typed mail (no reply awaited). Encodes `mail`
    /// via [`Kind::encode_into_bytes`] — works for both cast and
    /// postcard kinds.
    #[must_use]
    pub fn send_mail<K: Kind>(recipient: impl Into<String>, mail: &K) -> Self {
        Self::SendMail {
            recipient: recipient.into(),
            kind: K::ID,
            payload: mail.encode_into_bytes(),
        }
    }

    /// Send a typed mail and block until a reply arrives. Decode the
    /// reply downstream with [`ExecutionResult::reply`]. Encodes
    /// `mail` via [`Kind::encode_into_bytes`].
    #[must_use]
    pub fn send_and_await<K: Kind>(recipient: impl Into<String>, mail: &K) -> Self {
        Self::SendAndAwait {
            recipient: recipient.into(),
            kind: K::ID,
            payload: mail.encode_into_bytes(),
        }
    }
}

/// One output per executed op, keyed by the op's label in
/// [`ExecutionResult`]. `Replied` and `Captured` carry bytes; the
/// other two are unit markers confirming the op ran.
pub enum BenchOutput {
    Advanced,
    Mailed,
    Replied(Vec<u8>),
    Captured(Vec<u8>),
}

/// Map of per-op outputs from a successful [`TestBench::execute`]
/// call, keyed by each op's label. Fetch results by label so tests
/// read by intent (`result.captured("snap")`) and survive step
/// reordering, rather than destructuring a positional array.
#[derive(Default)]
pub struct ExecutionResult {
    inner: HashMap<String, BenchOutput>,
}

impl ExecutionResult {
    /// Whether a step with `label` ran.
    #[must_use]
    pub fn contains(&self, label: &str) -> bool {
        self.inner.contains_key(label)
    }

    /// Raw output for `label`, if the step ran.
    #[must_use]
    pub fn get(&self, label: &str) -> Option<&BenchOutput> {
        self.inner.get(label)
    }

    /// PNG bytes from a [`BenchOp::Capture`] step. `None` if `label`
    /// didn't run or wasn't a `Capture`.
    #[must_use]
    pub fn captured(&self, label: &str) -> Option<&[u8]> {
        match self.inner.get(label)? {
            BenchOutput::Captured(bytes) => Some(bytes),
            _ => None,
        }
    }

    /// Decode the reply from a [`BenchOp::SendAndAwait`] step as `R`.
    /// `R` is any reply kind (`LoadResult`, `ReplaceResult`,
    /// `WriteResult`, …); the bytes decode through the kind's declared
    /// codec (cast or postcard) via `Kind::decode_from_bytes`
    /// (ADR-0100). Errors with [`ExecutionError::NoSuchReply`] if
    /// `label` didn't run a `SendAndAwait` (or didn't run at all), or
    /// [`ExecutionError::ReplyDecode`] if the bytes don't decode as
    /// `R`.
    pub fn reply<R>(&self, label: &str) -> Result<R, ExecutionError>
    where
        R: Kind,
    {
        match self.inner.get(label) {
            Some(BenchOutput::Replied(bytes)) => {
                R::decode_from_bytes(bytes).ok_or_else(|| ExecutionError::ReplyDecode {
                    label: label.to_owned(),
                    error: "Kind::decode_from_bytes returned None".to_owned(),
                })
            }
            _ => Err(ExecutionError::NoSuchReply(label.to_owned())),
        }
    }
}

/// Failure modes of [`TestBench::execute`] and its result accessors.
#[derive(Debug)]
pub enum ExecutionError {
    /// Two ops in the same `execute` call shared a label.
    DuplicateLabel(String),
    /// The op at `label` failed mid-sequence; `error` is the
    /// underlying [`TestBenchError`] (settlement timeout, decode
    /// failure, unknown mailbox, …). Aborts the sequence.
    OpFailed {
        label: String,
        error: TestBenchError,
    },
    /// [`ExecutionResult::reply`] was asked for a label that didn't
    /// run a [`BenchOp::SendAndAwait`] (or didn't run at all).
    NoSuchReply(String),
    /// [`ExecutionResult::reply`] couldn't decode the stashed reply
    /// bytes as the requested type.
    ReplyDecode { label: String, error: String },
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateLabel(label) => {
                write!(f, "duplicate step label {label:?} in execute() sequence")
            }
            Self::OpFailed { label, error } => {
                write!(f, "execute() step {label:?} failed: {error}")
            }
            Self::NoSuchReply(label) => {
                write!(f, "no SendAndAwait reply stored under label {label:?}")
            }
            Self::ReplyDecode { label, error } => {
                write!(f, "decode reply for label {label:?}: {error}")
            }
        }
    }
}

impl error::Error for ExecutionError {}

impl TestBench {
    /// Execute `steps` in order. Each op dispatches via the matching
    /// settlement-gated [`TestBench`] primitive, blocks until its
    /// causal chain drains (ADR-0080 §6), then proceeds. Outputs are
    /// keyed by each op's label; fetch them from the returned
    /// [`ExecutionResult`] (`captured(label)`, `reply::<R>(label)`).
    ///
    /// Labels must be unique within one call
    /// ([`ExecutionError::DuplicateLabel`]). Any op failure aborts the
    /// sequence and returns [`ExecutionError::OpFailed`] naming the
    /// failing step.
    ///
    /// `execute` composes over the per-op primitives — it does not
    /// replace them. Tests that assert intermediate state between ops
    /// stay imperative, or split into multiple `execute` calls.
    pub fn execute(
        &mut self,
        steps: Vec<(&str, BenchOp)>,
    ) -> Result<ExecutionResult, ExecutionError> {
        let mut out = ExecutionResult::default();
        for (label, op) in steps {
            if out.contains(label) {
                return Err(ExecutionError::DuplicateLabel(label.to_owned()));
            }
            let result = match op {
                BenchOp::Advance { ticks } => self.advance(ticks).map(|_| BenchOutput::Advanced),
                BenchOp::SendMail {
                    recipient,
                    kind,
                    payload,
                } => self
                    .send_bytes(&recipient, kind, payload)
                    .map(|()| BenchOutput::Mailed),
                BenchOp::SendAndAwait {
                    recipient,
                    kind,
                    payload,
                } => self
                    .send_bytes_and_await(&recipient, kind, payload)
                    .map(BenchOutput::Replied),
                BenchOp::Capture => self.capture().map(BenchOutput::Captured),
                BenchOp::CaptureWithMails { pre, after } => self
                    .capture_with_mails(pre, after)
                    .map(BenchOutput::Captured),
            };
            match result {
                Ok(output) => {
                    out.inner.insert(label.to_owned(), output);
                }
                Err(error) => {
                    return Err(ExecutionError::OpFailed {
                        label: label.to_owned(),
                        error,
                    });
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cast reply kind with a non-`f32` field — its wire image is the
    /// raw cast bytes, which a postcard reader would misdecode.
    #[repr(C)]
    #[derive(Copy, Clone, Debug, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
    struct CastReply {
        code: u32,
        flag: u16,
        _pad: u16,
    }

    impl Kind for CastReply {
        const NAME: &'static str = "test.execute_cast_reply";
        const ID: KindId = KindId(0xDEAD_BEEF_000A_0001);

        fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
            (bytes.len() == size_of::<Self>()).then(|| bytemuck::pod_read_unaligned(bytes))
        }

        fn encode_into_bytes(&self) -> Vec<u8> {
            bytemuck::bytes_of(self).to_vec()
        }
    }

    /// ADR-0100: the `SendAndAwait` reply accessor decodes the recorded
    /// bytes through `Kind::decode_from_bytes`, so a cast reply kind
    /// round-trips uncorrupted (its `u32` / `u16` fields survive). A
    /// postcard decode would have misread the raw cast image.
    #[test]
    fn execution_result_reply_decodes_cast_kind() {
        let reply = CastReply {
            code: 0x0A0B_0C0D,
            flag: 0x1234,
            _pad: 0,
        };
        // The substrate reply path encodes via `Kind::encode_into_bytes`
        // (ADR-0100), so the recorded bytes are the cast image.
        let bytes = reply.encode_into_bytes();
        let result = ExecutionResult {
            inner: HashMap::from([("reply".to_owned(), BenchOutput::Replied(bytes))]),
        };

        let decoded: CastReply = result.reply("reply").expect("cast reply decodes");
        assert_eq!(decoded, reply);
    }
}
