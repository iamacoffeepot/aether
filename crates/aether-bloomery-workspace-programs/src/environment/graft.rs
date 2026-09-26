//! Place one node on a path through an explicit frame stack.

use aether_bloomery_kinds::{Name, Node, Refusal, Tree};
use aether_bloomery_program::{Env, Sync};

use super::refused;

/// Place `node` as `leaf` in the directory `spine` names below `root`, and
/// return the new root, unstaged.
///
/// The walk loads only the directories on `spine`, one frame per segment on a
/// `Vec`, so its depth is the spine's length, never the depth of either tree.
/// A missing directory on the spine becomes an empty one. Popping the frames
/// from the leaf up stages each rebuilt directory and writes its `Ref` into
/// the parent, so every entry off the spine keeps the `Ref` it had.
///
/// # Errors
///
/// A [`Refusal::Refused`] naming the path when a spine entry is not a
/// directory or `leaf` is already there.
pub(super) fn graft(env: &mut Env<Sync>, root: Tree, spine: &[Name], leaf: Name, node: Node) -> Result<Tree, Refusal> {
    let mut frames: Vec<(Tree, &Name)> = Vec::with_capacity(spine.len());
    let mut current = root;
    for segment in spine {
        let child = match current.entries().get(segment) {
            None => Tree::empty(),
            Some(Node::Directory(child)) => env.injected(*child)?,
            Some(_) => {
                let at = path(frames.iter().map(|&(_, name)| name).chain([segment]));
                return Err(refused(format!("base tree: {at} is not a directory")));
            }
        };
        frames.push((current, segment));
        current = child;
    }
    if current.entries().contains_key(&leaf) {
        let at = path(spine.iter().chain([&leaf]));
        return Err(refused(format!("base tree: {at} already exists")));
    }

    let mut placed = with_entry(&current, leaf, node);
    while let Some((parent, segment)) = frames.pop() {
        let staged = env.stage_encoded(&placed)?;
        placed = with_entry(&parent, segment.clone(), Node::Directory(staged));
    }
    Ok(placed)
}

/// `tree` with `name` bound to `node`.
fn with_entry(tree: &Tree, name: Name, node: Node) -> Tree {
    let mut entries = tree.entries().clone();
    entries.insert(name, node);
    Tree::new(entries)
}

/// The `/`-joined path of `names`.
fn path<'a>(names: impl IntoIterator<Item = &'a Name>) -> String {
    names.into_iter().map(Name::as_str).collect::<Vec<_>>().join("/")
}
