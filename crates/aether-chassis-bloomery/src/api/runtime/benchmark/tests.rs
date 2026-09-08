//! Door-level coverage for `POST /benchmark`: the class gate and the stated-act
//! gate, both provable without a coordinator.

use aether_bloomery::StoreClass;
use aether_http::HttpServerResponse;

use super::parse;

fn body(reason: &str) -> Vec<u8> {
    serde_json::json!({
        "set": "wave",
        "base": "b0".repeat(32),
        "pull_requests": [5820],
        "cells": ["c1".repeat(32)],
        "samples": 2,
        "reason": reason,
        "operator": "operator",
    })
    .to_string()
    .into_bytes()
}

fn refused(store: StoreClass, body: &[u8]) -> HttpServerResponse {
    match parse(store, body) {
        Err(response) => response,
        Ok(_) => panic!("expected a refused benchmark request"),
    }
}

fn text(response: &HttpServerResponse) -> String {
    String::from_utf8_lossy(&response.body).into_owned()
}

// Tripwire: the class gate runs before the body is even looked at.
//
// A live-classed coordinator is the one host a benchmark must never run on, and
// the ordering is what makes that legible: an operator who pointed a run at
// production gets told the *host* is wrong, not that their JSON is. Validating
// the body first would answer `400` on a malformed request and only reach the
// `409` once the body was fixed — teaching exactly the wrong lesson about why it
// was refused, and leaving a well-formed body one keystroke from running.
#[test]
fn a_live_classed_coordinator_refuses_before_it_reads_the_body() {
    let refusal = refused(StoreClass::Live, &body("measure the line"));
    assert_eq!(refusal.status, 409);
    assert!(text(&refusal).contains("trial store"), "the refusal names the store a run needs: {}", text(&refusal));

    let garbage = refused(StoreClass::Live, b"not json at all");
    assert_eq!(garbage.status, 409, "a live host answers for its class, not for the body it never parsed");
}

// A benchmark run is an operator act, gated the way `/hold`, `/repair` and
// `/supersede` are: a blank reason or an unnamed operator is refused rather than
// defaulted, so the journal never carries a run nobody owns.
#[test]
fn a_trial_coordinator_still_refuses_an_unstated_run() {
    let refusal = refused(StoreClass::Trial, &body("   "));
    assert_eq!(refusal.status, 422);

    assert!(parse(StoreClass::Trial, &body("measure the line")).is_ok(), "a stated run on a trial host parses");
}
