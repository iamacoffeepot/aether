//! Coordinator REST verbs. Every request body is a typed serde value.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::Endpoint;
use super::dto::{
    BloomSpec, BloomView, ConfigRequest, ConfigView, DraftPatch, DraftView, JournalView, OutcomeView, SealRequest,
    SupersedeRequest, ViewDocument,
};
use super::http;
use super::plan::spec_id;

/// Thin client over one coordinator.
pub struct Client<'a> {
    endpoint: &'a Endpoint,
}

impl<'a> Client<'a> {
    pub fn new(endpoint: &'a Endpoint) -> Self {
        Self { endpoint }
    }

    pub fn view(&self) -> Result<ViewDocument> {
        self.get("/view")
    }

    pub fn open_draft(&self) -> Result<DraftView> {
        http::json(self.endpoint, "POST", "/drafts", None::<&()>)
    }

    pub fn patch_draft(&self, draft_id: &str, patch: &DraftPatch) -> Result<DraftView> {
        self.send("PATCH", &format!("/drafts/{draft_id}"), patch)
    }

    pub fn author_config(&self, kind: &str, value: &Value) -> Result<ConfigView> {
        self.send("POST", "/configs", &ConfigRequest { kind, value })
    }

    pub fn seal(&self, draft_id: &str, request: &SealRequest) -> Result<OutcomeView> {
        self.send("POST", &format!("/drafts/{draft_id}/seal"), request)
    }

    pub fn supersede(&self, bloom_id: &str, request: &SupersedeRequest) -> Result<OutcomeView> {
        self.send("POST", &format!("/blooms/{bloom_id}/supersede"), request)
    }

    /// The sealed spec that minted `bloom_id`, recovered from the journal.
    ///
    /// The live projection names members and status but not the bloom-wide
    /// registry, so supersede reads the journal to reuse configs by digest.
    pub fn spec_for(&self, bloom_id: &str) -> Result<BloomSpec> {
        let journal: JournalView = self.get("/journal")?;
        for record in journal.records.into_iter().rev() {
            if let Some(spec) = spec_in_fact(&record.event.fact)
                && spec_id(&spec).as_hex() == bloom_id
            {
                return Ok(spec);
            }
        }
        bail!("journal has no sealed spec for bloom {bloom_id}")
    }

    fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        http::json(self.endpoint, "GET", path, None::<&()>)
    }

    fn send<T: Serialize, R: DeserializeOwned>(&self, method: &str, path: &str, body: &T) -> Result<R> {
        http::json(self.endpoint, method, path, Some(body))
    }
}

fn spec_in_fact(fact: &Value) -> Option<BloomSpec> {
    let spec = fact
        .get("Seal")
        .or_else(|| fact.get("Supersede").and_then(|body| body.get("successor")))
        .or_else(|| fact.get("GraphSeal").and_then(|body| body.get("spec")))?;
    serde_json::from_value(spec.clone()).ok()
}

/// The bloom in `view` whose id is `bloom_id`.
pub fn bloom_in<'a>(view: &'a ViewDocument, bloom_id: &str) -> Result<&'a BloomView> {
    view.blooms
        .iter()
        .find(|bloom| bloom.id.as_hex() == bloom_id)
        .with_context(|| format!("no bloom {bloom_id} in the live view"))
}
