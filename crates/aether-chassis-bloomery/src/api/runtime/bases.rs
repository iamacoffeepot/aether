//! The base-scoped operator routes: re-run `verify.base` on a red receipt.
//!
//! The receipt ledger is snapshot-scoped, so this door is keyed by a commit,
//! not by a bloom — the same posture the claims routes take for a resource
//! that is not bloom-scoped.

use aether_bloomery::{BaseReverify, Event, Fact, IdempotencyKey, digest_of};

use super::blooms::unstated;
use super::hex::{self, digest_from_hex, hex_encode};
use super::response::error_response;
use super::state::{ApiCapabilityState, Routed, admit};
use crate::api::dto::ReverifyBaseRequest;

impl ApiCapabilityState {
    /// `POST /bases/{base}/reverify` — run `verify.base` again on a red receipt
    /// whose failure the operator has judged does not describe the tree.
    ///
    /// Journal-first like its siblings: the route appends one
    /// `Fact::BaseReverify` and nothing else. The reducer overwrites the red as
    /// pending and queues the dispatch; the gates still judge.
    pub(super) fn reverify_base(base: &str, body: &[u8]) -> Routed {
        let Some(base) = digest_from_hex(base) else {
            return Routed::Reply(error_response(400, "base is not a 32-byte hex digest"));
        };
        let request: ReverifyBaseRequest = match hex::from_slice(body) {
            Ok(request) => request,
            Err(error) => return Routed::Reply(error_response(400, &format!("invalid reverify body: {error}"))),
        };
        let ReverifyBaseRequest { reason, operator, idempotency_key } = request;
        if let Some(refusal) = unstated(&reason, &operator) {
            return Routed::Reply(refusal);
        }

        let reverify = BaseReverify { base, reason, operator };
        let key = idempotency_key.unwrap_or_else(|| {
            format!(
                "aether.bloomery.base_reverify:{}:{}",
                hex_encode(base.as_bytes()),
                hex_encode(digest_of(&reverify).as_bytes())
            )
        });

        admit(&Event { idempotency_key: IdempotencyKey(key), fact: Fact::BaseReverify(reverify) })
    }
}
