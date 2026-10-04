//! The cargo config every proof run mounts at `/.cargo`: crates.io replaced
//! by the vendor tree, offline.
//!
//! Cargo reads `config.toml` from every ancestor of its working directory,
//! `/work`, so a config at `/.cargo` reaches every cargo process in the run,
//! including the `cargo metadata` a test spawns, which a top-level `--config`
//! flag does not.

use std::collections::BTreeMap;

use aether_bloomery_kinds::{EncodedArtifact, Name, Node, Ref, Tree};

use super::run::VENDOR;

/// The file name cargo looks for in each `.cargo` directory.
const FILE: &str = "config.toml";

/// The `config.toml` text, replacing crates.io with the directory the vendor
/// tree is mounted at.
fn text() -> String {
    format!(
        "[source.crates-io]\nreplace-with = \"vendored\"\n\n[source.vendored]\ndirectory = \"/{VENDOR}\"\n\n[net]\noffline = true\n"
    )
}

/// The tree whose one entry is `config.toml`.
pub(super) fn tree() -> Tree {
    let file = Node::File(Ref::of_bytes(text().as_bytes()));
    Tree::new(BTreeMap::from([(Name::new(FILE).expect("the config file name is a valid name"), file)]))
}

/// The two artifacts a session stages beside the [`ProofBound`](super::ProofBound)
/// that cites the config tree: the `config.toml` blob, then the tree.
///
/// # Panics
///
/// Never in practice: the fixed tree always encodes.
#[must_use]
pub fn cargo_config_artifacts() -> [EncodedArtifact; 2] {
    [
        EncodedArtifact::opaque_bytes(text().as_bytes()),
        EncodedArtifact::new(&tree()).expect("the fixed cargo config tree encodes"),
    ]
}

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{Digest, Node, Ref};

    use super::{FILE, cargo_config_artifacts, tree};
    use crate::proof::{ProofBound, TestEnv};

    #[test]
    fn the_bound_cites_the_tree_its_artifacts_store() {
        // Catches a bound citing a tree the opener never stages, which shows only live as an input the run lacks.
        let bound = ProofBound::new(
            Ref::from_digest(Digest::from_bytes([2; 32])),
            Ref::from_digest(Digest::from_bytes([3; 32])),
            TestEnv::default(),
        );
        let [blob, stored] = cargo_config_artifacts();
        assert_eq!(bound.cargo_config().digest(), stored.digest());

        let tree = tree();
        let entry = tree.entries().iter().find(|(name, _)| name.as_str() == FILE).map(|(_, node)| node);
        let cited = Node::File(Ref::from_digest(blob.digest()));
        assert_eq!(entry, Some(&cited));
    }
}
