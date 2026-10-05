//! Test helpers: one program run over a value and the closure it cites, and a
//! small tree for the tools to work on.

use std::collections::BTreeMap;

use aether_bloomery_kinds::{
    ClosureArtifact, EncodedArtifact, EntropyDraw, EntropyResult, Invoke, Invoked, Name, Node, Path, ProgramApi,
    ProgramName, ReadArtifactResult, Refusal, Tree,
};
use aether_bloomery_program::{
    AsyncProgram, ErasedTooled, NoBound, NoDetail, Pending, PollResult, Started, SyncProgram, invoke, start_async,
    tooled,
};
use aether_bloomery_workspace::TreePath;
use aether_bloomery_workspace_programs::proof::{ProofBound, TestEnv};
use aether_codec::encode_storage_schema;
use aether_data::{Cites, Digest, ErasedRef, Kind, Ref, Schema, Storage};
use serde_json::Value;

use crate::input::{Endpoint, InputLimit, ModelName, OfferedTools, OutputBudget, ReasoningEffort};
use crate::session::state::TurnSettings;

/// `value` as the closure member that stores it.
pub fn stored<K: Storage + Clone + Cites>(value: &K) -> ClosureArtifact {
    let (kind, payload, _) = EncodedArtifact::new(value).expect("a test value encodes").into_parts();
    ClosureArtifact::new(kind, payload)
}

/// Settings posting to a test endpoint and offering `tools`.
pub fn settings(tools: OfferedTools) -> TurnSettings {
    TurnSettings::new(
        Endpoint::new("https://example.test/v1/responses").expect("endpoint"),
        ModelName::new("muse-spark-1.3").expect("model"),
        tools,
        OutputBudget::new(64).expect("budget"),
        ReasoningEffort::Low,
        InputLimit::new(u64::MAX).expect("limit"),
    )
}

/// Run `P` over `input` with `closure` injected beside it, as the driver
/// invokes it, and decode the result it stages.
pub fn run<P: SyncProgram>(input: &P::Input, closure: Vec<ClosureArtifact>) -> Result<P::Result, Refusal> {
    run_with::<P>(input, closure).map(|(result, _)| result)
}

fn run_with<P: SyncProgram>(
    input: &P::Input,
    mut closure: Vec<ClosureArtifact>,
) -> Result<(P::Result, Store), Refusal> {
    let input = stored(input);
    let digest = input.claimed().unverified();
    closure.push(input);
    let name = ProgramName::new(P::NAME).expect("program name");
    finish::<P::Result>(invoke::<P>(Invoke::new(1, name, digest, closure)), Store::default())
}

/// Every artifact a test run could read or staged.
#[derive(Default)]
pub struct Store(BTreeMap<Digest, ClosureArtifact>);

impl Store {
    fn insert(&mut self, artifact: ClosureArtifact) {
        self.0.insert(artifact.claimed().unverified(), artifact);
    }

    /// The stored value `cited` names.
    pub fn value<K: Storage>(&self, cited: Ref<K>) -> K {
        let artifact = self.0.get(&cited.digest()).expect("the artifact is stored");
        let payload = artifact.load(cited.digest()).expect("stored bytes hash to their digest");
        K::decode_storage(&payload).expect("the stored value decodes").value
    }
}

/// Run async `P` over `input` with only the input injected, answering every
/// read it fetches from `closure`, and decode the result it stages beside
/// every artifact it could read or staged.
pub fn run_async<P: AsyncProgram>(
    input: &impl StoredInput,
    closure: Vec<ClosureArtifact>,
) -> Result<(P::Result, Store), Refusal> {
    let input = input.stored();
    let digest = input.claimed().unverified();
    let store = Store(closure.into_iter().map(|artifact| (artifact.claimed().unverified(), artifact)).collect());
    let name = ProgramName::new(P::NAME).expect("program name");
    let (mut session, mut waiting) = match start_async::<P>(Invoke::new(1, name, digest, vec![input])) {
        Started::Finished(invoked) => return finish::<P::Result>(invoked, store),
        Started::Live { session, waiting } => (session, waiting),
    };
    loop {
        let Some(Pending::Artifact(pending)) = waiting else {
            panic!("expected a pure program to wait only on reads, got {waiting:?}");
        };
        let reply = store.0.get(&pending.digest).map_or_else(
            || ReadArtifactResult::Missing { digest: pending.digest },
            |artifact| ReadArtifactResult::Found { artifact: artifact.clone() },
        );
        session.fulfill(pending, reply);
        waiting = match session.poll() {
            PollResult::Finished(invoked) => return finish::<P::Result>(invoked, store),
            PollResult::NeedArtifact(pending) => Some(Pending::Artifact(pending)),
            other => panic!("expected a pure program to finish or read, got {other:?}"),
        };
    }
}

/// Run async `P` over `input` with `closure` injected beside it, as the driver
/// invokes it, answering every `Entropy` draw with `entropy` and every read it
/// fetches from `closure`, and decode the result it stages.
pub fn run_drawn<P: AsyncProgram>(
    input: &P::Input,
    closure: Vec<ClosureArtifact>,
    entropy: &[u8],
) -> Result<(P::Result, Store), Refusal> {
    let input = stored(input);
    let digest = input.claimed().unverified();
    let mut injected = closure.clone();
    injected.push(input);
    let store = Store(closure.into_iter().map(|artifact| (artifact.claimed().unverified(), artifact)).collect());
    let name = ProgramName::new(P::NAME).expect("program name");
    let (mut session, mut waiting) = match start_async::<P>(Invoke::new(1, name, digest, injected)) {
        Started::Finished(invoked) => return finish::<P::Result>(invoked, store),
        Started::Live { session, waiting } => (session, waiting),
    };
    loop {
        let Some(pending) = waiting else {
            panic!("expected a sampled program to wait, got no pending");
        };
        match pending {
            Pending::Artifact(artifact) => {
                let reply = store.0.get(&artifact.digest).map_or_else(
                    || ReadArtifactResult::Missing { digest: artifact.digest },
                    |found| ReadArtifactResult::Found { artifact: found.clone() },
                );
                session.fulfill(artifact, reply);
            }
            Pending::Send(call) => {
                assert_eq!(call.api, ProgramApi::Entropy, "a sampled open draws only entropy");
                assert_eq!(call.kind_id, EntropyDraw::ID, "an entropy draw");
                assert_eq!(call.expected_reply, EntropyResult::ID, "answered as entropy");
                let reply = EntropyResult::Drawn { bytes: entropy.to_vec() };
                session.fulfill_send(&call, EntropyResult::ID, reply.encode_into_bytes());
            }
        }
        waiting = match session.poll() {
            PollResult::Finished(invoked) => return finish::<P::Result>(invoked, store),
            PollResult::NeedArtifact(pending) => Some(Pending::Artifact(pending)),
            PollResult::NeedSend(pending) => Some(Pending::Send(pending)),
            other @ PollResult::Waiting => panic!("expected a sampled program to finish, read, or draw, got {other:?}"),
        };
    }
}

/// A value a program runs over, as the closure member that stores it.
pub trait StoredInput {
    /// The closure member.
    fn stored(&self) -> ClosureArtifact;
}

impl<K: Storage + Clone + Cites> StoredInput for K {
    fn stored(&self) -> ClosureArtifact {
        stored(self)
    }
}

/// The decoded result of a finished run, with everything it staged added to
/// `store`.
fn finish<R: Storage>(invoked: Invoked, mut store: Store) -> Result<(R, Store), Refusal> {
    match invoked {
        Invoked::Completed { result, staged, .. } => {
            for artifact in staged {
                let (kind, payload, _) = artifact.into_parts();
                store.insert(ClosureArtifact::new(kind, payload));
            }
            Ok((store.value(Ref::from_digest(result)), store))
        }
        Invoked::Refused { refusal, .. } => Err(refusal),
        other => panic!("expected a program to complete or refuse, got {other:?}"),
    }
}

/// The detail every tree tool's `Edited` cites.
pub fn no_detail() -> Ref<NoDetail> {
    Ref::of_encoded(&NoDetail).expect("the detail encodes")
}

/// `name` as a tree entry name.
pub fn name(name: &str) -> Name {
    Name::new(name).expect("name")
}

/// `path` as a tree path.
pub fn path(path: &str) -> TreePath {
    TreePath::new(path).expect("tree path")
}

/// A small tree: `README`, the executable `run`, the symlink `link` to
/// `README`, the non-UTF-8 file `blob.bin`, and the directory `src` holding
/// `lib.rs`.
pub struct SmallTree {
    root: Tree,
    artifacts: Vec<ClosureArtifact>,
}

impl SmallTree {
    /// The text of `src/lib.rs`.
    pub const LIB: &[u8] = b"pub fn smelt() {}\n";

    /// Stage the tree.
    pub fn new() -> Self {
        let blobs: [&[u8]; 4] = [b"# Bloomery\n", b"#!/bin/sh\nsmelt\n", &[0xff, 0xfe], Self::LIB];
        let mut artifacts: Vec<_> = blobs.iter().map(|blob| stored_bytes(blob)).collect();
        let src = Tree::new(BTreeMap::from([(name("lib.rs"), Node::File(Ref::of_bytes(Self::LIB)))]));
        artifacts.push(stored(&src));
        let root = Tree::new(BTreeMap::from([
            (name("README"), Node::File(Ref::of_bytes(blobs[0]))),
            (name("run"), Node::Executable(Ref::of_bytes(blobs[1]))),
            (name("link"), Node::Symlink(Path::new("README").expect("symlink target"))),
            (name("blob.bin"), Node::File(Ref::of_bytes(blobs[2]))),
            (name("src"), Node::Directory(Ref::of_encoded(&src).expect("src"))),
        ]));
        artifacts.push(stored(&root));
        Self { root, artifacts }
    }

    /// The root directory.
    pub fn root(&self) -> &Tree {
        &self.root
    }

    /// The citation of the root directory.
    pub fn tree(&self) -> Ref<Tree> {
        Ref::of_encoded(&self.root).expect("root")
    }

    /// A call over this tree with `args`, as the loop builds it, and the
    /// artifacts it could read.
    pub fn call<A: Storage + Clone + Cites>(&self, args: &A) -> (ErasedTooled, Vec<ClosureArtifact>) {
        self.called(stored(args))
    }

    /// A call over this tree with the arguments `json` encodes as `A` by
    /// schema alone, as `muse.turn` decodes a model's arguments: no
    /// `#[storage(validate)]` rule is checked.
    pub fn call_json<A: Storage + Schema>(&self, json: &Value) -> (ErasedTooled, Vec<ClosureArtifact>) {
        let payload = encode_storage_schema(json, &A::SCHEMA).expect("the arguments match the schema");
        self.called(ClosureArtifact::new(A::ID, payload))
    }

    /// A call over an empty session tree with `args`, bound to a `ProofBound`
    /// whose vendor tree is this one, as a vendor tool's call is built, and
    /// the artifacts it could read.
    pub fn vendor_call<A: Storage + Clone + Cites>(&self, args: &A) -> (ErasedTooled, Vec<ClosureArtifact>) {
        let environment = Ref::from_digest(Digest::from_bytes([2; 32]));
        let proofs = ProofBound::new(environment, self.tree(), TestEnv::default());
        let session = Ref::of_encoded(&Tree::empty()).expect("the empty tree");
        self.called_over(session, stored(args), stored(&proofs))
    }

    /// A call over this tree with the stored `args`, binding `NoBound` as
    /// every session tool does, and the artifacts it could read.
    fn called(&self, args: ClosureArtifact) -> (ErasedTooled, Vec<ClosureArtifact>) {
        self.called_over(self.tree(), args, stored(&NoBound))
    }

    /// A call over the session tree `session` with the stored `args` and
    /// `bound`, and the artifacts it could read.
    fn called_over(
        &self,
        session: Ref<Tree>,
        args: ClosureArtifact,
        bound: ClosureArtifact,
    ) -> (ErasedTooled, Vec<ClosureArtifact>) {
        let cited = |artifact: &ClosureArtifact| ErasedRef::new(artifact.kind(), artifact.claimed().unverified());
        let input = tooled(session, cited(&args), cited(&bound));
        (input, self.artifacts.iter().cloned().chain([args, bound]).collect())
    }

    /// A diff call over the current tree `current` with `args`, bound to this
    /// tree as the base, and the artifacts it could read with `extra` for
    /// what the current tree adds.
    pub fn diff_call<A: Storage + Clone + Cites>(
        &self,
        current: Ref<Tree>,
        args: &A,
        extra: Vec<ClosureArtifact>,
    ) -> (ErasedTooled, Vec<ClosureArtifact>) {
        let (input, mut closure) = self.called_over(current, stored(args), stored(&self.root));
        closure.extend(extra);
        (input, closure)
    }

    /// The root of this tree with `change` applied to its entries, and the
    /// artifact staging it.
    pub fn changed(&self, change: impl FnOnce(&mut BTreeMap<Name, Node>)) -> (Ref<Tree>, ClosureArtifact) {
        let mut entries = self.root.entries().clone();
        change(&mut entries);
        let root = Tree::new(entries);
        let cited = Ref::of_encoded(&root).expect("a changed root encodes");
        (cited, stored(&root))
    }
}

pub fn stored_bytes(bytes: &[u8]) -> ClosureArtifact {
    let (kind, payload, _) = EncodedArtifact::opaque_bytes(bytes).into_parts();
    ClosureArtifact::new(kind, payload)
}
