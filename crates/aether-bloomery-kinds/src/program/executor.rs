//! Why an executor a program called ended its invocation (ADR-0237 decision 2).

use crate::program::fault::{Detail, FaultReason};

/// Why an executor a program called ended its invocation. The program never
/// observes it; the driver records it as the matching [`FaultReason`].
///
/// The set is closed to the faults an executor binding can report, so a
/// bundle cannot claim a reason the driver assigns itself.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Schema)]
pub enum ExecutorFault {
    /// The executor's time allotment ran out.
    TimedOut,
    /// The executor's memory allotment ran out.
    ResourceExhausted,
    /// The executor failed during the run for a reason outside the request.
    Failed { reason: Detail },
}

impl From<ExecutorFault> for FaultReason {
    fn from(fault: ExecutorFault) -> Self {
        match fault {
            ExecutorFault::TimedOut => Self::TimedOut,
            ExecutorFault::ResourceExhausted => Self::ResourceExhausted,
            ExecutorFault::Failed { reason } => Self::ExecutorFailed { reason },
        }
    }
}
