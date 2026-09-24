//! Fetch-on-miss scenarios: shared journal reads and the driver's artifact cache.
//!
//! Each test counts the journal reads the core issues for one digest, so a
//! read the cache or an in-flight read should have absorbed shows up as an
//! extra entry in `reads_seen`.

mod support;

use aether_bloomery_driver::{CallerId, Command};
use aether_bloomery_kinds::{Digest, OpaqueBytes, ReadArtifact, ReadArtifactResult, Utf8Text, artifact_digest};
use aether_data::Kind;
use support::{World, bundle_wasm, digest};

/// Fetch `digest` the way a bundle root's fetch-on-miss reaches the core.
fn fetch(world: &mut World, digest: Digest) -> (CallerId, Vec<Command>) {
    world.core.fetch_artifact(ReadArtifact { digest })
}

/// Journal reads the core issued for `digest`.
fn reads_of(world: &World, digest: Digest) -> usize {
    world.reads_seen.iter().filter(|seen| **seen == digest).count()
}

/// The `Found` answer for one stored text.
fn found(digest: Digest, text: &[u8]) -> ReadArtifactResult {
    ReadArtifactResult::Found { digest, kind: Utf8Text::ID, bytes: text.to_vec() }
}

#[test]
fn concurrent_fetches_share_one_read_and_a_repeat_reads_nothing() {
    // Catches a journal read per waiter, a waiter that is never answered, a
    // fetch that joins another digest's read, and a cache that is never
    // filled.
    let (mut world, startup) = World::open();
    assert!(world.drive(startup).is_empty());
    let text = world.store(Utf8Text::ID, b"hello");
    let unstored = digest(9);

    let (first, first_commands) = fetch(&mut world, text);
    let (second, second_commands) = fetch(&mut world, text);
    let (other, other_commands) = fetch(&mut world, unstored);
    assert!(world.drive(first_commands).is_empty());
    assert!(world.drive(second_commands).is_empty());
    assert!(world.drive(other_commands).is_empty());
    assert_eq!(reads_of(&world, text), 1, "concurrent fetches share one read");
    assert_eq!(reads_of(&world, unstored), 1, "another digest reads on its own");
    assert_eq!(
        world.fetched,
        [
            (first, found(text, b"hello")),
            (second, found(text, b"hello")),
            (other, ReadArtifactResult::Missing { digest: unstored }),
        ]
    );

    let (third, commands) = fetch(&mut world, text);
    assert!(world.drive(commands).is_empty());
    assert_eq!(reads_of(&world, text), 1, "a repeat fetch is answered from the cache");
    assert_eq!(world.fetched.last(), Some(&(third, found(text, b"hello"))));
}

#[test]
fn a_missing_fetch_is_not_cached() {
    // Catches a cached `Missing` that hides a later-stored artifact from
    // programs for the engine's life.
    let (mut world, startup) = World::open();
    assert!(world.drive(startup).is_empty());
    let late = artifact_digest(Utf8Text::ID, b"late");

    let (first, commands) = fetch(&mut world, late);
    assert!(world.drive(commands).is_empty());
    assert_eq!(world.fetched, [(first, ReadArtifactResult::Missing { digest: late })]);

    assert_eq!(world.store(Utf8Text::ID, b"late"), late);
    let (second, commands) = fetch(&mut world, late);
    assert!(world.drive(commands).is_empty());
    assert_eq!(reads_of(&world, late), 2, "a missing artifact is read again");
    assert_eq!(world.fetched.last(), Some(&(second, found(late, b"late"))));
}

#[test]
fn a_fetch_while_catching_up_is_answered() {
    // Catches fetches held behind the journal sync the way calls are.
    let (mut world, startup) = World::open();
    let bundle =
        world.store(OpaqueBytes::ID, &bundle_wasm(&[("run", Utf8Text::ID, OpaqueBytes::ID, "run it")], &[], b"prog"));
    world.seed_move("prog", bundle);
    let text = world.store(Utf8Text::ID, b"hello");

    let (caller, commands) = fetch(&mut world, text);
    assert!(world.drive(commands).is_empty());
    assert!(world.events_seen.is_empty(), "the startup read is still unfed");
    assert_eq!(world.fetched, [(caller, found(text, b"hello"))]);

    assert!(world.drive(startup).is_empty());
    assert!(world.abort.is_none());
}
