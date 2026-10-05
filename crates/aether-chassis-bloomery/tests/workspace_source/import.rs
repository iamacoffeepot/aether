//! `Import` over the unit's journal: the Engine API sequence, the decode rules, and the tree it stages.

use std::error::Error;
use std::thread;

use aether_actor::{ActorPath, PathRefusal, PathRefused};
use aether_bloomery_journal::{JournalActor, JournalReader};
use aether_bloomery_kinds::{Node, Path, Tree, UnitKey};
use aether_bloomery_workspace::testing::{StubDaemon, StubReply, StubRequest, TarWriter};
use aether_bloomery_workspace::{ImageRef, Import, ImportError, ImportResult};
use aether_data::Ref;
use aether_harness_bloomery::BloomeryHarness;

use crate::support::{CONTAINER, IMAGE, TestResult, boot, child, large_payload, lines, serving, stored, tree_of};

/// The payload bytes one `Stage` gathers before the workspace sends it; a file past it is staged alone.
const STAGE_MAX_BYTES: usize = 64 << 20;

/// The artifacts one `Stage` gathers before the workspace sends it.
const STAGE_MAX_ARTIFACTS: usize = 4_096;

/// An import of [`IMAGE`] staged into the harness's unit journal.
fn import(harness: &BloomeryHarness) -> Result<Import, Box<dyn Error>> {
    Ok(Import { image: ImageRef::new(IMAGE)?, source: harness.source() })
}

/// Boot over an empty journal, import [`IMAGE`] while a fresh stub serves `replies`, and answer the result, the
/// requests the stub read, and the harness to read the journal back through.
fn import_against(
    replies: Vec<StubReply>,
    flags: &[&str],
) -> Result<(ImportResult, Vec<StubRequest>, BloomeryHarness), Box<dyn Error>> {
    let stub = StubDaemon::bind()?;
    let mut harness = boot(Vec::new(), &stub.endpoint(), flags)?;
    let request = import(&harness)?;
    let (answer, requests) = serving(stub, replies, || harness.import(&request))?;
    Ok((answer, requests, harness))
}

/// An export as a daemon writes one: a `dev/` holding a device node, and an absolute symlink two directories down.
fn userland_export() -> Vec<u8> {
    TarWriter::new()
        .directory("dev/")
        .char_device("dev/null")
        .directory("etc/")
        .directory("etc/alternatives/")
        .file("etc/alternatives/cc", b"#!cc\n")
        .directory("usr/")
        .directory("usr/bin/")
        .symlink("usr/bin/cc", "/etc/alternatives/cc")
        .finish()
}

fn tree_of_answer(answer: ImportResult) -> Result<Ref<Tree>, Box<dyn Error>> {
    match answer {
        ImportResult::Ok { tree } => Ok(tree),
        ImportResult::Err(error) => Err(format!("the import failed: {error:?}").into()),
    }
}

fn failed(answer: &ImportResult) -> Result<&str, Box<dyn Error>> {
    match answer {
        ImportResult::Err(ImportError::Failed { detail }) => Ok(detail.as_str()),
        other => Err(format!("expected Failed, got {other:?}").into()),
    }
}

#[test]
fn an_import_answers_ok_and_holds_settlement_until_it_is_done() -> TestResult {
    // Catches the import run on the dispatcher or detached from the caller's chain: settlement must not come back
    // while the worker is still talking to the daemon, and the reply must arrive through the task completion. Also
    // catches the userland rules not chosen at `init`: the canonical rules refuse the export's absolute symlink.
    let stub = StubDaemon::bind()?;
    let mut harness = boot(Vec::new(), &stub.endpoint(), &[])?;
    let request = import(&harness)?;
    let export = TarWriter::new()
        .directory("etc/")
        .file("etc/hostname", b"ws\n")
        .symlink("etc/localtime", "/usr/share/zoneinfo/UTC")
        .finish();
    let script = StubReply::import_script(IMAGE, CONTAINER, &export);
    let expected = script.len();

    let (answer, read) = thread::scope(|scope| -> Result<_, Box<dyn Error>> {
        let served = scope.spawn(|| stub.answer(script));
        let pending = harness.settle_import(&request);
        let read = stub.requests_read();
        let answer = harness.wait(pending);
        served.join().map_err(|_| "the stub daemon thread panicked")??;
        Ok((answer, read))
    })?;

    assert_eq!(read, expected, "the chain settled before the import's last request was served");
    tree_of_answer(answer)?;
    Ok(())
}

#[test]
fn an_import_pulls_checks_creates_exports_then_removes_in_that_order() -> TestResult {
    // Catches a reordered or skipped step: an export before the create, a create before the digest check, a missing
    // removal, or an unversioned or wrongly spliced request path.
    let (answer, requests, _harness) =
        import_against(StubReply::import_script(IMAGE, CONTAINER, &userland_export()), &[])?;

    tree_of_answer(answer)?;
    assert_eq!(
        lines(&requests),
        [
            format!("POST /v1.44/images/create?fromImage={IMAGE}"),
            format!("GET /v1.44/images/{IMAGE}/json"),
            "POST /v1.44/containers/create".to_owned(),
            format!("GET /v1.44/containers/{CONTAINER}/export"),
            format!("DELETE /v1.44/containers/{CONTAINER}?force=true&v=true"),
        ]
    );
    Ok(())
}

#[test]
fn a_pull_error_or_an_unlisted_digest_fails_before_any_container_exists() -> TestResult {
    // Catches a container created for an image the pull did not deliver, or for one the daemon resolved to a
    // different digest than the ref pins.
    let pull_error = StubReply::chunked(200, vec![br#"{"error":"manifest unknown"}"#.to_vec()]);
    let other_digest = StubReply::with_length(200, r#"{"RepoDigests":["debian@sha256:2222"]}"#);

    let (pulled, pull_requests, _) = import_against(vec![pull_error], &[])?;
    let (listed, list_requests, _) = import_against(vec![StubReply::chunked(200, Vec::new()), other_digest], &[])?;

    failed(&pulled)?;
    failed(&listed)?;
    assert_eq!(pull_requests.len(), 1, "only the pull: {:?}", lines(&pull_requests));
    assert_eq!(list_requests.len(), 2, "only the pull and the inspect: {:?}", lines(&list_requests));
    Ok(())
}

#[test]
fn an_export_decodes_under_the_userland_rules() -> TestResult {
    // Catches the import decoding under canonical rules (both entries refused) or a rewrite that does not resolve
    // the same inside the tree: from `usr/bin`, `/etc/alternatives/cc` is two levels up, then down.
    let (answer, _, harness) = import_against(StubReply::import_script(IMAGE, CONTAINER, &userland_export()), &[])?;

    let top: Tree = stored(&harness, tree_of_answer(answer)?.digest())?;
    let Node::Directory(usr) = child(&top, "usr")? else {
        panic!("usr is a directory")
    };
    let Node::Directory(bin) = child(&stored(&harness, usr.digest())?, "bin")? else {
        panic!("usr/bin is a directory")
    };
    assert_eq!(child(&stored(&harness, bin.digest())?, "cc")?, Node::Symlink(Path::new("../../etc/alternatives/cc")?));
    let Node::Directory(dev) = child(&top, "dev")? else {
        panic!("dev is a directory")
    };
    assert!(stored::<Tree>(&harness, dev.digest())?.entries().is_empty(), "the device node under dev/ is dropped");
    Ok(())
}

#[test]
fn an_export_stages_a_tree_named_by_what_its_content_hashes_to() -> TestResult {
    // Catches a sink whose references drift from the content it stages: a blob hashed with the wrong kind prefix, a
    // chunk dropped or repeated across the copy buffer's boundary, or a tree staged under a digest other than its
    // encoding's. The expected tree is built from the content here, never by the sink.
    let large = large_payload();
    let export = TarWriter::new().directory("etc/").file("etc/hostname", b"ws\n").file("large.bin", &large).finish();

    let (answer, _, harness) = import_against(StubReply::import_script(IMAGE, CONTAINER, &export), &[])?;

    let etc = tree_of(vec![("hostname", Node::File(Ref::of_bytes(b"ws\n")))])?;
    let expected = tree_of(vec![
        ("etc", Node::Directory(Ref::of_encoded(&etc)?)),
        ("large.bin", Node::File(Ref::of_bytes(&large))),
    ])?;
    let tree = tree_of_answer(answer)?;
    assert_eq!(tree, Ref::of_encoded(&expected)?);
    assert_eq!(stored::<Tree>(&harness, tree.digest())?, expected, "the staged root decodes to the tree");
    assert!(harness.stores(&Ref::of_bytes(&large).digest()), "the large blob is staged");
    Ok(())
}

#[test]
fn a_second_import_of_the_same_export_names_the_same_tree() -> TestResult {
    // Catches an import that is not a function of its content: a timestamp or a container id reaching the tree
    // would change its digest.
    let stub = StubDaemon::bind()?;
    let mut harness = boot(Vec::new(), &stub.endpoint(), &[])?;
    let request = import(&harness)?;
    let script = || StubReply::import_script(IMAGE, CONTAINER, &userland_export());
    let both = script().into_iter().chain(script()).collect();

    let (answers, _) = serving(stub, both, || [harness.import(&request), harness.import(&request)])?;

    let [first, second] = answers;
    assert_eq!(tree_of_answer(first)?, tree_of_answer(second)?);
    Ok(())
}

#[test]
fn an_export_over_the_entry_limit_fails_and_still_removes_the_container() -> TestResult {
    // Catches the removal skipped on a failed decode (a container left behind per refused import) and a refused
    // decode answered as a tree.
    let (answer, requests, _) = import_against(
        StubReply::import_script(IMAGE, CONTAINER, &userland_export()),
        &["--workspace-import-max-entries", "3"],
    )?;

    let detail = failed(&answer)?;
    assert!(detail.contains("entry limit"), "{detail}");
    assert_eq!(
        requests.last().map(StubRequest::line),
        Some(format!("DELETE /v1.44/containers/{CONTAINER}?force=true&v=true"))
    );
    Ok(())
}

#[test]
fn a_stream_cut_inside_a_large_blob_fails_and_still_removes_the_container() -> TestResult {
    // Catches a partial import answered as a tree: a sink that finishes the blob it was streaming when the input ran
    // dry would name a tree nobody exported.
    let export = TarWriter::new().file("small", b"small").file("large.bin", &large_payload()).cut(700 * 1024);

    let (answer, requests, _) = import_against(StubReply::import_script(IMAGE, CONTAINER, &export), &[])?;

    let detail = failed(&answer)?;
    assert!(detail.contains("ended before the end-of-archive block"), "{detail}");
    assert_eq!(requests.len(), 5, "the container was removed: {:?}", lines(&requests));
    Ok(())
}

/// Import `export` over an empty journal, then walk the tree it answered and count its files, asserting each cited
/// blob is stored with the bytes its reference names.
fn stored_blobs_of(export: &[u8]) -> Result<usize, Box<dyn Error>> {
    let (answer, _, harness) = import_against(StubReply::import_script(IMAGE, CONTAINER, export), &[])?;

    let reader = JournalReader::open(harness.journal_path())?;
    let mut pending = vec![tree_of_answer(answer)?];
    let mut blobs = 0;
    while let Some(tree) = pending.pop() {
        let tree: Tree = reader.get(&tree.digest())?.ok_or("every tree the root cites is stored")?;
        for node in tree.entries().values() {
            match node {
                Node::Directory(child) => pending.push(*child),
                Node::File(blob) | Node::Executable(blob) => {
                    let (_, bytes) = reader.get_bytes(&blob.digest())?.ok_or("every blob the tree cites is stored")?;
                    assert_eq!(Ref::of_bytes(&bytes), *blob, "the stored blob holds the bytes its reference names");
                    blobs += 1;
                }
                Node::Symlink(_) => {}
            }
        }
    }
    Ok(blobs)
}

#[test]
fn a_file_larger_than_a_stage_batch_is_staged_whole_beside_the_files_around_it() -> TestResult {
    // Catches a batch flush that drops the blob too large for one batch, or the small files queued before or after
    // it, and a tree sent in a stage before the blob it cites (the journal would refuse the dangling citation).
    let large: Vec<u8> = (0..=250u8).cycle().take(STAGE_MAX_BYTES + 1).collect();
    let export =
        TarWriter::new().file("before", b"before\n").file("large.bin", &large).file("zafter", b"after\n").finish();

    assert_eq!(stored_blobs_of(&export)?, 3, "every file of the export is a stored blob");
    Ok(())
}

#[test]
fn an_export_of_more_files_than_one_stage_batch_holds_stages_every_blob_its_tree_cites() -> TestResult {
    // Catches a batch bound on artifacts that loses the artifacts past it, and a directory sent in a stage before
    // the blobs it cites that the next batch carries: every blob the tree cites must be stored with its bytes.
    let small: Vec<(String, Vec<u8>)> = (0..STAGE_MAX_ARTIFACTS + 8)
        .map(|index| (format!("many/{index}"), format!("{index}\n").into_bytes()))
        .collect();
    let export =
        small.iter().fold(TarWriter::new().directory("many/"), |tar, (path, content)| tar.file(path, content)).finish();

    assert_eq!(stored_blobs_of(&export)?, small.len(), "every file of the export is a stored blob");
    Ok(())
}

#[test]
fn a_request_naming_a_unit_with_no_journal_is_answered_and_reaches_no_daemon() -> TestResult {
    // Catches a source that is not proven when the request decodes, and a refusal that goes unanswered: an import
    // naming storage no journal stands at would pull, create, and export into nothing, and one dropped at decode
    // would leave its requester waiting forever. The refused import is answered naming the path before the import
    // after it, and the stub serves that second import's requests alone.
    let stub = StubDaemon::bind()?;
    let mut harness = boot(Vec::new(), &stub.endpoint(), &[])?;
    let elsewhere = ActorPath::<JournalActor>::instance(UnitKey::new("elsewhere")?.as_load_name());
    let unhoused = Import { image: ImageRef::new(IMAGE)?, source: elsewhere.narrow() };
    let request = import(&harness)?;

    let ((refused, answer), requests) =
        serving(stub, StubReply::import_script(IMAGE, CONTAINER, &userland_export()), || {
            let refused = harness.import(&unhoused);
            (refused, harness.import(&request))
        })?;

    let expected = PathRefused { path: unhoused.source.as_erased().clone(), reason: PathRefusal::Unpublished };
    assert_eq!(refused, ImportResult::Err(ImportError::Source(expected)));
    tree_of_answer(answer)?;
    assert_eq!(requests.len(), 5, "one import reached the daemon: {:?}", lines(&requests));
    Ok(())
}
