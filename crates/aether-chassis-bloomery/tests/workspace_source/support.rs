//! What the import and run scenarios share: a boot over a seed, a stub serving one script while the scenario drives,
//! and the stored values a scenario reads back.

use std::collections::BTreeMap;
use std::error::Error;
use std::io;
use std::thread;

use aether_bloomery_journal::{Batch, JournalReader};
use aether_bloomery_kinds::{Digest, Name, Node, Ref, Tree};
use aether_bloomery_workspace::testing::{StubDaemon, StubReply, StubRequest};
use aether_chassis_bloomery::BloomeryCli;
use aether_data::Storage;
use aether_harness_bloomery::{BloomeryHarness, SeededJournal};
use clap::Parser;

pub type TestResult = Result<(), Box<dyn Error>>;

/// The image every import scenario names.
pub const IMAGE: &str = "debian@sha256:1111111111111111111111111111111111111111111111111111111111111111";

/// The container id every import scenario's create answers.
pub const CONTAINER: &str = "c0ffee";

/// Boot the bloomery over `batches` with the workspace dialing `endpoint`, plus the workspace `flags`.
pub fn boot(batches: Vec<Batch>, endpoint: &str, flags: &[&str]) -> Result<BloomeryHarness, Box<dyn Error>> {
    let argv = ["aether-bloomery", "--workspace-endpoint", endpoint].into_iter().chain(flags.iter().copied());
    Ok(SeededJournal::new(batches).boot_with_argv(BloomeryCli::try_parse_from(argv)?))
}

/// [`boot`] with the workspace dialing a daemon that is gone: its socket and directory are removed before boot, so
/// any request that reaches the daemon fails.
pub fn boot_without_daemon(batches: Vec<Batch>, flags: &[&str]) -> Result<BloomeryHarness, Box<dyn Error>> {
    boot(batches, &StubDaemon::bind()?.endpoint(), flags)
}

/// Run `drive` while `stub` serves `replies`, then drop its listener, so a request the script did not expect fails
/// fast; answer what `drive` returned beside the requests the stub read.
pub fn serving<T>(
    stub: StubDaemon,
    replies: Vec<StubReply>,
    drive: impl FnOnce() -> T,
) -> Result<(T, Vec<StubRequest>), Box<dyn Error>> {
    beside(move || stub.serve(replies), drive)
}

/// [`serving`], keeping `stub`'s listener bound for a later script.
pub fn answering<T>(
    stub: &StubDaemon,
    replies: Vec<StubReply>,
    drive: impl FnOnce() -> T,
) -> Result<(T, Vec<StubRequest>), Box<dyn Error>> {
    beside(move || stub.answer(replies), drive)
}

/// Run `drive` while a scoped thread runs `serve`.
fn beside<T>(
    serve: impl FnOnce() -> io::Result<Vec<StubRequest>> + Send,
    drive: impl FnOnce() -> T,
) -> Result<(T, Vec<StubRequest>), Box<dyn Error>> {
    thread::scope(|scope| -> Result<_, Box<dyn Error>> {
        let served = scope.spawn(serve);
        let driven = drive();
        let requests = served.join().map_err(|_| "the stub daemon thread panicked")??;
        Ok((driven, requests))
    })
}

/// `"<METHOD> <target>"` of each request, in order.
pub fn lines(requests: &[StubRequest]) -> Vec<String> {
    requests.iter().map(StubRequest::line).collect()
}

/// A directory of `entries`.
pub fn tree_of(entries: Vec<(&str, Node)>) -> Result<Tree, Box<dyn Error>> {
    let mut map = BTreeMap::new();
    for (name, node) in entries {
        map.insert(Name::new(name)?, node);
    }
    Ok(Tree::new(map))
}

/// A directory of `entries`, staged in `batch`.
pub fn directory(batch: &mut Batch, entries: Vec<(&str, Node)>) -> Result<Ref<Tree>, Box<dyn Error>> {
    Ok(batch.stage_encoded(&tree_of(entries)?)?)
}

/// The value the unit's journal stores under `digest`, read on a connection of the scenario's own.
pub fn stored<K: Storage>(harness: &BloomeryHarness, digest: Digest) -> Result<K, Box<dyn Error>> {
    Ok(JournalReader::open(harness.journal_path())?.get::<K>(&digest)?.ok_or_else(|| format!("{digest} is stored"))?)
}

/// The entry `name` of `tree`.
pub fn child(tree: &Tree, name: &str) -> Result<Node, Box<dyn Error>> {
    Ok(tree.entries().get(&Name::new(name)?).ok_or_else(|| format!("no entry {name}"))?.clone())
}

/// Over 1 MiB, so the blob spans many copy buffers and several of a source's read windows.
pub fn large_payload() -> Vec<u8> {
    (0..=250u8).cycle().take(1_536 * 1024).collect()
}
