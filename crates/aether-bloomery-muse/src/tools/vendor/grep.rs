//! `vendor.grep`: the lines of the vendored crate sources that match a regex.

use aether_bloomery_kinds::{Mode, Refusal};
use aether_bloomery_program::{Async, Env, Program, Tooled, program};
use aether_bloomery_workspace_programs::proof::ProofBound;

use crate::tools::grep::{Budget, GrepArgs, grep};
use crate::tools::vendor::vendor_root;
use crate::tools::view::{Family, Viewed};

/// The `vendor.grep` program.
pub struct VendorGrep;

/// Finds the lines of the vendored crate sources' UTF-8 files that match a
/// regex as `tree.grep` finds them in the tree, shown as
/// `<path>:<line>:<text>` in path order.
///
/// Paths are relative to the vendor tree's root, which holds one directory
/// per vendored crate; narrow `path` to one crate to keep the search short.
#[program]
impl Program for VendorGrep {
    const NAME: &'static str = "vendor.grep";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Find the lines of the vendored crate sources that match a regex.";
    type Input = Tooled<GrepArgs, ProofBound>;
    type Result = Viewed;

    async fn run(input: Self::Input, env: &mut Env<Async>) -> Result<Self::Result, Refusal> {
        let root = vendor_root(&mut env, input.bound()).await?;
        grep(&mut env, root, input.args(), Family::Vendor, Budget::FULL).await
    }
}

#[cfg(test)]
mod tests {
    use super::VendorGrep;
    use crate::session::fixture::{SmallTree, run_async};
    use crate::tools::grep::GrepArgs;

    #[test]
    fn a_vendor_grep_searches_the_bound_vendor_tree() {
        // Catches a vendor grep run over the session's tree instead of the vendor tree the bound cites.
        let small = SmallTree::new();
        let (input, closure) = small.vendor_call(&GrepArgs::new("smelt", None, None));
        let (viewed, _) = run_async::<VendorGrep>(&input, closure).expect("a grep is a result");
        assert_eq!(viewed.text(), "run:2:smelt\nsrc/lib.rs:1:pub fn smelt() {}");
    }
}
