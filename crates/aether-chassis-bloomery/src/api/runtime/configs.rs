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

/// One authored configuration held across its store write: the view the caller
/// gets back, and the canonical bytes that join this cap's resolved-config cache
/// once the row is durable.
///
/// The bytes ride along rather than being re-fetched, because the whole point of
/// caching them here is to keep the pre-seal gate off the store: an operator who
/// authors a policy and immediately seals a draft naming it would otherwise race
/// a read that has no reason to have happened yet.
pub(super) struct AuthoredConfig {
    pub(super) view: ConfigView,
    pub(super) bytes: Vec<u8>,
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
        let record =
            RecordConfig { digest: digest.as_bytes().to_vec(), kind: request.kind.clone(), bytes: bytes.clone() };
        let correlation = self.send_tracked(ctx.actor::<StoreCapability>(), &record);
        // Held across the write rather than rebuilt from the store's reply,
        // which carries only success or failure — the address is this route's to
        // report.
        self.authoring.insert(correlation, AuthoredConfig { view: ConfigView { digest, kind: request.kind }, bytes });
        Routed::Deferred(correlation)
    }

    /// Answer a held authoring request from the store's write reply: `200` with
    /// the address on a durable write, `500` on a failed one — never an address
    /// the caller could seal against nothing.
    ///
    /// A durable write also makes the content resolvable here, so a draft sealing
    /// the address the caller just received gates without waiting on another
    /// store read. A failed write files nothing: the address never reached the
    /// caller, and caching content the store does not hold would let a seal
    /// succeed here that no restart could reproduce.
    pub(super) fn resolve_config_write(&mut self, ctx: &NativeCtx<'_, Manual>, error: Option<&str>) -> Option<()> {
        let AuthoredConfig { view, bytes } = self.authoring.remove(&ctx.reply_target().correlation_id)?;
        let response = match error {
            None => {
                self.configs.insert(view.digest, view.kind.clone(), bytes);
                json(200, &view)
            }
            Some(error) => error_response(500, &format!("config write failed: {error}")),
        };
        self.answer(ctx, &response);
        Some(())
    }

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
