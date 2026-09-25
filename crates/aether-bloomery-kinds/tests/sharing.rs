//! A file change moves digests only along the path to the root.

use std::collections::BTreeMap;
use std::error::Error;

use aether_bloomery_kinds::{Name, Node, OpaqueBytes, Ref, Tree};

fn name(value: &str) -> Name {
    Name::new(value).expect("valid name")
}

fn file(bytes: &[u8]) -> Node {
    Node::File(Ref::of_bytes(bytes))
}

fn directory(tree: &Tree) -> Node {
    Node::Directory(Ref::of_encoded(tree).expect("tree encodes"))
}

fn tree(entries: &[(&str, Node)]) -> Tree {
    let mut map = BTreeMap::new();
    for (entry, node) in entries {
        map.insert(name(entry), node.clone());
    }
    Tree::new(map)
}

fn file_ref(node: &Node) -> Ref<OpaqueBytes> {
    match node {
        Node::File(digest) => *digest,
        other => panic!("expected file, got {other:?}"),
    }
}

fn dir_ref(node: &Node) -> Ref<Tree> {
    match node {
        Node::Directory(digest) => *digest,
        other => panic!("expected directory, got {other:?}"),
    }
}

fn entry<'a>(tree: &'a Tree, key: &str) -> &'a Node {
    tree.entries().get(&name(key)).unwrap_or_else(|| panic!("missing {key}"))
}

#[test]
fn changing_one_file_moves_digests_only_along_the_path_to_the_root() -> Result<(), Box<dyn Error>> {
    // Catches an encoder that leaks position, ordering, or sibling content
    // into a subtree's digest.
    let cargo = file(b"[package]\nname = \"root\"\n");
    let journal_lib = file(b"pub fn journal() {}");
    let store = file(b"pub fn store() {}");
    let kinds_lib = file(b"pub fn kinds() {}");
    let docs_page = file(b"# docs");

    let journal = tree(&[("lib.rs", journal_lib.clone()), ("store.rs", store.clone())]);
    let kinds = tree(&[("lib.rs", kinds_lib.clone())]);
    let crates = tree(&[("journal", directory(&journal)), ("kinds", directory(&kinds))]);
    let docs = tree(&[("x.md", docs_page.clone())]);
    let root = tree(&[("Cargo.toml", cargo.clone()), ("crates", directory(&crates)), ("docs", directory(&docs))]);

    let store_changed = file(b"pub fn store() { /* changed */ }");
    let journal_changed = tree(&[("lib.rs", journal_lib), ("store.rs", store_changed.clone())]);
    let crates_changed = tree(&[("journal", directory(&journal_changed)), ("kinds", directory(&kinds))]);
    let root_changed =
        tree(&[("Cargo.toml", cargo), ("crates", directory(&crates_changed)), ("docs", directory(&docs))]);

    assert_ne!(file_ref(&store), file_ref(&store_changed));
    assert_ne!(dir_ref(entry(&crates, "journal")), dir_ref(entry(&crates_changed, "journal")));
    assert_ne!(dir_ref(entry(&root, "crates")), dir_ref(entry(&root_changed, "crates")));
    assert_ne!(Ref::of_encoded(&root)?, Ref::of_encoded(&root_changed)?);

    assert_eq!(dir_ref(entry(&crates, "kinds")), dir_ref(entry(&crates_changed, "kinds")));
    assert_eq!(dir_ref(entry(&root, "docs")), dir_ref(entry(&root_changed, "docs")));
    assert_eq!(file_ref(entry(&root, "Cargo.toml")), file_ref(entry(&root_changed, "Cargo.toml")));
    assert_eq!(file_ref(entry(&journal, "lib.rs")), file_ref(entry(&journal_changed, "lib.rs")));
    assert_eq!(file_ref(entry(&kinds, "lib.rs")), file_ref(&kinds_lib));
    assert_eq!(file_ref(entry(&docs, "x.md")), file_ref(&docs_page));
    Ok(())
}
