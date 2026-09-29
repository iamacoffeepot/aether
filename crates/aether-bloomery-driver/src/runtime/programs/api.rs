//! Program API calls relayed through the driver (ADR-0240 D6).
//!
//! An invocation sends each API call to its bundle root, which relays it to
//! the driver that sent the `Invoke`. The driver maps the API to a provider
//! it holds, decoding the one request kind that API carries, or refuses it
//! at once. A `Workspace` call carries a `RunRequest`, which names no
//! storage; the shell sends it on as a `Run` over its own unit's journal, so
//! a program cannot name the storage its run reads and writes (ADR-0240
//! I-5). A mapped call parks under an [`ApiTicket`] until the provider
//! answers; each caller is answered exactly once.

use aether_bloomery_kinds::{ApiCall, ApiCallResult, Detail, ProgramApi, Refusal};
use aether_data::{Kind, KindId};

use crate::runtime::core::{ApiReply, ApiTicket, CallerId, Command, ProgramCore};

/// Whether this unit holds a provider for `api`. `Http` maps to the http
/// capability and `Workspace` to the unit's workspace; `Process` has none.
pub const fn provided(api: ProgramApi) -> bool {
    match api {
        ProgramApi::Http | ProgramApi::Workspace => true,
        ProgramApi::Process => false,
    }
}

impl ProgramCore {
    /// Accept one bundle root's relayed program API call. The returned
    /// [`CallerId`] identifies the call for its exactly-once
    /// [`ApiAnswered`](Command::ApiAnswered).
    ///
    /// An API with a provider sends its decoded request there. An API with
    /// no provider, or a payload that is not the API's request kind, is
    /// answered [`Refusal::Refused`] at once.
    pub fn call_api(&mut self, request: ApiCall) -> (CallerId, Vec<Command>) {
        let caller = self.mint(CallerId::mint);
        let mut out = Vec::new();
        if self.aborted {
            return (caller, out);
        }
        let ApiCall { call, api, kind, payload } = request;
        let command = match api {
            ProgramApi::Http => decode::<aether_http::Fetch>(kind, &payload)
                .map(|request| Command::Fetch { ticket: self.park_api(caller, call), request }),
            ProgramApi::Workspace => decode::<aether_bloomery_workspace::RunRequest>(kind, &payload)
                .map(|request| Command::RunWorkspace { ticket: self.park_api(caller, call), request }),
            ProgramApi::Process => Err(format!("{api:?} has no provider in this unit")),
        };
        out.push(command.unwrap_or_else(|reason| Command::ApiAnswered {
            caller,
            result: ApiCallResult::Refused { call, refusal: Refusal::Refused { reason: Detail::new(reason) } },
        }));
        (caller, out)
    }

    /// Feed one provider's reply to a relayed call, encoded for the relay
    /// back as its kind and bytes. Unknown tickets return no commands.
    pub fn on_api_reply(&mut self, ticket: ApiTicket, reply: ApiReply) -> Vec<Command> {
        let mut out = Vec::new();
        if self.aborted {
            return out;
        }
        let Some((caller, call)) = self.api_calls.remove(&ticket) else {
            return out;
        };
        let (kind, payload) = match reply {
            ApiReply::Fetch(result) => (aether_http::FetchResult::ID, result.encode_into_bytes()),
            ApiReply::Workspace(result) => (aether_bloomery_workspace::RunResult::ID, result.encode_into_bytes()),
        };
        out.push(Command::ApiAnswered { caller, result: ApiCallResult::Replied { call, kind, payload } });
        out
    }

    /// Park `caller` for the invocation's `call` under a fresh ticket.
    fn park_api(&mut self, caller: CallerId, call: u64) -> ApiTicket {
        let ticket = self.mint(ApiTicket::mint);
        self.api_calls.insert(ticket, (caller, call));
        ticket
    }
}

/// `payload` decoded as `K`, when `kind` names `K` and the bytes decode.
fn decode<K: Kind>(kind: KindId, payload: &[u8]) -> Result<K, String> {
    if kind != K::ID {
        return Err(format!("the call carries kind {kind:?}, not {}", K::NAME));
    }
    K::decode_from_bytes(payload).ok_or_else(|| format!("the call's {} payload does not decode", K::NAME))
}
