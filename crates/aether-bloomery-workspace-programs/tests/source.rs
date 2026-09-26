//! `source.select` driven through the guest invocation seam over a small synthetic image tree.
//!
//! The image mirrors an imported source image: the checkout under `source`, beside the placeholders Docker adds to
//! every container. The closure carries the input and every `Tree` member, and no file blob.

use std::collections::BTreeMap;
use std::error::Error;

use aether_bloomery_kinds::{
    ClosureArtifact, Digest, EncodedArtifact, Invoke, Invoked, Name, Node, OpaqueBytes, ProgramName, Ref, Refusal, Tree,
};
use aether_bloomery_program::{Program, invoke};
use aether_bloomery_workspace_programs::source::{SelectInput, SourceSelect};
use aether_data::{Cites, Storage};

/// The members one invocation carries, filled as the trees are built.
#[derive(Default)]
struct Closure(Vec<ClosureArtifact>);

impl Closure {
    /// Encode `value`, carry it, and cite it.
    fn carry<K: Storage + Clone + Cites>(&mut self, value: &K) -> Result<Ref<K>, Box<dyn Error>> {
        let encoded = EncodedArtifact::new(value)?;
        self.0.push(ClosureArtifact::new(encoded.kind(), encoded.bytes().to_vec()));
        Ok(Ref::from_digest(encoded.digest()))
    }

    /// A carried directory of `entries`.
    fn dir<'a>(&mut self, entries: impl IntoIterator<Item = (&'a str, Node)>) -> Result<Ref<Tree>, Box<dyn Error>> {
        let entries = entries.into_iter().map(|(name, node)| Ok((Name::new(name)?, node)));
        self.carry(&Tree::new(entries.collect::<Result<BTreeMap<_, _>, Box<dyn Error>>>()?))
    }
}

/// A file whose blob is never carried.
fn file(content: &[u8]) -> Node {
    Node::File(Ref::<OpaqueBytes>::of_bytes(content))
}

/// What the image root binds `source` to.
#[derive(Clone, Copy)]
enum Source {
    /// The checkout, as the recipe copies it.
    Directory,
    /// A file where the checkout belongs.
    File,
    /// Nothing.
    Absent,
}

/// The selection's answer over an image whose root binds `source` as `shape` says, and the checkout directory the
/// image cites, when it cites one.
fn select(shape: Source) -> Result<(Invoked, Option<Digest>), Box<dyn Error>> {
    let mut closure = Closure::default();

    let empty = closure.dir([])?;
    let dev = closure.dir([("console", file(b"")), ("pts", Node::Directory(empty))])?;
    let etc = closure.dir([("hostname", file(b""))])?;
    let mut root = vec![(".dockerenv", file(b"")), ("dev", Node::Directory(dev)), ("etc", Node::Directory(etc))];
    let cited = match shape {
        Source::Directory => {
            let crates = closure.dir([("lib.rs", file(b"pub fn lib() {}"))])?;
            let source = closure.dir([("Cargo.toml", file(b"[workspace]")), ("crates", Node::Directory(crates))])?;
            root.push(("source", Node::Directory(source)));
            Some(source.digest())
        }
        Source::File => {
            root.push(("source", file(b"not a directory")));
            None
        }
        Source::Absent => None,
    };
    let image = closure.dir(root)?;
    let input = closure.carry(&SelectInput { image })?.digest();

    let name = ProgramName::new(SourceSelect::NAME)?;
    Ok((invoke::<SourceSelect>(Invoke::new(7, name, input, closure.0)), cited))
}

#[test]
fn the_selection_answers_the_cited_source_subtree() -> Result<(), Box<dyn Error>> {
    // Catches selecting the wrong entry, the placeholders entering the result, and a result rebuilt rather than the
    // cited subtree: the one staged artifact is the result, and its digest is the image's own `source` entry.
    let (invoked, cited) = select(Source::Directory)?;
    let cited = cited.ok_or("the image cites a source directory")?;
    let Invoked::Completed { seq: 7, result, staged } = invoked else {
        panic!("expected the selection to complete, got {invoked:?}");
    };
    assert_eq!(result, cited);
    assert_eq!(staged.iter().map(EncodedArtifact::digest).collect::<Vec<_>>(), [cited]);
    Ok(())
}

#[test]
fn an_image_without_a_source_directory_refuses_naming_it() -> Result<(), Box<dyn Error>> {
    // Catches a selection that answers the image root, or a file's blob, when the recipe's `source` directory is
    // missing.
    let cases =
        [(Source::Absent, "image tree: source is absent"), (Source::File, "image tree: source is not a directory")];
    for (shape, reason) in cases {
        match select(shape)?.0 {
            Invoked::Refused { seq: 7, refusal: Refusal::Refused { reason: refused } } => {
                assert_eq!(refused.as_str(), reason);
            }
            other => panic!("expected a refusal naming {reason:?}, got {other:?}"),
        }
    }
    Ok(())
}
