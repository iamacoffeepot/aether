//! The rate bucket and the in-flight pool.
//!
//! Both are global, because the stateless profile has no caller to key on. That
//! makes their failure modes shared rather than isolated, which is exactly why
//! the refusal has to give the number back: a client told only "busy" retries on
//! a schedule the server never chose.

use crate::runtime::admission::{InFlightPool, RateLimiter};

/// The burst is spendable immediately, and the refusal that follows names a
/// delay that actually produces a token.
#[test]
fn the_bucket_spends_its_burst_then_names_a_workable_retry() {
    let mut limiter = RateLimiter::new(60, 2, 0);

    assert!(limiter.admit(0).is_ok(), "the first burst token");
    assert!(limiter.admit(0).is_ok(), "the second burst token");
    let retry_after_millis = limiter.admit(0).expect_err("the burst is spent");

    assert!(retry_after_millis > 0, "a retry hint of zero would invite an immediate re-refusal");
    assert!(
        limiter.admit(retry_after_millis).is_ok(),
        "waiting the advertised {retry_after_millis} ms must actually produce a token",
    );
}

/// Refill is continuous, not per-period: a burst arriving just after the bucket
/// empties waits a proportional time rather than a whole minute.
#[test]
fn refill_is_proportional_to_elapsed_time() {
    let mut limiter = RateLimiter::new(60_000, 1, 0);

    assert!(limiter.admit(0).is_ok());
    assert!(limiter.admit(0).is_err(), "the single burst token is spent");
    assert!(limiter.admit(1).is_ok(), "one millisecond at 60,000 per minute is one token");
}

/// A rate of zero can never produce a token, so the honest hint is the whole
/// period rather than a number that reads as "try again immediately".
#[test]
fn a_zero_rate_reports_a_bounded_hint_rather_than_an_impossible_one() {
    let mut limiter = RateLimiter::new(0, 1, 0);

    assert!(limiter.admit(0).is_ok(), "the burst is still spendable");
    let retry_after_millis = limiter.admit(0).expect_err("no token can ever refill");

    assert_eq!(retry_after_millis, 60_000);
}

/// A refused acquisition takes nothing. A pool that charged for a refusal would
/// leak a permit per rejected request and wedge itself under exactly the load
/// it exists to survive.
#[test]
fn a_refused_permit_is_not_charged() {
    let mut pool = InFlightPool::new(2);

    assert!(pool.acquire());
    assert!(pool.acquire());
    assert!(!pool.acquire(), "the pool is full");
    assert!(!pool.acquire(), "and stays full");
    assert_eq!(pool.held(), 2, "a refusal must not have taken a permit");

    pool.release();
    assert!(pool.acquire(), "a released permit is reusable");
}

/// Releasing more than was taken cannot drive the count below zero and hand out
/// permits the pool never had.
#[test]
fn releasing_an_empty_pool_cannot_manufacture_permits() {
    let mut pool = InFlightPool::new(1);

    pool.release();
    pool.release();

    assert_eq!(pool.held(), 0);
    assert!(pool.acquire());
    assert!(!pool.acquire(), "the ceiling still binds after a spurious release");
}
