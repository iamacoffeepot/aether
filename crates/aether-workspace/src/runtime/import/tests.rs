//! The import sequence against the stub daemon and a temp journal root.

use std::error::Error;
use std::path::PathBuf;
use std::thread;

use aether_bloomery_journal::{ArtifactStore, Journal};
use aether_bloomery_kinds::{Name, Node, Path, Ref, Tree};
use aether_bloomery_tar::{Limits, Rules};
use tempfile::TempDir;

use super::Importer;
use crate::runtime::engine::{Endpoint, Engine};
use crate::runtime::testing::{StubDaemon, StubReply, StubRequest, TarWriter, artifact_rows};
use crate::{ImageRef, ImportResult};

type TestResult = Result<(), Box<dyn Error>>;

const IMAGE: &str = "debian@sha256:1111111111111111111111111111111111111111111111111111111111111111";
const CONTAINER: &str = "c0ffee";

/// A temp journal root and its artifact store.
struct Root {
    _temp: TempDir,
    path: PathBuf,
    store: ArtifactStore,
}

impl Root {
    fn open() -> Result<Self, Box<dyn Error>> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("journal");
        let store = Journal::open(&path)?.artifact_store();
        Ok(Self { _temp: temp, path, store })
    }

    /// Run one import of [`IMAGE`] against a stub serving `replies`, under an
    /// entry limit of `max_entries`, and return its answer and the requests
    /// the stub read.
    fn import(
        &self,
        replies: Vec<StubReply>,
        max_entries: u32,
    ) -> Result<(ImportResult, Vec<StubRequest>), Box<dyn Error>> {
        let stub = StubDaemon::bind()?;
        let importer = Importer {
            engine: Engine::new(Endpoint::parse(&stub.endpoint())?),
            artifacts: self.store.clone(),
            rules: Rules::userland(Limits::new(max_entries, 1 << 30)?),
        };
        let image = ImageRef::new(IMAGE)?;
        thread::scope(|scope| {
            let served = scope.spawn(|| stub.serve(replies));
            let answer = importer.answer(&image);
            let requests = served.join().map_err(|_| "the stub thread panicked")??;
            Ok((answer, requests))
        })
    }

    fn tree(&self, tree: &Ref<Tree>) -> Result<Tree, Box<dyn Error>> {
        Ok(self.store.batch()?.get::<Tree>(&tree.digest())?.ok_or("the tree is stored")?)
    }
}

/// An export as a daemon writes one: a `dev/` holding a device node, and an
/// absolute symlink two directories down.
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

fn lines(requests: &[StubRequest]) -> Vec<String> {
    requests.iter().map(StubRequest::line).collect()
}

fn tree_of(answer: ImportResult) -> Result<Ref<Tree>, Box<dyn Error>> {
    match answer {
        ImportResult::Ok { tree } => Ok(tree),
        ImportResult::Failed { detail } => Err(format!("the import failed: {}", detail.as_str()).into()),
    }
}

fn child(tree: &Tree, name: &str) -> Result<Node, Box<dyn Error>> {
    Ok(tree.entries().get(&Name::new(name)?).ok_or_else(|| format!("no entry {name}"))?.clone())
}

#[test]
fn an_import_pulls_checks_creates_exports_then_removes_in_that_order() -> TestResult {
    // Catches a reordered or skipped step: an export before the create, a
    // create before the digest check, a missing removal, or an unversioned or
    // wrongly spliced request path.
    let root = Root::open()?;

    let (answer, requests) = root.import(StubReply::import_script(IMAGE, CONTAINER, &userland_export()), 1_000)?;

    tree_of(answer)?;
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
    // Catches a container created for an image the pull did not deliver, or
    // for one the daemon resolved to a different digest than the ref pins.
    let root = Root::open()?;
    let pull_error = StubReply::chunked(200, vec![br#"{"error":"manifest unknown"}"#.to_vec()]);
    let other_digest = StubReply::with_length(200, r#"{"RepoDigests":["debian@sha256:2222"]}"#);

    let (pulled, pull_requests) = root.import(vec![pull_error], 1_000)?;
    let (listed, list_requests) = root.import(vec![StubReply::chunked(200, Vec::new()), other_digest], 1_000)?;

    assert!(matches!(pulled, ImportResult::Failed { .. }), "{pulled:?}");
    assert!(matches!(listed, ImportResult::Failed { .. }), "{listed:?}");
    assert_eq!(pull_requests.len(), 1, "only the pull: {:?}", lines(&pull_requests));
    assert_eq!(list_requests.len(), 2, "only the pull and the inspect: {:?}", lines(&list_requests));
    assert_eq!(artifact_rows(&root.path)?, 0);
    Ok(())
}

#[test]
fn an_export_decodes_under_the_userland_rules() -> TestResult {
    // Catches the import decoding under canonical rules (both entries refused)
    // or a rewrite that does not resolve the same inside the tree: from
    // `usr/bin`, `/etc/alternatives/cc` is two levels up, then down.
    let root = Root::open()?;

    let (answer, _) = root.import(StubReply::import_script(IMAGE, CONTAINER, &userland_export()), 1_000)?;

    let top = root.tree(&tree_of(answer)?)?;
    let Node::Directory(usr) = child(&top, "usr")? else {
        panic!("usr is a directory")
    };
    let Node::Directory(bin) = child(&root.tree(&usr)?, "bin")? else {
        panic!("usr/bin is a directory")
    };
    assert_eq!(child(&root.tree(&bin)?, "cc")?, Node::Symlink(Path::new("../../etc/alternatives/cc")?));
    let Node::Directory(dev) = child(&top, "dev")? else {
        panic!("dev is a directory")
    };
    assert!(root.tree(&dev)?.entries().is_empty(), "the device node under dev/ is dropped");
    Ok(())
}

#[test]
fn a_second_import_of_the_same_export_adds_no_rows() -> TestResult {
    // Catches an import that is not a function of its content: a timestamp or
    // container id reaching the tree would change its digest, and a row
    // inserted without the stored check would grow the journal on every run.
    let root = Root::open()?;
    let script = || StubReply::import_script(IMAGE, CONTAINER, &userland_export());

    let first = tree_of(root.import(script(), 1_000)?.0)?;
    let rows = artifact_rows(&root.path)?;
    let second = tree_of(root.import(script(), 1_000)?.0)?;

    assert_eq!(first, second);
    assert_eq!(artifact_rows(&root.path)?, rows);
    Ok(())
}

#[test]
fn an_export_over_the_entry_limit_commits_nothing_and_still_removes_the_container() -> TestResult {
    // Catches the removal skipped on a failed decode (a container left behind
    // per refused import) and a batch committed with the part decoded before
    // the refusal.
    let root = Root::open()?;

    let (answer, requests) = root.import(StubReply::import_script(IMAGE, CONTAINER, &userland_export()), 3)?;

    match answer {
        ImportResult::Failed { detail } => assert!(detail.as_str().contains("entry limit"), "{}", detail.as_str()),
        ImportResult::Ok { .. } => panic!("an export of seven entries must not fit a limit of three"),
    }
    assert_eq!(
        requests.last().map(StubRequest::line),
        Some(format!("DELETE /v1.44/containers/{CONTAINER}?force=true&v=true"))
    );
    assert_eq!(artifact_rows(&root.path)?, 0);
    Ok(())
}
