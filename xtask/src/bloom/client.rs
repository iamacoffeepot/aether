//! Coordinator REST verbs. Every request body is a typed serde value.

use aether_bloomery::{BloomSpec, Fact, ScopeRevision, Statement, ViewDocument};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::Endpoint;
use super::dto::{
    ApprovalStoredView, BloomView, CancelCommissionRequest, CommissionCancelledView, CommissionReopenedView,
    CommissionShowView, ConfigRequest, ConfigValueView, ConfigView, DraftPatch, DraftView, JournalEntry, JournalView,
    OutcomeView, ReopenCommissionRequest, RepairRequest, RetryRequest, ReverifyBaseRequest, RevisionEvidence,
    ScopeRevisionWrittenView, SealRequest, SupersedeRequest, SuppressionAnswerRequest, WithdrawRequest,
    WriteRevisionRequest,
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

    pub fn journal(&self) -> Result<JournalView> {
        // Matches the coordinator's `JOURNAL_MAX_LIMIT` (`GET /journal`).
        const JOURNAL_PAGE_LIMIT: u64 = 1000;

        let mut records = Vec::new();
        let mut from_sequence = None;
        let mut total_matched;
        loop {
            let path = from_sequence.map_or_else(
                || format!("/journal?limit={JOURNAL_PAGE_LIMIT}"),
                |from| format!("/journal?limit={JOURNAL_PAGE_LIMIT}&from_sequence={from}"),
            );
            let page: JournalView = self.get(&path).with_context(|| walk_stopped(&records, &path))?;
            if page.truncated && page.next_from_sequence.is_none() {
                bail!("journal page reports more records but no cursor");
            }

            total_matched = page.total_matched;
            let next = page.next_from_sequence.filter(|_| page.truncated);
            records.extend(page.records);
            let Some(next) = next else {
                break;
            };
            from_sequence = Some(next);
        }

        let shown = u64::try_from(records.len()).unwrap_or(u64::MAX);
        Ok(JournalView { records, total_matched, shown, truncated: false, next_from_sequence: None, notice: None })
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

    /// Take one member out of a walking bloom without superseding it (#5327).
    pub fn withdraw(&self, bloom_id: &str, workpiece: &str, request: &WithdrawRequest) -> Result<OutcomeView> {
        self.send("POST", &format!("/blooms/{bloom_id}/members/{workpiece}/withdraw"), request)
    }

    /// Run one member's current stage again on the candidate it holds (#5423).
    pub fn retry(&self, bloom_id: &str, workpiece: &str, request: &RetryRequest) -> Result<OutcomeView> {
        self.send("POST", &format!("/blooms/{bloom_id}/members/{workpiece}/retry"), request)
    }

    /// Run `verify.base` again on a red receipt.
    pub fn reverify_base(&self, base: &str, request: &ReverifyBaseRequest) -> Result<OutcomeView> {
        self.send("POST", &format!("/bases/{base}/reverify"), request)
    }

    /// Hand a wedged member the candidate the operator supplied and let the
    /// ordinary gates judge it (#4957).
    pub fn repair(&self, bloom_id: &str, workpiece: &str, request: &RepairRequest) -> Result<OutcomeView> {
        self.send("POST", &format!("/blooms/{bloom_id}/members/{workpiece}/repair"), request)
    }

    /// Answer the suppression requests a member's candidate is carrying
    /// (ADR-0193 §5).
    pub fn suppression(
        &self,
        bloom_id: &str,
        workpiece: &str,
        request: &SuppressionAnswerRequest,
    ) -> Result<OutcomeView> {
        self.send("POST", &format!("/blooms/{bloom_id}/members/{workpiece}/suppression"), request)
    }

    /// One commission's tip, typed, plus the approvals stored against it.
    pub fn commission(&self, id: &str) -> Result<CommissionShowView> {
        self.get(&format!("/commissions/{id}"))
    }

    /// Run the between-blooms archive pass. A `409` refusal exits as an error
    /// so a scripted operator run does not read a between-blooms block as
    /// success.
    pub fn archive_pass(&self) -> Result<super::dto::ArchivePassView> {
        http::json(self.endpoint, "POST", "/archive", None::<&()>)
    }

    /// List the records currently on the archive tier.
    pub fn list_archive(&self) -> Result<super::dto::ArchiveListView> {
        self.get("/archive")
    }

    /// Write `revision` as the commission's next scope revision, with sidecar
    /// evidence about it. The revision's bytes stay the signed subject.
    ///
    /// Serializes the typed value rather than a rendering: the REST edge
    /// accepts a digest as either hex or the canonical byte array, so the
    /// successor's stored bytes are exactly what the widening produced.
    pub fn write_revision(
        &self,
        id: &str,
        revision: &ScopeRevision,
        evidence: &RevisionEvidence,
    ) -> Result<ScopeRevisionWrittenView> {
        self.send(
            "POST",
            &format!("/commissions/{id}/revisions"),
            &WriteRevisionRequest { revision: revision.clone(), evidence: evidence.clone() },
        )
    }

    /// Submit `statement` as an approval of the commission's current revision.
    pub fn approve(&self, id: &str, statement: &Statement) -> Result<ApprovalStoredView> {
        self.send("POST", &format!("/commissions/{id}/approvals"), statement)
    }

    /// Close an open commission with a signed cancel envelope.
    pub fn cancel(&self, id: &str, request: &CancelCommissionRequest) -> Result<CommissionCancelledView> {
        self.send("POST", &format!("/commissions/{id}/cancel"), request)
    }

    /// Put a landed commission back in the line with a signed reopen envelope.
    pub fn reopen(&self, id: &str, request: &ReopenCommissionRequest) -> Result<CommissionReopenedView> {
        self.send("POST", &format!("/commissions/{id}/reopen"), request)
    }

    /// A stored configuration, decoded through its kind's schema.
    pub fn config(&self, digest: &str) -> Result<ConfigValueView> {
        self.get(&format!("/configs/{digest}"))
    }

    /// The sealed spec that minted `bloom_id`, recovered from the journal.
    ///
    /// The live projection names members and status but not the bloom-wide
    /// registry, so supersede reads the journal to reuse configs by digest.
    pub fn spec_for(&self, bloom_id: &str) -> Result<BloomSpec> {
        let journal = self.journal()?;
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

fn walk_stopped(records: &[JournalEntry], path: &str) -> String {
    records.last().map_or_else(
        || format!("journal walk stopped at {path}"),
        |entry| format!("journal walk stopped at sequence {}", entry.sequence),
    )
}

fn spec_in_fact(fact: &Fact) -> Option<BloomSpec> {
    match fact {
        Fact::Seal(spec) | Fact::GraphSeal { spec, .. } => Some(spec.clone()),
        Fact::Supersede { successor, .. } => Some(successor.clone()),
        _ => None,
    }
}

/// The bloom in `view` whose id is `bloom_id`.
pub fn bloom_in<'a>(view: &'a ViewDocument, bloom_id: &str) -> Result<&'a BloomView> {
    view.blooms
        .iter()
        .find(|bloom| bloom.id.to_string() == bloom_id)
        .with_context(|| format!("no bloom {bloom_id} in the live view"))
}

#[cfg(test)]
mod tests {
    use std::io::{self, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::Client;
    use crate::bloom::Endpoint;

    #[derive(Clone, Debug)]
    struct Recorded {
        method: String,
        path: String,
    }

    fn page(sequence: u64, truncated: bool, next: Option<u64>) -> Value {
        let bloom = "0".repeat(64);
        let head = bloom.clone();
        json!({
            "records": [{
                "sequence": sequence,
                "idempotency_key": "k",
                "event": {
                    "idempotency_key": "k",
                    "fact": { "Land": { "bloom": bloom, "new_head": head } }
                },
                "outcome": "Duplicate",
                "decider": "test"
            }],
            "total_matched": 3,
            "shown": 1,
            "truncated": truncated,
            "next_from_sequence": next,
        })
    }

    fn from_sequence(path: &str) -> Option<u64> {
        let query = path.split_once('?')?.1;
        query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key == "from_sequence").then_some(value)?.parse().ok()
        })
    }

    #[test]
    fn journal_walks_three_pages_in_order() {
        // A client that stops after the first page would return only n=3.
        let (journal, log) = with_fake(
            |request| match (request.method.as_str(), from_sequence(&request.path)) {
                ("GET", None) => (200, page(3, true, Some(3))),
                ("GET", Some(3)) => (200, page(2, true, Some(2))),
                ("GET", Some(2)) => (200, page(1, false, None)),
                _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
            },
            |port| {
                Client::new(&Endpoint { host: "127.0.0.1".to_owned(), port, token: None }).journal().expect("full walk")
            },
        );

        let sequences: Vec<_> = journal.records.iter().map(|record| record.sequence).collect();
        assert_eq!(sequences, vec![3, 2, 1]);
        assert!(!journal.truncated);
        assert_eq!(journal.shown, 3);
        assert_eq!(journal.total_matched, 3);
        assert_eq!(journal.next_from_sequence, None);
        assert_eq!(
            log.iter().map(|entry| entry.path.as_str()).collect::<Vec<_>>(),
            vec!["/journal?limit=1000", "/journal?limit=1000&from_sequence=3", "/journal?limit=1000&from_sequence=2",]
        );
    }

    #[test]
    fn journal_refuses_a_truncated_page_with_no_cursor() {
        // Returning the one page would silently drop the rest of the journal.
        let error = with_fake(
            |request| match request.method.as_str() {
                "GET" => (200, page(3, true, None)),
                _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
            },
            |port| {
                Client::new(&Endpoint { host: "127.0.0.1".to_owned(), port, token: None })
                    .journal()
                    .expect_err("truncated without cursor")
            },
        )
        .0;
        assert!(
            error.to_string().contains("journal page reports more records but no cursor"),
            "silent short read: {error}"
        );
    }

    fn serve_one(mut stream: TcpStream, handler: &impl Fn(&Recorded) -> (u16, Value), log: &Mutex<Vec<Recorded>>) {
        stream.set_read_timeout(Some(Duration::from_secs(2))).expect("read timeout");
        let mut buf = Vec::new();
        loop {
            let mut chunk = [0_u8; 1024];
            let n = match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => n,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock || err.kind() == io::ErrorKind::TimedOut => break,
                Err(_) => break,
            };
            buf.extend_from_slice(&chunk[..n]);
            let Some(head_end) = buf.windows(4).position(|window| window == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&buf[..head_end]);
            let mut parts = head.split_whitespace();
            let method = parts.next().unwrap_or("").to_owned();
            let path = parts.next().unwrap_or("").to_owned();
            let request = Recorded { method, path };
            log.lock().expect("log").push(request.clone());
            let (status, reply) = handler(&request);
            let payload = serde_json::to_vec(&reply).expect("encode reply");
            let head = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&payload);
            break;
        }
    }

    fn with_fake<H, T>(handler: H, body: impl FnOnce(u16) -> T) -> (T, Vec<Recorded>)
    where
        H: Fn(&Recorded) -> (u16, Value) + Send + Sync,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake coordinator");
        listener.set_nonblocking(true).expect("nonblocking accept");
        let port = listener.local_addr().expect("local addr").port();
        let log = Mutex::new(Vec::new());
        let stop = AtomicBool::new(false);
        let result = thread::scope(|scope| {
            scope.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => serve_one(stream, &handler, &log),
                        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            let result = body(port);
            stop.store(true, Ordering::Relaxed);
            result
        });
        (result, log.into_inner().expect("log"))
    }
}
