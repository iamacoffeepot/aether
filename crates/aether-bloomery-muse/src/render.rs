//! The text each conversation item sends.
//!
//! A message sends its text, a replayed call its verbatim arguments, and a
//! refused output its stored text. A result is rendered: its stored payload
//! decoded with the schema its output cites and serialized as JSON. Object
//! keys serialize sorted, so the rendered bytes depend only on the cited
//! payload and schema, and the same input always sends the same request.

use aether_bloomery_kinds::{Detail, ErasedRef, Refusal};
use aether_bloomery_program::{Async, Env, ToolSchema};
use aether_codec::decode_storage_schema;

use crate::input::{ToolOutput, TurnItem};

/// Most JSON values one rendered result may hold.
pub const RENDER_MAXIMUM_VALUES: usize = 65_536;

/// The text `item` sends, read from the closure the driver injected.
///
/// # Errors
///
/// The read's refusal, or `Refusal::Refused` when a result is not stored
/// under its schema's kind or does not decode with it: the input was built
/// wrong, and nothing is fetched.
pub async fn item(env: &mut Env<Async>, item: &TurnItem) -> Result<String, Refusal> {
    match item {
        TurnItem::Message { text, .. } | TurnItem::CallOutput { output: ToolOutput::Refused(text), .. } => {
            env.read_text(*text).await
        }
        TurnItem::Call(call) => env.read_text(call.arguments()).await,
        TurnItem::CallOutput { output: ToolOutput::Result { schema, result }, .. } => {
            let schema = env.read(*schema).await?;
            let result = of_kind(&schema, *result)?;
            json(&schema, &env.read_payload(result).await?)
        }
    }
}

/// `result`, when it is cited under `schema`'s kind.
fn of_kind(schema: &ToolSchema, result: ErasedRef) -> Result<ErasedRef, Refusal> {
    if result.kind() == schema.kind_id() {
        Ok(result)
    } else {
        Err(refused(format!("a call output's result is not stored as its schema's kind {}", schema.kind_name())))
    }
}

/// `payload` decoded with `schema`, as JSON.
fn json(schema: &ToolSchema, payload: &[u8]) -> Result<String, Refusal> {
    let value = decode_storage_schema(payload, &schema.schema(), RENDER_MAXIMUM_VALUES).map_err(|error| {
        refused(format!("a call output's result does not decode as {}: {error}", schema.kind_name()))
    })?;
    Ok(serde_json::to_string(&value).expect("a decoded JSON value always serializes"))
}

fn refused(reason: String) -> Refusal {
    Refusal::Refused { reason: Detail::new(reason) }
}
