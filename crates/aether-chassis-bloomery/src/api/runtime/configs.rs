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
//!
//! Authoring also fills this cap's own resolved-configuration cache, and the
//! boot [`LoadConfigs`] read seeds it. The pre-seal approve gate resolves a
//! draft's sealed tier policy out of that cache (#4616) rather than reaching the
//! store from inside a synchronous admission decision, so the cap needs the
//! content in hand before it can gate anything.

use aether_actor::Manual;
use aether_bloomery::{Digest, LoadConfigs, LoadConfigsResult, config_address};
use aether_codec::encode_schema;
use aether_data::schema::SchemaType;
use aether_http::HttpServerResponse;
use aether_kinds::descriptors;
use aether_substrate::actor::native::NativeCtx;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::response::{error_response, json};
use super::state::{ApiCapabilityState, Routed};
use crate::store::{RecordConfig, RecordConfigResult, StoreCapability};

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

/// `POST /configs` — encode a configuration through its kind's schema, address
/// it, and relay the store write; [`config_response`] answers once it lands.
pub(super) fn author_config(body: &[u8]) -> Routed {
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
            return Routed::Reply(error_response(400, &format!("config does not match `{}`: {error}", request.kind)));
        }
    };

    let digest = config_address(&request.kind, &bytes);
    Routed::RecordConfig(RecordConfig { digest: digest.as_bytes().to_vec(), kind: request.kind, bytes })
}

/// Render the store's write reply into the authoring response: `200` with the
/// address on a durable write, `500` on a failed one — never an address the
/// caller could seal against nothing.
///
/// Everything this route needs arrives in the reply's own echo rather than in
/// state held across the write, which is what lets it ride the plain relay
/// (ADR-0154 §3) instead of keeping a correlation map. That is also why the echo
/// carries the bytes: a durable write has to leave the content resolvable here
/// too (#4616), and a `#[http::reply]` route receives only the reply.
///
/// A failed write files nothing — the address never reached the caller, and
/// caching content the store does not hold would let a seal succeed here that no
/// restart could reproduce. A digest that is not 32 bytes is the store
/// contradicting its own contract, so it is a `500` rather than a silently
/// truncated address.
pub(super) fn config_response(state: &mut ApiCapabilityState, result: RecordConfigResult) -> HttpServerResponse {
    match result {
        RecordConfigResult::Ok { digest, kind, bytes } => {
            let Some(address) = Digest::from_slice(&digest) else {
                return error_response(500, &format!("config write echoed a {}-byte address", digest.len()));
            };

            state.configs.insert(address, kind.clone(), bytes);
            json(200, &ConfigView { digest: address, kind })
        }
        RecordConfigResult::Err { error } => error_response(500, &format!("config write failed: {error}")),
    }
}

impl ApiCapabilityState {
    /// Fill the resolved-configuration cache from the store's boot read (#4616).
    ///
    /// The boot posture is the control core's (ADR-0174): an unreadable table or
    /// a malformed address aborts rather than coming up on a partial cache,
    /// because a cap that silently held less content than the store does would
    /// gate a seal against a policy it merely failed to read — resolving a lower
    /// tier than the bloom actually sealed, which is the one failure this whole
    /// path exists to make impossible.
    pub(super) fn hydrate_configs(&mut self, ctx: &mut NativeCtx<'_, Manual>, mail: LoadConfigsResult) {
        let records = match mail {
            LoadConfigsResult::Ok { records } => records,
            LoadConfigsResult::Err { error } => ctx.fatal_abort(format!("boot configuration read failed: {error}")),
        };
        for record in records {
            let Some(address) = Digest::from_slice(&record.digest) else {
                ctx.fatal_abort(format!("stored configuration `{}` has a malformed address", record.kind));
            };
            self.configs.insert(address, record.kind, record.bytes);
        }
        self.configs_ready = true;
    }
}

/// Ask the store for every stored configuration — the boot read the cap's `wire`
/// fires so the pre-seal gate has content to resolve a sealed policy against.
pub(super) fn load_configs(ctx: &mut NativeCtx<'_>) {
    ctx.actor::<StoreCapability>().send_detached(&LoadConfigs);
}
