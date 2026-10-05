//! The programs a session binds as tools.
//!
//! Every tool's input is `Tooled<A, B>`: the session's current tree and the
//! bound value `B` its offer carries, which the loop binds, and the arguments
//! `A` the model writes. Every tool here lives in the bundle [`MUSE`] resolves
//! to and binds `NoBound`. [`offered`] is the set every session offers:
//! `tree.edit`, `tree.write`, and `tree.remove`, which read only the tree
//! nodes and blobs they touch and return an `Edited` tree;
//! `tree.list`, `tree.read`, and `tree.grep`, which only read the tree and
//! return its text as [`Viewed`], capped at [`VIEW_MAX_BYTES`] with the cut
//! marked; `muse.echo`, a value-only fixture; and `muse.end`, which ends the
//! run as done, blocked, or asking a question. [`offered_with_proofs`] adds
//! the proofs from the bundle `WORKSPACE_PROGRAMS` resolves to,
//! `proof.clippy`, which formats the tree and checks it with clippy, and
//! `proof.test`, which formats the tree and runs its workspace tests with
//! the session's test env; each returns the formatted tree as an `Edited`
//! and binds the session's `ProofBound`, its environment, vendor tree, and
//! test env.
//!
//! A tool never refuses over what the model wrote: invalid arguments, a
//! path that names nothing usable, a pattern that does not compile, or a
//! text over the cap return a result saying why (an edit returns the tree
//! unchanged), since a refused run is a fault and ends the session. Only a
//! tree node or blob the store cannot give refuses.

mod echo;
mod edit;
mod end;
mod grep;
mod list;
mod read;
mod remove;
mod spine;
mod view;
mod write;

use std::iter;

use aether_bloomery_kinds::{EncodedArtifact, Head, OpaqueBytes, Ref, Refusal};
use aether_bloomery_program::{Async, Env, NoBound, Program, ToolArguments, ToolSchema, tool_definition};
use aether_bloomery_workspace_programs::WORKSPACE_PROGRAMS;
use aether_bloomery_workspace_programs::proof::{ClippyProof, ProofBound, TestProof};
use aether_data::{Schema, Storage};

pub use echo::{Echo, EchoArgs, EchoResult};
pub use edit::{EditArgs, TreeEdit};
pub use end::{End, EndArgs, Ending, NUDGE_TEXT, end_position, ends_run};
pub use grep::{GrepArgs, TreeGrep};
pub use list::{ListArgs, TreeList};
pub use read::{READ_MAX_LINES, ReadArgs, TreeRead};
pub use remove::{RemoveArgs, TreeRemove};
pub use view::{VIEW_MAX_BYTES, Viewed};
pub use write::{TreeWrite, WriteArgs};

use crate::input::{OfferedTool, OfferedTools};
use crate::session::{MUSE, program_name};

/// The most bytes of text one tool call may write or match: 1 MiB.
pub const MAX_TEXT_BYTES: usize = 1 << 20;

/// Every bound tool offered as a session turn offers it, and the artifacts
/// those offers cite: each definition, each arguments and result schema, and
/// the bound value, for a caller that opens a session to stage.
///
/// # Panics
///
/// When a bound tool does not render as a tool or its schema does not encode,
/// which holds or fails the same way on every call.
#[must_use]
pub fn offered() -> (OfferedTools, Vec<EncodedArtifact>) {
    let no_bound = EncodedArtifact::new(&NoBound).expect("the bound encodes");
    let none = Ref::<NoBound>::from_digest(no_bound.digest());
    let (tools, artifacts): (Vec<_>, Vec<_>) = [
        bound::<Echo>(MUSE, none),
        bound::<TreeEdit>(MUSE, none),
        bound::<TreeWrite>(MUSE, none),
        bound::<TreeRemove>(MUSE, none),
        bound::<TreeList>(MUSE, none),
        bound::<TreeRead>(MUSE, none),
        bound::<TreeGrep>(MUSE, none),
        bound::<End>(MUSE, none),
    ]
    .into_iter()
    .unzip();
    let mut artifacts = artifacts.concat();
    artifacts.extend([no_bound, EncodedArtifact::text(NUDGE_TEXT)]);
    (OfferedTools::new(tools).expect("the bound tools keep every tool list rule"), artifacts)
}

/// Every tool [`offered`] offers, then every proof tool bound to `proofs`,
/// and the artifacts those offers cite, `proofs` among them.
///
/// # Panics
///
/// As [`offered`] does.
#[must_use]
pub fn offered_with_proofs(proofs: &ProofBound) -> (OfferedTools, Vec<EncodedArtifact>) {
    let (tools, mut artifacts) = offered();
    let bound = EncodedArtifact::new(proofs).expect("the proof bound encodes");
    let (proofs, cited): (Vec<_>, Vec<_>) = proof_offers(Ref::from_digest(bound.digest())).into_iter().unzip();
    artifacts.extend(cited.into_iter().flatten().chain([bound]));
    let tools = tools.as_slice().iter().cloned().chain(proofs).collect();
    (OfferedTools::new(tools).expect("the bound tools keep every tool list rule"), artifacts)
}

/// Every proof tool from the bundle [`WORKSPACE_PROGRAMS`] resolves to,
/// binding `proofs` into every call, and the artifacts each offer cites
/// besides `proofs`.
pub fn proof_offers(proofs: Ref<ProofBound>) -> Vec<(OfferedTool, Vec<EncodedArtifact>)> {
    vec![bound::<ClippyProof>(WORKSPACE_PROGRAMS, proofs), bound::<TestProof>(WORKSPACE_PROGRAMS, proofs)]
}

/// `P` as a bound tool from the bundle `head` resolves to, binding the cited
/// `value` into every call, and the artifacts its offer cites besides `value`.
fn bound<P: Program>(
    head: Head<OpaqueBytes>,
    value: Ref<<P::Input as ToolArguments>::Bound>,
) -> (OfferedTool, Vec<EncodedArtifact>)
where
    P::Input: ToolArguments,
    P::Result: Schema,
{
    let definition = tool_definition::<P>().expect("a bound tool renders as a tool").to_string();
    let schemas = [ToolSchema::of::<<P::Input as ToolArguments>::Arguments>(), ToolSchema::of::<P::Result>()]
        .map(|schema| EncodedArtifact::new(&schema).expect("a tool schema encodes"));
    let [input, result] = schemas.each_ref().map(|schema| Ref::from_digest(schema.digest()));
    let tool = OfferedTool::new(program_name::<P>(), head, Ref::of_text(&definition), input, value.erase(), result);
    (tool, iter::once(EncodedArtifact::text(&definition)).chain(schemas).collect())
}

/// A tool's arguments, or the sentence, without its closing stop, on why
/// they are invalid.
///
/// `muse.turn` decodes the model's JSON by schema alone, so the arguments
/// reach the tool without their `#[storage(validate)]` rules checked; the
/// tool's own decode checks them, and a broken rule is the model's mistake.
///
/// # Errors
///
/// The [`Refusal`] of arguments the store cannot give.
async fn read_args<A: Storage>(env: &mut Env<Async>, args: Ref<A>) -> Result<Result<A, String>, Refusal> {
    let payload = env.read_payload(args.erase()).await?;
    Ok(A::decode_storage(&payload)
        .map(|data| data.value)
        .map_err(|error| format!("The arguments are invalid ({error})")))
}
