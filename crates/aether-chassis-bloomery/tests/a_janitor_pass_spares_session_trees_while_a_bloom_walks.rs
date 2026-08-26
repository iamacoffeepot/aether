#![cfg(all(unix, feature = "github"))]

//! A janitor tick must not reclaim a session tree, even one the live-set
//! derivation does not name, while a bloom walks *or* after it lands.
//! Session trees are records (ADR-0211): the tick never deletes them; the
//! archive pass moves them when the operator asks between blooms.
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
//! registered `sessions/<slug>/tree` the live set does not name, so a tick
//! that still deleted records would release it while the bloom is still sealed.
//! The between-blooms gate keeps it through the walk; leaving records on the
//! tick keeps it after land.

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
        assert!(
            tree.is_dir(),
            "session tree {MISSED_SLUG} was reclaimed by the janitor tick: status {:?} outstanding {:?}",
            harness.bloom(bloom).status,
            harness.outstanding(),
        );
        if between {
            // The view can land on the tick that the janitor sampled as still
            // walking. One more pass is the between-blooms window the archive
            // pass uses; the tick itself must still leave the record in place.
            harness.janitor_tick();
            still_there(&tree, "the between-blooms tick after the bloom landed");
            return;
        }
    }

    panic!(
        "the bloom stayed {:?} with outstanding {:?} rather than landing inside {BUDGET} ticks",
        harness.bloom(bloom).status,
        harness.outstanding(),
    );
}
