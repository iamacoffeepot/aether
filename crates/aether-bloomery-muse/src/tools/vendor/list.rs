//! `vendor.list`: one directory level of the vendored crate sources.

use aether_bloomery_kinds::{Mode, Refusal};
use aether_bloomery_program::{Async, Env, Program, Tooled, program};
use aether_bloomery_workspace_programs::proof::ProofBound;

use crate::tools::list::{ListArgs, list};
use crate::tools::vendor::vendor_root;
use crate::tools::view::{Family, Viewed};

/// The `vendor.list` program.
pub struct VendorList;

/// Lists one directory of the vendored crate sources as `tree.list` lists
/// one of the tree: a line per entry, `<kind>\t<name>`, in name order.
///
/// Paths are relative to the vendor tree's root, which holds one directory
/// per vendored crate.
#[program]
impl Program for VendorList {
    const NAME: &'static str = "vendor.list";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "List the entries of one directory of the vendored crate sources.";
    type Input = Tooled<ListArgs, ProofBound>;
    type Result = Viewed;

    async fn run(input: Self::Input, env: &mut Env<Async>) -> Result<Self::Result, Refusal> {
        let root = vendor_root(&mut env, input.bound()).await?;
        list(&mut env, root, input.args(), Family::Vendor).await
    }
}

#[cfg(test)]
mod tests {
    use super::VendorList;
    use crate::session::fixture::{SmallTree, path, run_async};
    use crate::tools::list::ListArgs;

    #[test]
    fn a_vendor_listing_reads_the_bound_vendor_tree_and_hints_its_own_sibling() {
        // Catches a vendor listing run over the session's tree instead of the vendor tree the bound cites, and a
        // hint that sends the model to a `tree.*` tool for a path of the vendor tree.
        let small = SmallTree::new();
        for (args, text) in [
            (ListArgs::new(Some(path("src"))), "file\tlib.rs"),
            (ListArgs::new(Some(path("README"))), "README is a file; use vendor.read."),
        ] {
            let (input, closure) = small.vendor_call(&args);
            let (viewed, _) = run_async::<VendorList>(&input, closure).expect("a listing is a result");
            assert_eq!(viewed.text(), text, "{args:?}");
        }
    }
}
