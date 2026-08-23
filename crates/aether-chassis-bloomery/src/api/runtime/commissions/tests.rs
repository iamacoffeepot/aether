//! Auth, refusal mapping, and response-status tests for commission routes.

use aether_bloomery::{Digest, KeyId, signed_cancel};
use aether_http::{HttpHeader, HttpServerRequest, HttpServerResponse};

use super::{
    approval_response, authorize, cancel_request, cancel_response, create_response, list_response, query_status,
    reopen_request, reopen_response, revision_response, scope_run_response, show_response,
};
use crate::api::dto::{CancelCommissionRequest, ReopenCommissionRequest};
use crate::store::{
    CancelCommissionResult, CreateCommissionResult, EnqueueScopeRunResult, ListCommissionsResult, LoadCommissionResult,
    RecordCommissionApprovalResult, ReopenCommissionResult, WriteScopeRevisionResult,
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

fn signed_cancel_of(intent: Digest) -> aether_bloomery::Statement {
    signed_cancel(KeyId("operator".into()), &[7_u8; 32], intent)
}

fn encode_cancel(statement: &aether_bloomery::Statement, reason: &str) -> Vec<u8> {
    serde_json::to_vec(&CancelCommissionRequest { statement: statement.clone(), reason: reason.to_owned() })
        .expect("cancel body encodes")
}

fn refused_cancel(
    result: Result<(aether_bloomery::Statement, Digest, String), HttpServerResponse>,
) -> HttpServerResponse {
    match result {
        Err(response) => response,
        Ok(_) => panic!("expected a refused cancel request"),
    }
}

fn error_text(response: &HttpServerResponse) -> String {
    String::from_utf8_lossy(&response.body).into_owned()
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
    assert_eq!(revision_response(WriteScopeRevisionResult::NotOpen).status, 409);
    assert_eq!(revision_response(WriteScopeRevisionResult::Malformed).status, 400);
    assert_eq!(revision_response(WriteScopeRevisionResult::Err { error: "disk".to_owned() }).status, 500);
    assert_eq!(approval_response(RecordCommissionApprovalResult::Stale).status, 409);
    assert_eq!(approval_response(RecordCommissionApprovalResult::NotOpen).status, 409);
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
        scope_run_response(EnqueueScopeRunResult::Ok {
            id: "wp-1".to_owned(),
            ordinal: 1,
            sequence: 1,
            subject: digest.clone(),
        })
        .status,
        201
    );
    assert_eq!(
        show_response(LoadCommissionResult::Ok {
            id: "wp-1".to_owned(),
            intent: digest.clone(),
            current_revision: None,
            current_ordinal: None,
            status: "open".to_owned(),
            current: None,
            approvals: Vec::new(),
            scope_verify: None,
        })
        .status,
        200
    );
    assert_eq!(list_response(ListCommissionsResult::Ok { commissions: Vec::new() }).status, 200);
    assert_eq!(cancel_response(CancelCommissionResult::Ok { id: "wp-1".to_owned(), digest }).status, 200);
    assert_eq!(create_response(CreateCommissionResult::Duplicate { id: "wp-1".to_owned() }).status, 409);
    assert_eq!(scope_run_response(EnqueueScopeRunResult::Missing { id: "wp-1".to_owned() }).status, 404);
    assert_eq!(scope_run_response(EnqueueScopeRunResult::AlreadyInFlight { ordinal: 1 }).status, 409);
    assert_eq!(show_response(LoadCommissionResult::Missing { id: "wp-1".to_owned() }).status, 404);
}

#[test]
fn list_query_reads_the_status_filter() {
    assert_eq!(query_status("status=open"), Some("open".to_owned()));
    assert_eq!(query_status("foo=1&status=cancelled"), Some("cancelled".to_owned()));
    assert_eq!(query_status(""), None);
}

#[test]
fn a_cancel_with_a_blank_reason_is_refused_before_the_signature_is_read() {
    // An unexplained cancel is a board entry nobody can account for later.
    let statement = signed_cancel_of(Digest::from_bytes([3; 32]));
    let blank = refused_cancel(cancel_request(&encode_cancel(&statement, "")));
    let whitespace = refused_cancel(cancel_request(&encode_cancel(&statement, " \t\n")));
    assert_eq!(blank.status, 400);
    assert_eq!(whitespace.status, 400);
    assert!(error_text(&blank).contains("cancel reason is required"), "blank: {}", error_text(&blank));
    assert!(error_text(&whitespace).contains("cancel reason is required"), "whitespace: {}", error_text(&whitespace));
}

#[test]
fn a_cancel_body_that_is_not_the_request_envelope_is_a_400_not_a_500() {
    let body = serde_json::to_vec(&signed_cancel_of(Digest::from_bytes([3; 32]))).expect("a statement encodes");
    let response = refused_cancel(cancel_request(&body));
    assert_eq!(response.status, 400);
    assert!(
        error_text(&response).contains("invalid cancel body"),
        "a bare Statement is a decode refusal: {}",
        error_text(&response)
    );
}

#[test]
fn a_cancel_request_carries_the_reason_and_the_intent_through() {
    // Tripwire: a reason dropped at the route makes `--reason` a required argument that means nothing.
    let intent = Digest::from_bytes([3; 32]);
    let statement = signed_cancel_of(intent);
    let reason = "the work landed on a sibling branch";
    let (got_statement, got_intent, got_reason) =
        cancel_request(&encode_cancel(&statement, reason)).expect("a valid cancel body is accepted");
    assert_eq!(got_statement, statement);
    assert_eq!(got_intent, intent);
    assert_eq!(got_reason, reason);
}

#[test]
fn a_cancel_whose_words_are_not_a_digest_is_refused() {
    let mut statement = signed_cancel_of(Digest::from_bytes([3; 32]));
    statement.words = b"not-a-digest".to_vec();
    let response = refused_cancel(cancel_request(&encode_cancel(&statement, "landed elsewhere")));
    assert_eq!(response.status, 400);
    assert!(error_text(&response).contains("cancel words are not an intent digest"), "{}", error_text(&response));
}

#[test]
fn a_reopen_refusal_carries_the_status_the_operator_needs_to_act_on() {
    // A 409 that says only "refused" sends the operator back to the board to
    // work out which of the two guards stopped them. Each refusal names the
    // thing that decided it.
    let resolved = reopen_response(ReopenCommissionResult::Resolved { bloom: "abcd".to_owned() });
    let not_landed = reopen_response(ReopenCommissionResult::NotLanded { status: "cancelled".to_owned() });

    assert_eq!(resolved.status, 409);
    assert!(error_text(&resolved).contains("abcd"), "the resolving bloom is named: {}", error_text(&resolved));
    assert_eq!(not_landed.status, 409);
    assert!(
        error_text(&not_landed).contains("cancelled"),
        "the status it is actually in is named: {}",
        error_text(&not_landed)
    );
    assert_eq!(reopen_response(ReopenCommissionResult::Missing { id: "wp-1".to_owned() }).status, 404);
    assert_eq!(reopen_response(ReopenCommissionResult::WrongSubject).status, 400);
    assert_eq!(reopen_response(ReopenCommissionResult::Err { error: "disk".to_owned() }).status, 500);
}

#[test]
fn a_reopen_with_a_blank_reason_is_refused_before_the_signature_is_read() {
    // The same door-side check the cancel runs, and the refusal says which act
    // it refused rather than leaving the operator to guess the route.
    let statement = signed_cancel_of(Digest::from_bytes([3; 32]));
    let body = serde_json::to_vec(&ReopenCommissionRequest { statement, reason: " \t".to_owned() })
        .expect("reopen body encodes");

    let Err(response) = reopen_request(&body) else {
        panic!("expected a refused reopen request");
    };

    assert_eq!(response.status, 400);
    assert!(error_text(&response).contains("reopen reason is required"), "{}", error_text(&response));
}
