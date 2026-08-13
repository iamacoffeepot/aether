//! Coordinator REST operations the bloom commands compose.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::bloom::http;

/// A localhost coordinator the commands drive.
pub(super) struct Coordinator {
    pub(super) port: u16,
}

impl Coordinator {
    pub(super) fn view(&self) -> Result<Value> {
        self.get("/view")
    }

    pub(super) fn journal(&self) -> Result<Value> {
        self.get("/journal")
    }

    /// Author `value` as `kind` and return the content address to name in a registry.
    pub(super) fn author_config(&self, kind: &str, value: &Value) -> Result<String> {
        let reply = self.send("POST", "/configs", &json!({ "kind": kind, "value": value }))?;
        reply
            .get("digest")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .with_context(|| format!("POST /configs for {kind} did not return a digest"))
    }

    pub(super) fn stage_workpiece(&self, id: &str, intent: &str, scope_revision: &str) -> Result<()> {
        self.send("POST", "/workpieces", &json!({ "id": id, "intent": intent, "scope_revision": scope_revision }))?;
        Ok(())
    }

    pub(super) fn open_draft(&self) -> Result<String> {
        let opened = http::json(self.port, "POST", "/drafts", None)?;
        opened
            .get("draft_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .context("POST /drafts did not return a draft_id")
    }

    pub(super) fn patch_draft(&self, draft_id: &str, patch: &Value) -> Result<Value> {
        self.send("PATCH", &format!("/drafts/{draft_id}"), patch)
    }

    pub(super) fn seal(&self, draft_id: &str, body: &Value) -> Result<Value> {
        self.send("POST", &format!("/drafts/{draft_id}/seal"), body)
    }

    pub(super) fn supersede(&self, bloom_id: &str, body: &Value) -> Result<Value> {
        self.send("POST", &format!("/blooms/{bloom_id}/supersede"), body)
    }

    fn get(&self, path: &str) -> Result<Value> {
        http::json(self.port, "GET", path, None)
    }

    fn send(&self, method: &str, path: &str, body: &Value) -> Result<Value> {
        let bytes = serde_json::to_vec(body).context("encode request body")?;
        http::json(self.port, method, path, Some(&bytes))
    }
}

/// Find `bloom_id` in the live view document.
pub(super) fn bloom_in_view<'a>(view: &'a Value, bloom_id: &str) -> Result<&'a Value> {
    let blooms = view.get("blooms").and_then(Value::as_array).context("view document is missing blooms")?;
    blooms
        .iter()
        .find(|bloom| bloom.get("id").and_then(Value::as_str).is_some_and(|id| id.eq_ignore_ascii_case(bloom_id)))
        .with_context(|| format!("no bloom {bloom_id} in the live view"))
}

/// Render a write-route outcome the way `status` renders a bloom id.
pub(super) fn render_outcome(outcome: &Value) -> Result<String> {
    let outcome = outcome.get("outcome").unwrap_or(outcome);
    if let Some(id) = outcome.get("Sealed").and_then(Value::as_str) {
        return Ok(format!("sealed  {id}"));
    }
    if let Some(pair) = outcome.get("Superseded") {
        let predecessor = pair.get("predecessor").and_then(Value::as_str).unwrap_or("?");
        let successor = pair.get("successor").and_then(Value::as_str).unwrap_or("?");
        return Ok(format!("superseded  {predecessor}  →  {successor}"));
    }
    if outcome.get("SealRejected").is_some() || outcome.get("SupersedeRejected").is_some() {
        bail!("coordinator refused: {outcome}");
    }
    Ok(format!("outcome  {outcome}"))
}
