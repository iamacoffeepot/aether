//! The order-scoped operator route: drop one outstanding order without faulting
//! its lane.

use aether_http::HttpServerResponse;

use super::blooms::unstated;
use super::hex;
use super::response::{error_response, json};
use super::state::Routed;
use crate::api::dto::{CancelOrderRequest, CancelOrderView};
use crate::store::{CancelOrder, CancelOrderResult};

/// `POST /orders/{nonce}/cancel` — drop the outstanding order at `nonce` and
/// record the operator cancellation so its later upload is ignored.
pub(super) fn cancel_order(nonce: &str, body: &[u8]) -> Routed {
    if nonce.trim().is_empty() {
        return Routed::Reply(error_response(400, "order nonce must not be blank"));
    }
    let request: CancelOrderRequest = match hex::from_slice(body) {
        Ok(request) => request,
        Err(error) => return Routed::Reply(error_response(400, &format!("invalid cancel-order body: {error}"))),
    };
    let CancelOrderRequest { reason, operator } = request;
    if let Some(refusal) = unstated(&reason, &operator) {
        return Routed::Reply(refusal);
    }
    Routed::CancelOrder(CancelOrder { nonce: nonce.to_owned(), reason, operator })
}

/// Render the store's cancel reply.
pub(super) fn cancel_response(result: CancelOrderResult) -> HttpServerResponse {
    match result {
        CancelOrderResult::Ok { nonce, removed } => json(200, &CancelOrderView { nonce, cancelled: removed }),
        CancelOrderResult::Err { error } => error_response(500, &error),
    }
}
