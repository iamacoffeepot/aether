//! The draft shaping routes — open, read, list, and patch. Pre-seal shaping
//! "claims nothing" (ADR-0149 §The bloom), so a draft lives entirely in the
//! router's `drafts` map under a monotonic per-process handle and every route
//! here answers synchronously. Sealing that draft is the next module over.

use aether_bloomery::BloomDraft;
use aether_http::HttpServerResponse;

use super::response::{error_response, json};
use super::state::{ApiCapabilityState, MAX_OPEN_DRAFTS, Routed};
use crate::api::dto::{DraftPatch, DraftView, DraftsView};

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
    pub(super) fn patch_draft(&mut self, id: &str, body: &[u8]) -> Routed {
        let handle = match parse_draft_id(id) {
            Some(handle) if self.drafts.contains_key(&handle) => handle,
            _ => return Routed::Reply(error_response(404, "no such draft")),
        };
        let patch: DraftPatch = match serde_json::from_slice(body) {
            Ok(patch) => patch,
            Err(error) => return Routed::Reply(error_response(400, &format!("invalid draft patch: {error}"))),
        };
        let draft = self.drafts.get_mut(&handle).expect("draft presence checked above");
        if let Some(proposals) = patch.proposals {
            draft.proposals = proposals;
        }
        if let Some(base) = patch.base {
            draft.base = base;
        }
        if let Some(budget) = patch.budget {
            draft.budget = budget;
        }
        if let Some(forecast) = patch.forecast {
            draft.forecast = forecast;
        }
        Routed::Reply(json(200, &DraftView { draft_id: handle.to_string(), draft: draft.clone() }))
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
