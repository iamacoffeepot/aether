//! The programs a session binds as tools.
//!
//! Every tool's input is `Tooled<A, B>`: the session's current tree, which the
//! loop binds, the arguments `A` the model writes, and the bound `B` the loop
//! binds from the offer. [`offered`] is the
//! set `muse.session.open` accepts: `tree.edit` and `tree.write`, which read
//! only the tree nodes and blobs they touch and return an `Edited` tree;
//! `tree.list`, `tree.read`, and `tree.grep`, which only read the tree and
//! return its text as [`Viewed`], capped at [`VIEW_MAX_BYTES`] with the cut
//! marked; and `muse.echo`, a value-only fixture. The model sees only the
//! arguments: the definition is rendered from the arguments schema alone, and
//! the bound travels only in the cited offer.
//!
//! A tool never refuses over what the model wrote: invalid arguments, a
//! path that names nothing usable, a pattern that does not compile, or a
//! text over the cap return a result saying why (an edit returns the tree
//! unchanged), since a refused run is a fault and ends the session. Only a
//! tree node or blob the store cannot give refuses.

mod echo;
mod edit;
mod grep;
mod list;
mod read;
mod spine;
mod view;
mod write;

use std::iter;

use aether_bloomery_kinds::{EncodedArtifact, Ref, Refusal};
use aether_bloomery_program::{Async, Env, NoBound, Program, ToolArguments, ToolSchema, tool_definition};
use aether_data::{Schema, Storage};

pub use echo::{Echo, EchoArgs, EchoResult};
pub use edit::{EditArgs, TreeEdit};
pub use grep::{GrepArgs, TreeGrep};
pub use list::{ListArgs, TreeList};
pub use read::{READ_MAX_LINES, ReadArgs, TreeRead};
pub use view::{VIEW_MAX_BYTES, Viewed};
pub use write::{TreeWrite, WriteArgs};

use crate::input::{OfferedTool, OfferedTools};
use crate::session::program_name;

/// The most bytes of text one tool call may write or match: 1 MiB.
pub const MAX_TEXT_BYTES: usize = 1 << 20;

/// Every bound tool offered as a session turn offers it, and the artifacts
/// those offers cite: each definition and each arguments, bound, and result schema,
/// for a caller that opens a session to stage.
///
/// # Panics
///
/// When a bound tool does not render as a tool or its schema does not encode,
/// which holds or fails the same way on every call.
#[must_use]
pub fn offered() -> (OfferedTools, Vec<EncodedArtifact>) {
    let (tools, artifacts): (Vec<_>, Vec<_>) = [
        bound::<Echo>(),
        bound::<TreeEdit>(),
        bound::<TreeWrite>(),
        bound::<TreeList>(),
        bound::<TreeRead>(),
        bound::<TreeGrep>(),
    ]
    .into_iter()
    .unzip();
    (OfferedTools::new(tools).expect("the bound tools keep every tool list rule"), artifacts.concat())
}

/// `P` as a bound tool, and the artifacts its offer cites. Every tool bound
/// here binds nothing beyond the tree, so its offer carries the `NoBound` value.
fn bound<P: Program>() -> (OfferedTool, Vec<EncodedArtifact>)
where
    P::Input: ToolArguments<Bound = NoBound>,
    P::Result: Schema,
{
    let definition = tool_definition::<P>().expect("a bound tool renders as a tool").to_string();
    let schemas = [
        ToolSchema::of::<<P::Input as ToolArguments>::Arguments>(),
        ToolSchema::of::<<P::Input as ToolArguments>::Bound>(),
        ToolSchema::of::<P::Result>(),
    ]
    .map(|schema| EncodedArtifact::new(&schema).expect("a tool schema encodes"));
    let [input, bound_schema, result] = schemas.each_ref().map(|schema| Ref::from_digest(schema.digest()));
    let bound = Ref::of_encoded(&NoBound).expect("the canonical bound encodes");
    let tool =
        OfferedTool::new(program_name::<P>(), Ref::of_text(&definition), input, bound.erase(), bound_schema, result);
    (
        tool,
        iter::once(EncodedArtifact::text(&definition))
            .chain(schemas)
            .chain([EncodedArtifact::new(&NoBound).expect("the canonical bound encodes")])
            .collect(),
    )
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
