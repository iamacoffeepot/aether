//! The programs a session binds as tools.
//!
//! Every tool's input is `Tooled<A>`: the session's current tree, which the
//! loop binds, and the arguments `A` the model writes. [`offered`] is the
//! set `muse.session.open` accepts: `tree.edit` and `tree.write`, which read
//! only the tree nodes and blobs they touch and return an `Edited` tree, and
//! `muse.echo`, a value-only fixture.
//!
//! A tool never refuses over what the model wrote: invalid arguments, a
//! path that names nothing usable, or a text over the cap return the tree
//! unchanged with a summary saying why, since a refused run is a fault and
//! ends the session. Only a tree node or blob the store cannot give refuses.

mod echo;
mod edit;
mod spine;
mod write;

use std::iter;

use aether_bloomery_kinds::{EncodedArtifact, Ref, Refusal};
use aether_bloomery_program::{Async, Env, Program, ToolArguments, ToolSchema, tool_definition};
use aether_data::{Schema, Storage};

pub use echo::{Echo, EchoArgs, EchoResult};
pub use edit::{EditArgs, TreeEdit};
pub use write::{TreeWrite, WriteArgs};

use crate::input::{OfferedTool, OfferedTools};
use crate::session::program_name;

/// The most bytes of text one tool call may write or match: 1 MiB.
pub const MAX_TEXT_BYTES: usize = 1 << 20;

/// Every bound tool offered as a session turn offers it, and the artifacts
/// those offers cite: each definition and each arguments and result schema,
/// for a caller that opens a session to stage.
///
/// # Panics
///
/// When a bound tool does not render as a tool or its schema does not encode,
/// which holds or fails the same way on every call.
#[must_use]
pub fn offered() -> (OfferedTools, Vec<EncodedArtifact>) {
    let (tools, artifacts): (Vec<_>, Vec<_>) =
        [bound::<Echo>(), bound::<TreeEdit>(), bound::<TreeWrite>()].into_iter().unzip();
    (OfferedTools::new(tools).expect("the bound tools keep every tool list rule"), artifacts.concat())
}

/// `P` as a bound tool, and the artifacts its offer cites.
fn bound<P: Program>() -> (OfferedTool, Vec<EncodedArtifact>)
where
    P::Input: ToolArguments,
    P::Result: Schema,
{
    let definition = tool_definition::<P>().expect("a bound tool renders as a tool").to_string();
    let schemas = [ToolSchema::of::<<P::Input as ToolArguments>::Arguments>(), ToolSchema::of::<P::Result>()]
        .map(|schema| EncodedArtifact::new(&schema).expect("a tool schema encodes"));
    let [input, result] = schemas.each_ref().map(|schema| Ref::from_digest(schema.digest()));
    let tool = OfferedTool::new(program_name::<P>(), Ref::of_text(&definition), input, result);
    (tool, iter::once(EncodedArtifact::text(&definition)).chain(schemas).collect())
}

/// A tool's arguments, or the summary of why they are invalid.
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
        .map_err(|error| format!("The arguments are invalid ({error}), so nothing changed.")))
}
