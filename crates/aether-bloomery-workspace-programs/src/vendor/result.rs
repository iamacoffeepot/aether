//! `vendor.cargo.result`: the vendor tree cargo wrote, or the stderr of a vendor run that failed.

use aether_bloomery_kinds::{OpaqueBytes, Ref, Tree};

/// The answer of one cargo vendor step.
///
/// The outcome is the variant, so no field can disagree with it.
///
/// **Pairing.** `Vendored.tree` is the `cargo vendor --locked` directory for
/// the `Cargo.lock` at the root of the transition's `source`: one directory
/// per registry package, each holding its `.cargo-checksum.json`, the layout
/// `source.vendored.directory` reads. A `proof.clippy` input is well-formed
/// when its `vendor` is the `Vendored.tree` of a `vendor.cargo` transition
/// whose `source` has the same `Cargo.lock` as the proof's `source`, and in
/// practice the same `source` digest. The proof replaces only `crates-io`, so
/// the pairing covers registry sources only. A dependency-free source vendors
/// to the empty tree.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "vendor.cargo.result")]
pub enum VendorResult {
    /// The step exited 0: the tree is the vendor directory.
    Vendored {
        /// `/work` after the step, minus scratch: exactly what cargo vendor wrote.
        tree: Ref<Tree>,
    },
    /// The step exited with any other code, or died by signal, such as a
    /// stale lock under `--locked` or a registry fetch error.
    Failed {
        /// Everything the step wrote to stderr, where cargo reports why.
        stderr: Ref<OpaqueBytes>,
    },
}
