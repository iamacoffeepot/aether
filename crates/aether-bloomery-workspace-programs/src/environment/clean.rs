//! Drop Docker's init-layer placeholders from the base root.
//!
//! Docker's init layer adds an empty `.dockerenv` at the root and an empty
//! `console` file and empty `pts` and `shm` directories under `dev/` to every
//! container, so an import carries them whatever the image holds. Keeping
//! them would make an environment's digest depend on the import backend, and
//! a run gains nothing from them: the runtime supplies `/dev`, and every
//! container gets its own `.dockerenv`.

use aether_bloomery_kinds::{Node, OpaqueBytes, Ref, Refusal, Tree};
use aether_bloomery_program::{Env, Sync};

use super::{name, refused};

/// The marker file Docker's init layer writes at the root.
const DOCKERENV: &str = ".dockerenv";

/// The mount point where the runtime supplies `/dev`.
const DEV: &str = "dev";

/// `root` without an empty `.dockerenv`, and with `dev` replaced by a staged
/// empty directory once every entry of it is proven empty.
///
/// # Errors
///
/// A [`Refusal::Refused`] naming the path when `.dockerenv` is not an empty
/// file, `dev` is not a directory, or an entry of `dev` is not an empty file
/// or an empty directory: the merge cannot tell any of those from userland
/// content.
pub(super) fn clean(env: &mut Env<Sync>, root: &Tree) -> Result<Tree, Refusal> {
    let empty_file = Ref::<OpaqueBytes>::of_bytes(&[]);
    let empty_tree = Ref::of_encoded(&Tree::empty()).map_err(|error| refused(format!("the empty tree: {error}")))?;
    let dockerenv = name(DOCKERENV)?;
    let dev = name(DEV)?;

    let mut entries = root.entries().clone();
    match entries.remove(&dockerenv) {
        None => {}
        Some(Node::File(blob) | Node::Executable(blob)) if blob == empty_file => {}
        Some(_) => return Err(refused(format!("base tree: {DOCKERENV} is not an empty file"))),
    }
    let placeholders = match entries.get(&dev) {
        None => return Ok(Tree::new(entries)),
        Some(Node::Directory(tree)) => env.injected(*tree)?,
        Some(_) => return Err(refused(format!("base tree: {DEV} is not a directory"))),
    };
    for (entry, node) in placeholders.entries() {
        let empty = match node {
            Node::File(blob) | Node::Executable(blob) => *blob == empty_file,
            Node::Directory(tree) => *tree == empty_tree,
            Node::Symlink(_) => false,
        };
        if !empty {
            let entry = entry.as_str();
            return Err(refused(format!("base tree: {DEV}/{entry} is not an empty file or directory")));
        }
    }
    entries.insert(dev, Node::Directory(env.stage_encoded(&Tree::empty())?));
    Ok(Tree::new(entries))
}
