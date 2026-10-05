//! Mail that relays one program API call from an invocation through its
//! bundle root to the driver that sent the `Invoke` (ADR-0240 D6).

use alloc::vec::Vec;

use aether_actor::HeldReply;
use aether_data::KindId;

use crate::Detail;
use crate::program::refusal::Refusal;

/// The closed set of APIs a program's `run` may bind. The driver maps each
/// to a provider it holds, answers it itself, or refuses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, aether_data::Schema)]
pub enum ProgramApi {
    /// `aether.http`.
    Http,
    /// `aether.process`.
    Process,
    /// The unit's workspace.
    Workspace,
    /// The driver's own randomness, answered from the operating system with no
    /// capability behind it.
    Entropy,
}

/// One captured API call, sent by an invocation to its root and relayed by
/// the root to the `Invoke`'s sender.
///
/// `call` is minted by the invocation and echoed by every hop: the root's
/// relay back to its inline child carries no correlation.
#[aether_data::kind(name = "aether.bloomery.program.api_call", eq, no_serde)]
pub struct ApiCall {
    /// The invocation's id for this call.
    pub call: u64,
    /// The API the program bound.
    pub api: ProgramApi,
    /// Kind of the encoded request.
    pub kind: KindId,
    /// The request, encoded once when the program captured it.
    pub payload: Vec<u8>,
}

/// The answer to one [`ApiCall`], relayed back along the same hops.
#[aether_data::kind(name = "aether.bloomery.program.api_call_result", eq, no_serde)]
pub enum ApiCallResult {
    /// The provider replied with `payload`, encoded as `kind`.
    Replied { call: u64, kind: KindId, payload: Vec<u8> },
    /// A hop refused the call; no provider saw it.
    Refused { call: u64, refusal: Refusal },
}

/// One draw of randomness: how many bytes the program asked the driver to
/// fill from the operating system.
#[aether_data::kind(name = "aether.bloomery.program.entropy_draw", eq, no_serde)]
pub struct EntropyDraw {
    /// How many random bytes the program asked for.
    pub count: u8,
}

/// The driver's answer to one [`EntropyDraw`].
#[aether_data::kind(name = "aether.bloomery.program.entropy_result", eq, no_serde)]
pub enum EntropyResult {
    /// The drawn bytes, exactly as many as the draw asked for.
    Drawn { bytes: Vec<u8> },
    /// The operating system yielded no randomness.
    Unavailable,
}

impl HeldReply for ApiCallResult {
    fn unanswered() -> Self {
        Self::Refused {
            call: 0,
            refusal: Refusal::Refused { reason: Detail::new("bloomery driver closed before answering") },
        }
    }
}

impl ApiCallResult {
    /// The invocation's id for the call this answers.
    #[must_use]
    pub const fn call(&self) -> u64 {
        match self {
            Self::Replied { call, .. } | Self::Refused { call, .. } => *call,
        }
    }
}
