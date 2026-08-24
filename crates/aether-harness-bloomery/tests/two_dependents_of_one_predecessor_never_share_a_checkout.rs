//! Two members depend on one predecessor: when it resolves they both dispatch,
//! and they must not both be handed the predecessor's session checkout.
//!
//! The fan-out is the case a linear reading of ADR-0196 misses. A session owns
//! its tree because every harness binds a conversation to the directory it was
//! born in, and a dependent continues its predecessor's conversation *in that
//! tree* — which is exactly right for a chain, A then B then C, each reset in
//! place. The member graph is a DAG: with edges A -> B and A -> C both
//! dependents unblock on one admission and dispatch in the same tick.
//!
//! Pre-fix each of them inherited A's slug, the checkout is a pure function of
//! the slug, and nothing at dispatch asked whether a live run already held that
//! path — so two lanes ran `git checkout --detach --force` plus `git clean
//! -ffdx` in one working tree and edited it concurrently. The later reset
//! deleted the earlier lane's in-progress work, and a capture could commit the
//! union of two members' edits as one candidate.
//!
//! Only the whole coordinator shows it: the edges are sealed into the reducer,
//! the readiness fold is what dispatches both dependents at once, and the
//! checkout each lane stands in is decided in the executor three crates away.
//! The mock lane records the directory it ran in, which is the one fact that
//! cannot be recovered any other way (#5425).

#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;

use aether_bloomery::CONSTRUCT_IMPLEMENT_COMMAND;
use aether_harness_bloomery::{BloomeryHarness, digest};

const PREDECESSOR: &str = "wp-a";
const FIRST: &str = "wp-b";
const SECOND: &str = "wp-c";

/// How many ticks the three constructs are given. Generous: every lane is a
/// real child process over a real repository, and the two dependents wait on
/// the predecessor's whole line before they dispatch at all.
const BUDGET: u32 = 600;

/// The directory each construct lane stood in, in dispatch order.
fn construct_worktrees(harness: &BloomeryHarness) -> Vec<String> {
    harness
        .ledger()
        .into_iter()
        .filter(|run| run.command == CONSTRUCT_IMPLEMENT_COMMAND)
        .map(|run| run.worktree.unwrap_or_default())
        .collect()
}

/// Those of them that are session checkouts, deduplicated. A lane that resolves
/// no session builds under `slot-<index>` instead — the aggregate lanes, which
/// are nobody's conversation — so only session trees are counted.
fn session_trees(worktrees: &[String]) -> BTreeSet<&str> {
    worktrees.iter().map(String::as_str).filter(|worktree| worktree.contains("/sessions/")).collect()
}

#[test]
fn two_dependents_of_one_predecessor_never_share_a_checkout() {
    let mut harness = BloomeryHarness::start();
    harness.seal_graph(
        &[(PREDECESSOR, digest(0x51)), (FIRST, digest(0x52)), (SECOND, digest(0x53))],
        &[(FIRST, PREDECESSOR), (SECOND, PREDECESSOR)],
    );

    // Ticked directly rather than through `run_until`, which consults the
    // liveness oracle on every still tick: these members carry fabricated scope
    // revisions, so the line *past* the fan-out walks into a stage whose sealed
    // revision the commission store cannot render. What this scenario is about
    // is the three constructs — the predecessor's, and one for each dependent
    // once its resolution unblocks them both.
    for _ in 0..BUDGET {
        if construct_worktrees(&harness).len() >= 3 {
            break;
        }
        harness.tick();
    }

    // At least three, rather than exactly: a member whose work was reset away
    // under it captures nothing, fails closed, and is dispatched again — which
    // is how the defect reads from here, and is not what this asserts on.
    let worktrees = construct_worktrees(&harness);
    assert!(
        worktrees.len() >= 3,
        "the fan-out dispatched both dependents once the predecessor resolved: {worktrees:?}",
    );
    let trees = session_trees(&worktrees);
    assert_eq!(
        trees.len(),
        2,
        "one dependent continues the predecessor's session in its tree, and the sibling that arrives second opens \
         its own rather than resetting a live checkout out from under it: {worktrees:?}",
    );
}
