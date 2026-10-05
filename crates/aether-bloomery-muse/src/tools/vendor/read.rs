//! `vendor.read`: a window of numbered lines from one vendored source file.

use aether_bloomery_kinds::{Mode, Refusal};
use aether_bloomery_program::{Async, Env, Program, Tooled, program};
use aether_bloomery_workspace_programs::proof::ProofBound;

use crate::tools::read::{ReadArgs, read};
use crate::tools::vendor::vendor_root;
use crate::tools::view::{Family, Viewed};

/// The `vendor.read` program.
pub struct VendorRead;

/// Reads a window of a UTF-8 file of the vendored crate sources as
/// `tree.read` reads one of the tree: numbered lines, `<n>\t<line>`, 1-based.
///
/// Paths are relative to the vendor tree's root, which holds one directory
/// per vendored crate.
#[program]
impl Program for VendorRead {
    const NAME: &'static str = "vendor.read";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Read numbered lines from one file of the vendored crate sources.";
    type Input = Tooled<ReadArgs, ProofBound>;
    type Result = Viewed;

    async fn run(input: Self::Input, env: &mut Env<Async>) -> Result<Self::Result, Refusal> {
        let root = vendor_root(&mut env, input.bound()).await?;
        read(&mut env, root, input.args(), Family::Vendor).await
    }
}

#[cfg(test)]
mod tests {
    use super::VendorRead;
    use crate::session::fixture::{SmallTree, path, run_async};
    use crate::tools::read::ReadArgs;

    #[test]
    fn a_vendor_read_reads_the_bound_vendor_tree_and_hints_its_own_sibling() {
        // Catches a vendor read run over the session's tree instead of the vendor tree the bound cites, and a hint
        // that sends the model to a `tree.*` tool for a path of the vendor tree.
        let small = SmallTree::new();
        for (args, text) in [
            (ReadArgs::new(path("run"), None, None), "1\t#!/bin/sh\n2\tsmelt"),
            (ReadArgs::new(path("src"), None, None), "src is a directory; use vendor.list."),
        ] {
            let (input, closure) = small.vendor_call(&args);
            let (viewed, _) = run_async::<VendorRead>(&input, closure).expect("a read is a result");
            assert_eq!(viewed.text(), text, "{args:?}");
        }
    }
}
