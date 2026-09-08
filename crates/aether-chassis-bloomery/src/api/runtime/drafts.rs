//! The draft shaping routes — open, read, list, and patch. Pre-seal shaping
//! "claims nothing" (ADR-0149 §The bloom), so a draft lives entirely in the
//! router's `drafts` map under a monotonic per-process handle and every route
//! here answers synchronously. Sealing that draft is the next module over.
//!
//! One field is the host's rather than the operator's. Naming a `base` is what
//! forms a draft against a tree, so it is also where the coordinator reads that
//! tree's `pipeline.toml` and seals the lane vocabulary it declares (ADR-0215);
//! see [`pipeline`](super::pipeline) for what the read tolerates and what it
//! refuses.

use aether_actor::Manual;
use aether_bloomery::{BloomDraft, ConfigRegistry, PIPELINE_MANIFEST_PATH, PipelineManifest, PipelineManifestError};
use aether_data::Kind;
use aether_http::HttpServerResponse;
use aether_substrate::actor::native::NativeCtx;

use super::hex;
use super::pipeline::DerivedManifest;
use super::response::{error_response, json};
use super::state::{ApiCapabilityState, MAX_OPEN_DRAFTS, Routed};
use crate::api::dto::{DraftPatch, DraftView, DraftsView};
use crate::store::{RecordConfig, StoreCapability};

impl ApiCapabilityState {
    /// `POST /drafts` — open a fresh empty draft under a new handle.
    pub(super) fn open_draft(&mut self) -> Routed {
        if self.drafts.len() >= MAX_OPEN_DRAFTS {
            return Routed::Reply(error_response(429, "open-draft budget exhausted"));
        }
        let draft_id = self.next_draft;
        self.next_draft += 1;
        let draft = BloomDraft::default();
        self.drafts.insert(draft_id, draft.clone());
        Routed::Reply(json(201, &DraftView { draft_id: draft_id.to_string(), draft }))
    }

    /// `GET /drafts/{id}` — read one open draft.
    pub(super) fn get_draft(&self, id: &str) -> Routed {
        match self.lookup_draft(id) {
            Ok((draft_id, draft)) => Routed::Reply(json(200, &DraftView { draft_id: draft_id.to_string(), draft })),
            Err(response) => Routed::Reply(response),
        }
    }

    /// `PATCH /drafts/{id}` — replace the present fields of an open draft.
    ///
    /// A patch that names a `base` also resolves that base's pipeline manifest
    /// and seals it into the draft's registry, so the vocabulary a bloom
    /// attests is read off the tree it will run rather than compiled into the
    /// coordinator (ADR-0215). Every refusal is decided before the draft is
    /// touched: a patch that cannot be honoured leaves the draft as it was
    /// rather than half-applied.
    pub(super) fn patch_draft(&mut self, ctx: &NativeCtx<'_, Manual>, id: &str, body: &[u8]) -> Routed {
        let handle = match parse_draft_id(id) {
            Some(handle) if self.drafts.contains_key(&handle) => handle,
            _ => return Routed::Reply(error_response(404, "no such draft")),
        };
        let patch: DraftPatch = match hex::from_slice(body) {
            Ok(patch) => patch,
            Err(error) => return Routed::Reply(error_response(400, &format!("invalid draft patch: {error}"))),
        };
        let derived = match self.derived_manifest(handle, &patch) {
            Ok(derived) => derived,
            Err(response) => return Routed::Reply(response),
        };

        let draft = self.drafts.get_mut(&handle).expect("draft presence checked above");
        if let Some(proposals) = patch.proposals {
            draft.proposals = proposals;
        }
        if let Some(configs) = patch.configs {
            draft.configs = configs;
        }
        if let Some(base) = patch.base {
            draft.base = base;
        }
        if let Some(forecast) = patch.forecast {
            draft.forecast = forecast;
        }
        if let Some(manifest) = &derived {
            draft.configs.insert::<PipelineManifest>(manifest.address);
        }
        let view = json(200, &DraftView { draft_id: handle.to_string(), draft: draft.clone() });

        if let Some(manifest) = derived {
            self.record_derived_manifest(ctx, manifest);
        }
        Routed::Reply(view)
    }

    /// The manifest this patch's `base` declares, or the refusal to answer.
    ///
    /// Refuses two things. A `pipeline.toml` that will not decode is named with
    /// its base, because the fallback ADR-0215 rejected would be silent at the
    /// exact moment the tree and the coordinator disagree most. And a patch
    /// whose own registry names a *different* manifest address is refused
    /// rather than overwritten: the operator is asserting a vocabulary the base
    /// does not carry, and quietly replacing it would leave them holding a
    /// draft that reads back as something they did not ask for.
    fn derived_manifest(&self, handle: u64, patch: &DraftPatch) -> Result<Option<DerivedManifest>, HttpServerResponse> {
        let Some(base) = patch.base else {
            return Ok(None);
        };
        let derived = match self.derive_pipeline_manifest(base) {
            Ok(derived) => derived,
            Err(PipelineManifestError::Missing) => {
                return Err(error_response(
                    422,
                    &format!(
                        "base {} carries no `{PIPELINE_MANIFEST_PATH}`; a base must declare its lanes (ADR-0215)",
                        base.to_hex()
                    ),
                ));
            }
            Err(error) => {
                return Err(error_response(
                    422,
                    &format!(
                        "base {} carries a `{PIPELINE_MANIFEST_PATH}` this coordinator cannot read: {error}; draft \
                         formation fails closed",
                        base.to_hex()
                    ),
                ));
            }
        };
        let Some(derived) = derived else {
            // The digest does not resolve to a git object this host can read.
            // That is not a missing file: the coordinator never looked. A
            // `Fact::Seal` still meets the door — a spec that names no
            // manifest is refused there.
            return Ok(None);
        };

        let named = patch.configs.as_ref().map_or_else(
            || self.drafts.get(&handle).and_then(|draft| draft.configs.address::<PipelineManifest>()),
            ConfigRegistry::address::<PipelineManifest>,
        );
        if let Some(named) = named
            && named != derived.address
        {
            return Err(error_response(
                422,
                &format!(
                    "draft seals `{}` at {}, but base {} declares {}; the manifest is derived from the base, never \
                     authored",
                    PipelineManifest::NAME,
                    named.to_hex(),
                    base.to_hex(),
                    derived.address.to_hex(),
                ),
            ));
        }
        Ok(Some(derived))
    }

    /// File the derived manifest's bytes: in this cap's resolved-configuration
    /// cache now, and in the store's durable table on its own chain.
    ///
    /// The cache fill is what makes the `200` honest — the address the patch
    /// hands back resolves through `GET /configs/{digest}` before the reply
    /// lands, and the pre-seal gate reads that same cache. The store write is
    /// fire-and-forget for the reason a dispatch description is (see
    /// [`persist_descriptions`](ApiCapabilityState::persist_descriptions)): a
    /// record *about* a draft must not be able to fail the draft it describes,
    /// and the seal that follows is a separate request with its own round trip.
    /// A write that does fail is loud twice over — the store's `Err` is warned
    /// by [`config_response`](super::configs::config_response), and a bloom
    /// sealed against a row nothing holds is refused by the reducer's own
    /// unproducible-configuration check rather than admitted (ADR-0174).
    fn record_derived_manifest(&mut self, ctx: &NativeCtx<'_, Manual>, manifest: DerivedManifest) {
        let DerivedManifest { address, bytes } = manifest;
        self.configs.insert(address, PipelineManifest::NAME, bytes.clone(), None);
        ctx.actor::<StoreCapability>().send_detached(&RecordConfig {
            digest: address.as_bytes().to_vec(),
            kind: PipelineManifest::NAME.to_owned(),
            bytes,
        });
    }

    /// Render every open draft with its handle.
    pub(super) fn drafts_view(&self) -> DraftsView {
        DraftsView {
            drafts: self
                .drafts
                .iter()
                .map(|(id, draft)| DraftView { draft_id: id.to_string(), draft: draft.clone() })
                .collect(),
        }
    }

    /// Resolve a draft handle to its id + a clone, or the `404` to reply.
    pub(super) fn lookup_draft(&self, id: &str) -> Result<(u64, BloomDraft), HttpServerResponse> {
        parse_draft_id(id)
            .and_then(|handle| self.drafts.get(&handle).map(|draft| (handle, draft.clone())))
            .ok_or_else(|| error_response(404, "no such draft"))
    }
}

/// Parse a draft handle path segment.
fn parse_draft_id(id: &str) -> Option<u64> {
    id.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::parse_draft_id;

    #[test]
    fn parse_draft_id_is_a_u64() {
        assert_eq!(parse_draft_id("7"), Some(7));
        assert_eq!(parse_draft_id("notanid"), None);
    }
}
