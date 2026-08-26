//! Auth, refusal mapping, and response-status tests for commission routes.

use aether_bloomery::{
    ApprovalPolicy, ApprovalRule, Digest, KeyId, SCOPE_REVISION_SCHEMA, ScopeRevision, ScopeRouting, Tier, WorkpieceId,
    digest_of, signed_cancel,
};
use aether_data::wire::from_bytes;
use aether_http::{HttpHeader, HttpServerRequest, HttpServerResponse};

use super::{
    approval_response, authorize, auto_approval_write, cancel_request, cancel_response, create_response, list_response,
    query_status, reopen_request, reopen_response, revision_response, scope_run_response, show_response,
};
use crate::api::dto::{CancelCommissionRequest, ReopenCommissionRequest};
use crate::store::{
    CancelCommissionResult, CreateCommissionResult, EnqueueScopeRunResult, ListCommissionsResult, LoadCommissionResult,
    RecordCommissionApproval, RecordCommissionApprovalResult, ReopenCommissionResult, WriteScopeRevisionResult,
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
            current_unreadable: None,
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
fn an_unreadable_current_revision_is_shown_not_a_500() {
    // A 500 on an intact older row is the operator-facing form of taking the
    // whole commission down: show cannot print the tip, and scope cannot name
    // it as predecessor. The body is unreadable; the head is not.
    let digest = vec![7; 32];
    let marked = show_response(LoadCommissionResult::Ok {
        id: "wp-1".to_owned(),
        intent: digest.clone(),
        current_revision: Some(digest.clone()),
        current_ordinal: Some(1),
        status: "open".to_owned(),
        current: None,
        approvals: Vec::new(),
        scope_verify: None,
        current_unreadable: Some("canonical commission bytes are malformed".to_owned()),
    });
    let from_bytes = show_response(LoadCommissionResult::Ok {
        id: "wp-1".to_owned(),
        intent: digest.clone(),
        current_revision: Some(digest),
        current_ordinal: Some(1),
        status: "open".to_owned(),
        current: Some(vec![0xff, 0x00]),
        approvals: Vec::new(),
        scope_verify: None,
        current_unreadable: None,
    });
    assert_eq!(marked.status, 200, "a store-marked unreadable tip is still a commission: {}", error_text(&marked));
    assert_eq!(
        from_bytes.status,
        200,
        "bytes this binary cannot decode are still a commission: {}",
        error_text(&from_bytes)
    );
    assert!(
        error_text(&marked).contains("canonical commission bytes are malformed"),
        "the marker carries the reason: {}",
        error_text(&marked)
    );
    assert!(
        error_text(&from_bytes).contains("malformed"),
        "the api-side decode names the same class of failure: {}",
        error_text(&from_bytes)
    );
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

/// A stored revision for `id` declaring exactly `surface`.
fn revision_declaring(id: &str, surface: &[&str]) -> ScopeRevision {
    ScopeRevision {
        schema: SCOPE_REVISION_SCHEMA,
        workpiece: WorkpieceId(id.to_owned()),
        predecessor: None,
        problem: "problem".to_owned(),
        design: "design".to_owned(),
        plan: "plan".to_owned(),
        declared_surface: surface.iter().map(|glob| (*glob).to_owned()).collect(),
        dogfood_brief: String::new(),
        routing: ScopeRouting { size: "S".to_owned(), model: "construct: test".to_owned() },
        dependencies: Vec::new(),
        description: "advisory".to_owned(),
        implements: Vec::new(),
        declared_crates: Vec::new(),
        declared_reads: Vec::new(),
    }
}

/// A store load carrying `revision` as the open commission's current tip.
fn loaded_open(id: &str, revision: &ScopeRevision) -> LoadCommissionResult {
    LoadCommissionResult::Ok {
        id: id.to_owned(),
        intent: vec![2; 32],
        current_revision: Some(digest_of(revision).as_bytes().to_vec()),
        current_ordinal: Some(1),
        status: "open".to_owned(),
        current: Some(revision.to_canonical()),
        approvals: Vec::new(),
        scope_verify: None,
        current_unreadable: None,
    }
}

/// `docs/guide/**` advances on its own; everything else stops at the owner.
fn ladder() -> ApprovalPolicy {
    ApprovalPolicy {
        default: Tier::Human,
        rules: vec![ApprovalRule { glob: "docs/guide/**".to_owned(), tier: Tier::Auto }],
    }
}

fn refused_auto(result: Result<RecordCommissionApproval, HttpServerResponse>) -> HttpServerResponse {
    match result {
        Err(response) => response,
        Ok(_) => panic!("expected a refused auto approval"),
    }
}

#[test]
fn an_auto_surface_mints_an_unsigned_approval_bound_to_the_stored_revision() {
    // The producer #5325 says the store models but nothing writes. The bug this
    // catches is a door that binds the wrong digest — the intent, or the load's
    // index column rather than a recompute over the canonical bytes — which
    // would file the approval against a revision it does not approve.
    let revision = revision_declaring("wp-1", &["docs/guide/**"]);
    let write = auto_approval_write(Some(&ladder()), &WorkpieceId("wp-1".to_owned()), loaded_open("wp-1", &revision))
        .expect("an auto-tier surface mints its own approval");

    let statement: aether_bloomery::Statement = from_bytes(&write.statement).expect("the minted statement decodes");

    assert_eq!(write.id, "wp-1", "the write is addressed to the commission the path named");
    assert_eq!(statement.words, digest_of(&revision).as_bytes(), "the approval binds the stored revision's digest");
    assert!(
        !statement.is_instruction_capable(),
        "an auto approval is the gate's observation, never an author signature"
    );
}

#[test]
fn an_above_auto_surface_is_refused_naming_the_tier_it_resolved() {
    // The line that keeps an unsigned producer from becoming a way to
    // self-approve anything: the door reads the stored surface itself and
    // refuses upward. A caller cannot claim the tier, because it never supplies
    // one.
    let revision = revision_declaring("wp-1", &["crates/aether-data/src/**"]);

    let response = refused_auto(auto_approval_write(
        Some(&ladder()),
        &WorkpieceId("wp-1".to_owned()),
        loaded_open("wp-1", &revision),
    ));

    assert_eq!(response.status, 422);
    assert!(error_text(&response).contains("Human"), "the refusal names the tier it found: {}", error_text(&response));
}

#[test]
fn an_unreadable_ladder_refuses_rather_than_defaulting_to_auto() {
    // No policy is not "no restriction". A door that minted an approval because
    // it could not read the ladder would grant exactly the tier it failed to
    // check.
    let revision = revision_declaring("wp-1", &["docs/guide/**"]);

    let response =
        refused_auto(auto_approval_write(None, &WorkpieceId("wp-1".to_owned()), loaded_open("wp-1", &revision)));

    assert_eq!(response.status, 422, "an unloadable policy fails closed");
}

#[test]
fn a_commission_with_nothing_to_approve_is_refused() {
    // Each of these would otherwise mint an approval for something no operator
    // could act on: a closed commission, or one whose scope has not been
    // written yet.
    let revision = revision_declaring("wp-1", &["docs/guide/**"]);
    let id = WorkpieceId("wp-1".to_owned());

    let LoadCommissionResult::Ok { intent, current_revision, current_ordinal, current, .. } =
        loaded_open("wp-1", &revision)
    else {
        panic!("the helper builds an Ok load");
    };
    let landed = LoadCommissionResult::Ok {
        id: "wp-1".to_owned(),
        intent: intent.clone(),
        current_revision,
        current_ordinal,
        status: "landed".to_owned(),
        current,
        approvals: Vec::new(),
        scope_verify: None,
        current_unreadable: None,
    };
    let unscoped = LoadCommissionResult::Ok {
        id: "wp-1".to_owned(),
        intent,
        current_revision: None,
        current_ordinal: None,
        status: "open".to_owned(),
        current: None,
        approvals: Vec::new(),
        scope_verify: None,
        current_unreadable: None,
    };

    assert_eq!(refused_auto(auto_approval_write(Some(&ladder()), &id, landed)).status, 422, "a closed commission");
    assert_eq!(refused_auto(auto_approval_write(Some(&ladder()), &id, unscoped)).status, 422, "an unscoped commission");
    assert_eq!(
        refused_auto(auto_approval_write(
            Some(&ladder()),
            &id,
            LoadCommissionResult::Missing { id: "wp-1".to_owned() }
        ))
        .status,
        404,
        "an unknown commission"
    );
}
