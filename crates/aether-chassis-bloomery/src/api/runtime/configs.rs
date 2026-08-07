//! The configuration authoring route — `POST /configs` (ADR-0174).
//!
//! A sealed [`ConfigRegistry`](aether_bloomery::ConfigRegistry) names each
//! configuration by an opaque address, so without a route that computes one an
//! operator could seal a configuration only by content-addressing it
//! out-of-band. Post the value, get the address back, name it in a draft.
//!
//! One route serves every configuration kind, which is what the registry buys.
//! It resolves the request's kind name against the descriptor inventory every
//! native binary carries, encodes the JSON body through that kind's schema with
//! [`encode_schema`], and addresses the result. Adding a configuration kind
//! therefore adds no route.
//!
//! Authoring also **stores** the bytes under the address, which is what makes a
//! seal more than an attestation: resolution at the point of use reads that row.
//! The write happens here rather than at seal because a seal request carries
//! only the address — the content exists nowhere else.
//!
//! The route answers on the store's reply rather than inline, so a `200` means
//! the address it hands back will actually resolve. That matters more here than
//! it does for a fire-and-forget write: a configuration sealed against a missing
//! row is exactly the receipt-says-one-thing-run-does-another divergence the
//! registry exists to close, so handing back an address before the row is
//! durable would reintroduce it at the authoring step.
//!
//! Content addressing makes the write idempotent — identical content under the
//! same kind addresses to the same digest and rewrites the same row — so the
//! route is safely repeatable.

use aether_actor::Manual;
use aether_bloomery::{Digest, config_address};
use aether_codec::encode_schema;
use aether_data::schema::SchemaType;
use aether_kinds::descriptors;
use aether_substrate::actor::native::NativeCtx;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::response::{error_response, json};
use super::state::{ApiCapabilityState, Routed};
use crate::store::{RecordConfig, StoreCapability};

/// The request: which kind the value is, and the value itself as JSON.
#[derive(Deserialize)]
pub(super) struct ConfigRequest {
    /// The configuration kind's name, as `#[kind(name = "…")]` declares it.
    kind: String,
    /// The configuration, shaped by that kind's schema.
    value: Value,
}

/// The reply: the address to name in a registry, and the kind it was sealed
/// under. Both, because the address alone does not say what it addresses and a
/// registry entry needs the pair.
#[derive(Serialize)]
pub(super) struct ConfigView {
    /// The content address to name in a draft's or member's registry.
    digest: Digest,
    /// The kind the bytes decode as — the registry key.
    kind: String,
}

/// The kind's schema, from the descriptor inventory this binary links.
///
/// A miss means the requested kind is not compiled into the resolving binary,
/// which is a client error rather than a server one: the vocabulary is fixed at
/// build time, so no retry helps and the caller needs to know the name is wrong.
fn schema_of(kind: &str) -> Option<SchemaType> {
    descriptors::all().into_iter().find(|entry| entry.name == kind).map(|entry| entry.schema)
}

impl ApiCapabilityState {
    /// `POST /configs` — encode a configuration through its kind's schema,
    /// address it, store it under that address, and reply with both once the
    /// write lands.
    pub(super) fn author_config(&mut self, ctx: &NativeCtx<'_, Manual>, body: &[u8]) -> Routed {
        let request: ConfigRequest = match serde_json::from_slice(body) {
            Ok(request) => request,
            Err(error) => return Routed::Reply(error_response(400, &format!("invalid config body: {error}"))),
        };

        let Some(schema) = schema_of(&request.kind) else {
            return Routed::Reply(error_response(400, &format!("unknown config kind `{}`", request.kind)));
        };
        let bytes = match encode_schema(&request.value, &schema) {
            Ok(bytes) => bytes,
            Err(error) => {
                return Routed::Reply(error_response(
                    400,
                    &format!("config does not match `{}`: {error}", request.kind),
                ));
            }
        };

        let digest = config_address(&request.kind, &bytes);
        let record = RecordConfig { digest: digest.as_bytes().to_vec(), kind: request.kind.clone(), bytes };
        let correlation = self.send_tracked(ctx.actor::<StoreCapability>(), &record);
        // Held across the write rather than rebuilt from the store's reply,
        // which carries only success or failure — the address is this route's to
        // report.
        self.configs.insert(correlation, ConfigView { digest, kind: request.kind });
        Routed::Deferred(correlation)
    }

    /// Answer a held authoring request from the store's write reply: `200` with
    /// the address on a durable write, `500` on a failed one — never an address
    /// the caller could seal against nothing.
    pub(super) fn resolve_config_write(&mut self, ctx: &NativeCtx<'_, Manual>, error: Option<&str>) -> Option<()> {
        let view = self.configs.remove(&ctx.reply_target().correlation_id)?;
        let response = error
            .map_or_else(|| json(200, &view), |error| error_response(500, &format!("config write failed: {error}")));
        self.answer(ctx, &response);
        Some(())
    }
}
