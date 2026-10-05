//! The paired walk `tree.diff` runs over its base and current trees.

use std::collections::BTreeSet;

use aether_bloomery_kinds::{Name, Node, Path, Refusal};
use aether_bloomery_program::{Async, Env};
use aether_data::{OpaqueBytes, Ref};

use super::hunks::FileMode;

/// One path as the base tree (`old`) and the current tree (`new`) hold it.
/// `path` is empty for the root.
pub(super) struct Pair {
    pub(super) path: String,
    pub(super) old: Option<Node>,
    pub(super) new: Option<Node>,
}

/// A changed entry that is not a directory.
pub(super) enum Leaf {
    File { blob: Ref<OpaqueBytes>, mode: FileMode },
    Symlink(Path),
}

/// One changed file, with at least one side `Some`.
pub(super) struct Changed {
    pub(super) path: String,
    pub(super) old: Option<Leaf>,
    pub(super) new: Option<Leaf>,
}

/// What the walk found.
pub(super) struct Walked {
    pub(super) changed: Vec<Changed>,
    pub(super) visited: usize,
    pub(super) stopped: bool,
}

/// The changed files under `start`, walking both trees together with an
/// explicit stack.
///
/// # Errors
///
/// The [`Refusal`] of a directory the store cannot give.
pub(super) async fn changes(env: &mut Env<Async>, start: Pair, max_entries: usize) -> Result<Walked, Refusal> {
    let mut stack = vec![start];
    let mut changed = Vec::new();
    let mut visited = 0;
    let mut stopped = false;
    while let Some(pair) = stack.pop() {
        if visited >= max_entries {
            stopped = true;
            break;
        }
        visited += 1;
        if pair.old == pair.new {
            continue;
        }
        match (&pair.old, &pair.new) {
            (Some(Node::Directory(old_dir)), Some(Node::Directory(new_dir))) => {
                let old_tree = env.read(*old_dir).await?;
                let new_tree = env.read(*new_dir).await?;
                let mut names: BTreeSet<&Name> = BTreeSet::new();
                names.extend(old_tree.entries().keys());
                names.extend(new_tree.entries().keys());
                for name in names.into_iter().rev() {
                    let child = child_path(&pair.path, name);
                    let old = old_tree.entries().get(name).cloned();
                    let new = new_tree.entries().get(name).cloned();
                    stack.push(Pair { path: child, old, new });
                }
            }
            (Some(Node::Directory(dir)), other) => {
                let tree = env.read(*dir).await?;
                for (name, node) in tree.entries().iter().rev() {
                    let child = child_path(&pair.path, name);
                    stack.push(Pair { path: child, old: Some(node.clone()), new: None });
                }
                let other_is_leaf = matches!(other, Some(Node::File(_) | Node::Executable(_) | Node::Symlink(_)));
                if other_is_leaf {
                    let Some(node) = other.clone() else {
                        continue;
                    };
                    let leaf = leaf_of(node);
                    changed.push(Changed { path: pair.path.clone(), old: None, new: Some(leaf) });
                }
            }
            (other, Some(Node::Directory(dir))) => {
                let tree = env.read(*dir).await?;
                for (name, node) in tree.entries().iter().rev() {
                    let child = child_path(&pair.path, name);
                    stack.push(Pair { path: child, old: None, new: Some(node.clone()) });
                }
                let other_is_leaf = matches!(other, Some(Node::File(_) | Node::Executable(_) | Node::Symlink(_)));
                if other_is_leaf {
                    let Some(node) = other.clone() else {
                        continue;
                    };
                    let leaf = leaf_of(node);
                    changed.push(Changed { path: pair.path.clone(), old: Some(leaf), new: None });
                }
            }
            _ => {
                let old = pair.old.clone().map(leaf_of);
                let new = pair.new.clone().map(leaf_of);
                let old_is_symlink = matches!(old, Some(Leaf::Symlink(_)));
                let new_is_symlink = matches!(new, Some(Leaf::Symlink(_)));
                let old_is_file = matches!(old, Some(Leaf::File { .. }));
                let new_is_file = matches!(new, Some(Leaf::File { .. }));
                let first_swap = old_is_symlink && new_is_file;
                let second_swap = old_is_file && new_is_symlink;
                let swaps_kind = first_swap || second_swap;
                let old_present = old.is_some();
                let new_present = new.is_some();
                let both_present = old_present && new_present;
                let swaps = swaps_kind && both_present;
                if swaps {
                    changed.push(Changed { path: pair.path.clone(), old, new: None });
                    changed.push(Changed { path: pair.path, old: None, new });
                } else {
                    let either_present = old_present || new_present;
                    if either_present {
                        changed.push(Changed { path: pair.path, old, new });
                    }
                }
            }
        }
    }
    Ok(Walked { changed, visited, stopped })
}

/// The child of `parent` named `name`, as `entries` in `grep.rs` builds it.
fn child_path(parent: &str, name: &Name) -> String {
    if parent.is_empty() {
        name.as_str().to_owned()
    } else {
        format!("{parent}/{}", name.as_str())
    }
}

/// `node` as a changed leaf; never a directory.
fn leaf_of(node: Node) -> Leaf {
    match node {
        Node::File(blob) => Leaf::File { blob, mode: FileMode::Regular },
        Node::Executable(blob) => Leaf::File { blob, mode: FileMode::Executable },
        Node::Symlink(target) => Leaf::Symlink(target),
        Node::Directory(_) => unreachable!("a directory is expanded, never a leaf"),
    }
}
