//! `vendor.cargo` driven through the guest invocation seam, with the captured workspace run answered in place.
//!
//! The closure carries only the encoded input over fixed digests: the program reads nothing but its input.

mod support;

use std::error::Error;

use aether_bloomery_kinds::{Digest, Invoked, Ref, Refusal, Tree};
use aether_bloomery_workspace_programs::vendor::{CargoVendor, VendorInput, VendorResult};
use aether_workspace::{Outcome, RunResult};

use support::{completed_with, one_step, output_tree};

/// Start the vendor run over fixed digests, answer its one captured run with `reply`, and poll to the invocation's end.
fn answer(reply: &RunResult) -> Result<Invoked, Box<dyn Error>> {
    let input = VendorInput {
        source: Ref::from_digest(Digest::from_bytes([1; 32])),
        environment: Ref::from_digest(Digest::from_bytes([2; 32])),
    };
    support::answer::<CargoVendor>(&input, reply)
}

#[test]
fn exit_zero_vendors_citing_the_output_tree() -> Result<(), Box<dyn Error>> {
    // Catches citing the input source, stdout, or the run tree instead of the output.
    completed_with(answer(&one_step(Some(0))?)?, &VendorResult::Vendored { tree: output_tree() })
}

#[test]
fn a_non_zero_exit_fails_citing_stderr_not_stdout() -> Result<(), Box<dyn Error>> {
    // Catches a failed vendor recorded as a vendor tree, and the wrong log cited.
    completed_with(answer(&one_step(Some(101))?)?, &VendorResult::Failed { stderr: Ref::of_bytes(b"stderr") })
}

#[test]
fn a_workspace_refusal_is_the_programs_refusal() -> Result<(), Box<dyn Error>> {
    // Catches a refusal recorded as a vendor result, over the empty run tree's missing-input path.
    let missing = Ref::of_encoded(&Tree::empty())?.digest();
    let invoked = answer(&RunResult::Refused(aether_workspace::Refusal::InputMissing(missing)))?;
    let Invoked::Refused { seq: 7, refusal: Refusal::Refused { reason } } = invoked else {
        return Err(format!("expected the program's own refusal, got {invoked:?}").into());
    };
    assert!(reason.as_str().contains("InputMissing"), "{reason:?}");
    Ok(())
}

#[test]
fn a_run_that_answers_no_step_is_refused() -> Result<(), Box<dyn Error>> {
    // Catches an executor's malformed outcome recorded as a vendor result.
    let invoked = answer(&RunResult::Ok(Outcome { steps: Vec::new(), tree: output_tree() }))?;
    assert!(matches!(invoked, Invoked::Refused { seq: 7, refusal: Refusal::Refused { .. } }), "{invoked:?}");
    Ok(())
}
