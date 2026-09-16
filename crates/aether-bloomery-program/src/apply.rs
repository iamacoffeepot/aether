//! The driver: one transaction that stages the declaration and exactly one event.

use std::any::Any;
use std::error::Error;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};

use aether_bloomery_journal::{AppendError, Batch, BatchError, GetError, Journal, JournalError, Seq, split_artifact};
use aether_bloomery_kinds::{Digest, ExecutorName, Fault, FaultReason, Ref, Transition};
use aether_data::{KindId, StorageError};

use crate::execute::Refusal;
use crate::kinds;
use crate::read::ReadError;
use crate::registry::Executors;

/// Outcome of a successful apply: one event landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// The attempt produced an execution.
    Transition(Seq),
    /// The attempt produced no execution.
    Fault(Seq),
}

/// Failure before any event is written.
#[derive(Debug)]
pub enum ApplyError {
    /// The program digest is not stored.
    NoProgram,
    /// Nothing claims it; nothing is written.
    NoExecutor,
    /// The input's prefix is not `program.input`; nothing is written.
    InputKind {
        /// Kind the declaration asked for.
        expected: KindId,
        /// Kind actually prefixed on the stored blob.
        actual: KindId,
    },
    /// Backend failure.
    Journal(JournalError),
    /// Append refused after the `HeadMoved` retry.
    Append(AppendError),
    /// Reading the declaration or input failed.
    Read(ReadError),
    /// Encoding the declaration or event failed.
    Storage(StorageError),
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoProgram => write!(f, "program digest is not stored"),
            Self::NoExecutor => write!(f, "no executor claims this program"),
            Self::InputKind { expected, actual } => {
                write!(f, "input kind {actual} is not declared input {expected}")
            }
            Self::Journal(error) => write!(f, "{error}"),
            Self::Append(error) => write!(f, "{error}"),
            Self::Read(error) => write!(f, "{error}"),
            Self::Storage(error) => write!(f, "{error}"),
        }
    }
}

impl Error for ApplyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Journal(error) => Some(error),
            Self::Append(error) => Some(error),
            Self::Read(error) => Some(error),
            Self::Storage(error) => Some(error),
            Self::NoProgram | Self::NoExecutor | Self::InputKind { .. } => None,
        }
    }
}

impl From<JournalError> for ApplyError {
    fn from(error: JournalError) -> Self {
        Self::Journal(error)
    }
}

impl From<AppendError> for ApplyError {
    fn from(error: AppendError) -> Self {
        Self::Append(error)
    }
}

impl From<ReadError> for ApplyError {
    fn from(error: ReadError) -> Self {
        Self::Read(error)
    }
}

impl From<GetError> for ApplyError {
    fn from(error: GetError) -> Self {
        Self::Read(ReadError::Get(error))
    }
}

impl From<StorageError> for ApplyError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

impl From<BatchError> for ApplyError {
    fn from(error: BatchError) -> Self {
        match error {
            BatchError::Storage(error) => Self::Storage(error),
        }
    }
}

/// Apply `program` to `input` on the first executor that claims it.
///
/// One transaction: the declaration (idempotent), every staged blob, and
/// exactly one event, [`Transition`] or [`Fault`].
///
/// # Errors
///
/// [`ApplyError`] when nothing is written.
pub fn apply(
    journal: &mut Journal,
    executors: &Executors,
    program: Ref<kinds::Program>,
    input: Digest,
    cause: Option<Seq>,
) -> Result<Applied, ApplyError> {
    let declaration = journal.get::<kinds::Program>(&program.digest())?.ok_or(ApplyError::NoProgram)?;
    match journal.get_bytes(&input)? {
        Some((actual, _)) if actual != declaration.input => {
            return Err(ApplyError::InputKind { expected: declaration.input, actual });
        }
        Some(_) | None => {}
    }
    let Some(executor_name) = executors.claims(&program.digest()).next().cloned() else {
        return Err(ApplyError::NoExecutor);
    };
    let program_digest = program.digest();
    let outcome = catch_unwind(AssertUnwindSafe(|| executors.execute(&program_digest, &input, journal)));
    match outcome {
        Ok(None) => Err(ApplyError::NoExecutor),
        Ok(Some(Ok((mut batch, result)))) => {
            // Execution<P> stages a result whose prefix is P::Result::ID, and
            // declaration.result is that same id. A runtime branch here would
            // imply the type did not hold; debug_assert is the check, not a
            // second path.
            debug_assert!(
                batch
                    .staged_blob(&result)
                    .is_some_and(|bytes| { split_artifact(bytes).is_ok_and(|(kind, _)| kind == declaration.result) }),
                "result prefix must equal the declaration result kind"
            );
            batch.stage_encoded(&declaration)?;
            batch.push_event(&Transition { program, input, result, executor: executor_name }, cause)?;
            Ok(Applied::Transition(append_retry(journal, &batch)?))
        }
        Ok(Some(Err(refusal))) => {
            let reason = match refusal {
                Refusal::Refused(text) => FaultReason::refused(text),
                Refusal::InputMissing => FaultReason::InputMissing,
                Refusal::InputDecode => FaultReason::InputDecode,
            };
            Ok(Applied::Fault(append_fault(journal, &declaration, program, input, executor_name, reason, cause)?))
        }
        Err(payload) => {
            let message = panic_message(payload.as_ref());
            Ok(Applied::Fault(append_fault(
                journal,
                &declaration,
                program,
                input,
                executor_name,
                FaultReason::panicked(message),
                cause,
            )?))
        }
    }
}

fn append_fault(
    journal: &mut Journal,
    declaration: &kinds::Program,
    program: Ref<kinds::Program>,
    input: Digest,
    executor: ExecutorName,
    reason: FaultReason,
    cause: Option<Seq>,
) -> Result<Seq, ApplyError> {
    let mut batch = Batch::new();
    batch.stage_encoded(declaration)?;
    batch.push_event(&Fault { program, input, executor, reason }, cause)?;
    append_retry(journal, &batch)
}

fn append_retry(journal: &mut Journal, batch: &Batch) -> Result<Seq, ApplyError> {
    let head = journal.head()?;
    match journal.append(head, batch) {
        Ok(range) => Ok(range.start),
        Err(AppendError::HeadMoved { actual }) => match journal.append(actual, batch) {
            Ok(range) => Ok(range.start),
            Err(error) => Err(ApplyError::Append(error)),
        },
        Err(error) => Err(ApplyError::Append(error)),
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panic".to_owned())
}
