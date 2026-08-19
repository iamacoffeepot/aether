#![cfg(feature = "github")]

//! Executor-lane correctness for the three defects taken in one visit.
//!
//! The drain-guard and backoff-window cases need the crate-private drain
//! (`a_superseded_blooms_queued_redispatch_is_retired_rather_than_replayed`,
//! `a_clean_drain_leaves_the_backoff_window_intact` in the executor reactor
//! runtime tests). Process-group teardown needs the private `ChildProcess`
//! (`killing_a_lane_child_terminates_its_grandchildren` in the process runner
//! tests). This file pins the public mock-lane argv consumer: a novel
//! `--flag value` pair must not steal the positional command.

#![allow(clippy::unwrap_used, reason = "a parser test that cannot parse its own fixture reports it by panicking")]

use std::path::Path;

use aether_chassis_bloomery::bloomery::mock_lane::argv;

fn argv_words(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

#[test]
fn a_novel_flag_value_pair_does_not_steal_the_positional_command() {
    // Tripwire: the mock consumed values only for five enumerated flags. Any
    // other `--flag value` pair leaked `value` into the last-positional-wins
    // command, silently corrupting the parsed transform id.
    let args = argv::parse(argv_words(&[
        "verify.check",
        "--out",
        "/tmp/e",
        "--nonce",
        "n",
        "--brand-new-flag",
        "leaked-value",
    ]))
    .unwrap();

    assert_eq!(args.command, "verify.check");
    assert_eq!(args.out, Path::new("/tmp/e"));
    assert_eq!(args.nonce, "n");
}
