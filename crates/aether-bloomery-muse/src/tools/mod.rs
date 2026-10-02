//! The programs a session binds as tools.
//!
//! Every tool's input is `Tooled<A, B>`: the session's current tree and the
//! bound value `B` its offer carries, which the loop binds, and the arguments
//! `A` the model writes. Every tool here binds `NoBound`. [`offered`] is the
//! set `muse.session.open` accepts: `tree.edit` and `tree.write`, which read
//! only the tree nodes and blobs they touch and return an `Edited` tree;
//! `tree.list`, `tree.read`, and `tree.grep`, which only read the tree and
//! return its text as [`Viewed`], capped at [`VIEW_MAX_BYTES`] with the cut
//! marked; `muse.echo`, a value-only fixture; and `muse.end`, which ends the
//! run as done, blocked, or asking a question.
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
mod spine;
mod view;
mod write;

use std::iter;

use aether_bloomery_kinds::{EncodedArtifact, Ref, Refusal};
use aether_bloomery_program::{Async, Env, NoBound, Program, ToolArguments, ToolSchema, tool_definition};
use aether_data::{Schema, Storage};

pub use echo::{Echo, EchoArgs, EchoResult};
pub use edit::{EditArgs, TreeEdit};
pub use end::{End, EndArgs, Ending, NUDGE_TEXT, end_position, end_result};
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
/// those offers cite: each definition, each arguments and result schema, and
/// the bound value, for a caller that opens a session to stage.
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
        bound::<End>(),
    ]
    .into_iter()
    .unzip();
    let mut artifacts = artifacts.concat();
    artifacts.push(EncodedArtifact::text(NUDGE_TEXT));
    (OfferedTools::new(tools).expect("the bound tools keep every tool list rule"), artifacts)
}

/// `P` as a bound tool, and the artifacts its offer cites. Every tool bound
/// here binds nothing beyond the tree, so its offer carries the `NoBound` value.
fn bound<P: Program>() -> (OfferedTool, Vec<EncodedArtifact>)
where
    P::Input: ToolArguments<Bound = NoBound>,
    P::Result: Schema,
{
    let definition = tool_definition::<P>().expect("a bound tool renders as a tool").to_string();
    let schemas = [ToolSchema::of::<<P::Input as ToolArguments>::Arguments>(), ToolSchema::of::<P::Result>()]
        .map(|schema| EncodedArtifact::new(&schema).expect("a tool schema encodes"));
    let [input, result] = schemas.each_ref().map(|schema| Ref::from_digest(schema.digest()));
    let bound = EncodedArtifact::new(&NoBound).expect("the bound encodes");
    let tool = OfferedTool::new(
        program_name::<P>(),
        Ref::of_text(&definition),
        input,
        Ref::<NoBound>::from_digest(bound.digest()).erase(),
        result,
    );
    (tool, iter::once(EncodedArtifact::text(&definition)).chain(schemas).chain([bound]).collect())
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
