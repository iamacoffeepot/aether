//! Auth, refusal mapping, and response-status tests for commission routes.

use aether_http::{HttpHeader, HttpServerRequest, HttpServerResponse};

use super::{
    approval_response, authorize, cancel_response, create_response, list_response, query_status, revision_response,
    show_response,
};
use crate::store::{
    CancelCommissionResult, CreateCommissionResult, ListCommissionsResult, LoadCommissionResult,
    RecordCommissionApprovalResult, WriteScopeRevisionResult,
};

fn request(authorization: Option<&str>) -> HttpServerRequest {
    HttpServerRequest {
        method: aether_http::HttpMethod::Get,
        path: "/commissions".to_owned(),
        query: String::new(),
        headers: authorization
            .map(|value| vec![HttpHeader { name: "Authorization".to_owned(), value: value.to_owned() }])
            .unwrap_or_default(),
        body: Vec::new(),
        peer_addr: "127.0.0.1:1".to_owned(),
    }
}

fn refused(request: &HttpServerRequest, token: &str) -> HttpServerResponse {
    match authorize(request, token) {
        Err(response) => response,
        Ok(()) => panic!("expected a refused request"),
    }
}

#[test]
fn an_unauthenticated_request_is_refused() {
    // A missing, empty, or wrong bearer must not reach the store. The
    // configured token being empty is also a refusal: the surface that can
    // approve work is fail-closed when nothing has been configured.
    assert_eq!(refused(&request(None), "secret").status, 401);
    assert_eq!(refused(&request(Some("Bearer secret")), "").status, 401);
    assert_eq!(refused(&request(Some("Bearer other")), "secret").status, 401);
    assert_eq!(refused(&request(Some("Basic secret")), "secret").status, 401);
    assert!(authorize(&request(Some("Bearer secret")), "secret").is_ok());
}

#[test]
fn a_tampered_or_stale_write_is_not_a_transport_error() {
    // 4xx names a refused envelope; 5xx names a store/transport failure. A
    // client that treats every non-200 as a retryable transport error would
    // replay a stale or tampered statement.
    assert_eq!(revision_response(WriteScopeRevisionResult::Stale).status, 409);
    assert_eq!(revision_response(WriteScopeRevisionResult::Malformed).status, 400);
    assert_eq!(revision_response(WriteScopeRevisionResult::Err { error: "disk".to_owned() }).status, 500);
    assert_eq!(approval_response(RecordCommissionApprovalResult::Stale).status, 409);
    assert_eq!(approval_response(RecordCommissionApprovalResult::MissingRevision).status, 404);
    assert_eq!(approval_response(RecordCommissionApprovalResult::Refused { error: "wrong".to_owned() }).status, 400);
    assert_eq!(approval_response(RecordCommissionApprovalResult::Err { error: "disk".to_owned() }).status, 500);
    assert_eq!(cancel_response(CancelCommissionResult::WrongSubject).status, 400);
    assert_eq!(cancel_response(CancelCommissionResult::NotOpen).status, 409);
    assert_eq!(cancel_response(CancelCommissionResult::Err { error: "disk".to_owned() }).status, 500);
}

#[test]
fn every_route_result_has_a_success_status() {
    let digest = vec![7; 32];
    assert_eq!(
        create_response(CreateCommissionResult::Ok { id: "wp-1".to_owned(), digest: digest.clone() }).status,
        201
    );
    assert_eq!(revision_response(WriteScopeRevisionResult::Ok { digest: digest.clone() }).status, 201);
    assert_eq!(
        show_response(LoadCommissionResult::Ok {
            id: "wp-1".to_owned(),
            intent: digest.clone(),
            current_revision: None,
            current_ordinal: None,
            status: "open".to_owned(),
            current: None,
            approvals: Vec::new(),
        })
        .status,
        200
    );
    assert_eq!(list_response(ListCommissionsResult::Ok { commissions: Vec::new() }).status, 200);
    assert_eq!(cancel_response(CancelCommissionResult::Ok { id: "wp-1".to_owned(), digest }).status, 200);
    assert_eq!(create_response(CreateCommissionResult::Duplicate { id: "wp-1".to_owned() }).status, 409);
    assert_eq!(show_response(LoadCommissionResult::Missing { id: "wp-1".to_owned() }).status, 404);
}

#[test]
fn list_query_reads_the_status_filter() {
    assert_eq!(query_status("status=open"), Some("open".to_owned()));
    assert_eq!(query_status("foo=1&status=cancelled"), Some("cancelled".to_owned()));
    assert_eq!(query_status(""), None);
}
