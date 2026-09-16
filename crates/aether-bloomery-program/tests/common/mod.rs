//! Shared clock, trim program, and in-process executors for program tests.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::str;

use aether_bloomery_journal::Clock;
use aether_bloomery_kinds::{Mode, Node, OpaqueBytes, Ref, Tree};
use aether_bloomery_program::{Execute, Execution, Program, ReadArtifacts, Refusal, Staging};

pub struct FixedClock(pub u64);

impl Clock for FixedClock {
    fn now_millis(&self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.trim_input")]
pub struct TrimInput {
    pub tree: Ref<Tree>,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.trim_result")]
pub struct TrimResult {
    pub tree: Ref<Tree>,
    pub changed: u32,
}

pub struct Trim;

impl Program for Trim {
    const NAME: &'static str = "trim";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Strip trailing whitespace from every File.";
    type Input = TrimInput;
    type Result = TrimResult;
}

pub struct TrimExecutor;

impl Execute<Trim> for TrimExecutor {
    fn execute(&self, input: TrimInput, store: &dyn ReadArtifacts) -> Result<Execution<Trim>, Refusal> {
        let mut staging = Staging::new();
        let (tree, changed) = trim_tree(input.tree, store, &mut staging, 0)?;
        staging.finish(TrimResult { tree, changed }).map_err(|error| Refusal::Refused(error.to_string()))
    }
}

pub struct RefuseExecutor;

impl Execute<Trim> for RefuseExecutor {
    fn execute(&self, _input: TrimInput, _store: &dyn ReadArtifacts) -> Result<Execution<Trim>, Refusal> {
        Err(Refusal::Refused("no".into()))
    }
}

pub struct PanicExecutor;

impl Execute<Trim> for PanicExecutor {
    fn execute(&self, _input: TrimInput, _store: &dyn ReadArtifacts) -> Result<Execution<Trim>, Refusal> {
        panic!("trim panicked");
    }
}

fn trim_tree(
    tree_ref: Ref<Tree>,
    store: &dyn ReadArtifacts,
    staging: &mut Staging<Trim>,
    depth: u32,
) -> Result<(Ref<Tree>, u32), Refusal> {
    if depth > 32 {
        return Err(Refusal::Refused("tree too deep".into()));
    }
    let tree = store.get::<Tree>(&tree_ref.digest()).map_err(|_| Refusal::InputDecode)?.ok_or(Refusal::InputMissing)?;
    let mut entries = BTreeMap::new();
    let mut changed = 0;
    let mut rebuilt = false;
    for (name, node) in tree.entries() {
        let next = match node {
            Node::File(file) => {
                let payload = file_bytes(store, file)?;
                let trimmed = trim_bytes(&payload);
                if trimmed.as_slice() == payload.as_slice() {
                    Node::File(*file)
                } else {
                    changed += 1;
                    rebuilt = true;
                    Node::File(staging.stage_bytes(&trimmed))
                }
            }
            Node::Directory(child) => {
                let (new_child, child_changed) = trim_tree(*child, store, staging, depth + 1)?;
                changed += child_changed;
                if new_child.digest() == child.digest() {
                    Node::Directory(*child)
                } else {
                    rebuilt = true;
                    Node::Directory(new_child)
                }
            }
            other => other.clone(),
        };
        entries.insert(name.clone(), next);
    }
    if rebuilt {
        let staged = staging
            .stage_encoded(&Tree::new(entries).map_err(|error| Refusal::Refused(error.to_string()))?)
            .map_err(|error| Refusal::Refused(error.to_string()))?;
        Ok((staged, changed))
    } else {
        Ok((tree_ref, changed))
    }
}

fn file_bytes(store: &dyn ReadArtifacts, file: &Ref<OpaqueBytes>) -> Result<Vec<u8>, Refusal> {
    match store.get_bytes(&file.digest()).map_err(|_| Refusal::InputDecode)? {
        Some((_, payload)) => Ok(payload),
        None => Err(Refusal::InputMissing),
    }
}

fn trim_bytes(bytes: &[u8]) -> Vec<u8> {
    str::from_utf8(bytes).map_or_else(
        |_| {
            let end = bytes.iter().rposition(|byte| !byte.is_ascii_whitespace()).map_or(0, |index| index + 1);
            bytes[..end].to_vec()
        },
        |text| text.trim_end().as_bytes().to_vec(),
    )
}
