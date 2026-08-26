#![cfg(all(unix, feature = "github"))]

//! A janitor pass while a bloom is walking must not reclaim a session tree,
//! even one the live-set derivation does not name; a later pass, once the bloom
//! is terminal and nothing is outstanding, reclaims it.
//!
//! Production: board-5435, 2026-08-25. The janitor's live set — slug rows of
//! active-unlanded blooms' non-withdrawn members, plus outstanding orders —
//! missed sessions that were still resumable. The member's tree was reclaimed
//! mid-walk; every later refine lap resumed the conversation (`session_reuse.arm:
//! "resumed"`), edited a fresh checkout, captured a clean diff, and declined
//! (`produced_candidate: false`). Dispatches 3301 and 3318 are the two parks.
//! The coordinator journal that afternoon shows reclaim bursts against
//! `s-dispatch-3189` through `s-dispatch-3196` while `s-dispatch-3228` /
//! `s-dispatch-3229` were compiling in their trees.
//!
//! Unfixed, this scenario fails the first janitor pass: the planted tree is a
//! registered `sessions/<slug>/tree` the live set does not name, so
//! `reclaim_session_trees` releases it while the bloom is still sealed. With
//! the between-blooms gate it survives every pass until the bloom has landed
//! and the outstanding set is empty.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};

use aether_bloomery::BloomStatus;
use aether_harness_bloomery::{BloomeryHarness, digest};

const WORKPIECE: &str = "wp";

/// The production dispatch whose tree the 2026-08-25 janitor reclaimed while
/// later sessions were still compiling. Planted rather than minted, so the
/// live-set miss is the fixture: nothing in the journal names this slug.
const MISSED_SLUG: &str = "s-dispatch-3301";

/// How many coordinator ticks the bloom is given to land. Generous: every lane
/// is a real child over a real repository, and the fold has to run after the
/// member resolves.
const BUDGET: u32 = 600;

fn planted_tree(harness: &BloomeryHarness) -> PathBuf {
    harness.runs_dir().join("sessions").join(MISSED_SLUG).join("tree")
}

fn plant_missed_session_tree(harness: &BloomeryHarness) -> PathBuf {
    let tree = planted_tree(harness);
    fs::create_dir_all(tree.parent().unwrap()).expect("the session directory creates");
    let path = tree.to_string_lossy().into_owned();
    let _ = harness.repo().git(&["worktree", "add", "--detach", &path, harness.repo().head()]);
    assert!(tree.is_dir(), "the planted session tree is a real git worktree");
    tree
}

fn still_there(tree: &Path, when: &str) {
    assert!(tree.is_dir(), "session tree {MISSED_SLUG} survived {when}: {}", tree.display());
}

#[test]
fn a_janitor_pass_spares_session_trees_while_a_bloom_walks() {
    let mut harness = BloomeryHarness::start();
    let bloom = harness.seal_member(WORKPIECE, digest(0x51));
    let tree = plant_missed_session_tree(&harness);

    harness.janitor_tick();
    still_there(&tree, "the first janitor pass while the bloom is sealed");

    for _ in 0..BUDGET {
        harness.tick();
        harness.janitor_tick();
        let between = harness.bloom(bloom).status == BloomStatus::Landed && harness.outstanding().is_empty();
        if !tree.is_dir() {
            assert!(
                between,
                "session tree {MISSED_SLUG} was reclaimed while the bloom was still walking: status {:?} outstanding {:?}",
                harness.bloom(bloom).status,
                harness.outstanding(),
            );
            return;
        }
        if between {
            // The view can land on the tick that the janitor sampled as still
            // walking. One more pass is the between-blooms reclaim the gate is for.
            harness.janitor_tick();
            assert!(
                !tree.is_dir(),
                "once the bloom is terminal and nothing is outstanding, the between-blooms pass reclaims the tree",
            );
            return;
        }
    }

    panic!(
        "the bloom stayed {:?} with outstanding {:?} rather than landing inside {BUDGET} ticks",
        harness.bloom(bloom).status,
        harness.outstanding(),
    );
}
