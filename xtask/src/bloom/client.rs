//! Coordinator REST verbs. Every request body is a typed serde value.

use aether_bloomery::{BloomSpec, Fact, ScopeRevision, Statement, ViewDocument};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::Endpoint;
use super::dto::{
    AdminCancelLaneRequest, AdminDropLapRequest, AdminRerunRequest, AdminSessionRequest, AdminSetCandidateRequest,
    AdminWaiveRequest, ApprovalStoredView, BloomView, CancelCommissionRequest, CancelOrderRequest, CancelOrderView,
    CommissionCancelledView, CommissionReopenedView, CommissionShowView, ConfigRequest, ConfigValueView, ConfigView,
    DraftPatch, DraftView, JournalEntry, JournalView, LiveOrderView, OutcomeView, ProposeRequest,
    ReopenCommissionRequest, RepairRequest, RetryRequest, ReverifyBaseRequest, RevisionEvidence,
    ScopeRevisionWrittenView, ScopeRunOpenedView, ScopeRunRequest, SealRequest, SupersedeRequest,
    SuppressionAnswerRequest, WithdrawRequest, WriteRevisionRequest,
};
use super::plan::spec_id;
use super::{hex, http};

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

    /// The live projection plus the outstanding orders `GET /view` renders
    /// beside it (ADR-0219).
    ///
    /// Two deserializations of one body rather than one flattened type: the
    /// orders are not a [`ViewDocument`] field — that document is wire-encoded
    /// into the outbox, where a trailing optional would break queued payloads —
    /// so the route flattens them in beside it. Reading the body once as
    /// [`Value`] and shaping it twice keeps this client honest about that
    /// without teaching the projection a field the coordinator does not have.
    pub fn live_view(&self) -> Result<(ViewDocument, Vec<LiveOrderView>)> {
        split_live_view(self.get("/view")?)
    }

    /// Open or close one bloom's admin session (ADR-0219). `edge` is `enter` or
    /// `exit`, which are the same body at two doors.
    pub fn admin_session(&self, bloom_id: &str, edge: &str, request: &AdminSessionRequest) -> Result<OutcomeView> {
        self.send("POST", &format!("/blooms/{bloom_id}/admin/{edge}"), request)
    }

    /// Cancel one running dispatch from inside admin mode (ADR-0219).
    pub fn admin_cancel_lane(&self, bloom_id: &str, request: &AdminCancelLaneRequest) -> Result<OutcomeView> {
        self.send("POST", &format!("/blooms/{bloom_id}/admin/cancel-lane"), request)
    }

    /// Hand a workpiece a candidate from inside admin mode (ADR-0219).
    pub fn admin_set_candidate(&self, bloom_id: &str, request: &AdminSetCandidateRequest) -> Result<OutcomeView> {
        self.send("POST", &format!("/blooms/{bloom_id}/admin/set-candidate"), request)
    }

    /// Run one stage again from inside admin mode (ADR-0219).
    pub fn admin_rerun(&self, bloom_id: &str, request: &AdminRerunRequest) -> Result<OutcomeView> {
        self.send("POST", &format!("/blooms/{bloom_id}/admin/rerun"), request)
    }

    /// Void a red verdict's findings from inside admin mode (ADR-0219).
    pub fn admin_waive(&self, bloom_id: &str, request: &AdminWaiveRequest) -> Result<OutcomeView> {
        self.send("POST", &format!("/blooms/{bloom_id}/admin/waive"), request)
    }

    /// Discard a completed lap's candidate from inside admin mode (ADR-0219).
    pub fn admin_drop_lap(&self, bloom_id: &str, request: &AdminDropLapRequest) -> Result<OutcomeView> {
        self.send("POST", &format!("/blooms/{bloom_id}/admin/drop-lap"), request)
    }

    /// Drop one outstanding order from the board without faulting its lane.
    pub fn cancel_order(&self, nonce: &str, request: &CancelOrderRequest) -> Result<CancelOrderView> {
        self.send("POST", &format!("/orders/{nonce}/cancel"), request)
    }

    /// Propose a signed operator change onto the day's branch (ADR-0205).
    pub fn propose(&self, request: &ProposeRequest) -> Result<OutcomeView> {
        self.send("POST", "/proposals", request)
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

    /// Open a pre-bloom scoping run on a commission.
    pub fn scope_run(&self, id: &str, request: &ScopeRunRequest) -> Result<ScopeRunOpenedView> {
        self.send("POST", &format!("/commissions/{id}/scope-runs"), request)
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

/// Split one `GET /view` body into the projection and the orders flattened
/// beside it.
///
/// Both halves decode through [`hex`] — the REST edge's own body codec — and
/// not through `serde_json` directly. The coordinator renders every digest as
/// 64 hex characters while [`Digest`](aether_bloomery::Digest)'s own
/// `Deserialize` expects the canonical 32-byte array, so the raw codec refuses
/// a live body at the first digest it reaches. That is the codec every other
/// verb here already reaches the coordinator through, by way of
/// [`http::json`]; this is the one reader that holds the parsed [`Value`]
/// first, which is exactly the entry point `hex::from_value` exists for.
///
/// The orders are taken out of the body before the document decodes, so the
/// document is not cloned to read them. What it leaves behind is a null at a
/// key `ViewDocument` does not declare, which it ignores the same way it
/// ignores the route's other flattened siblings.
fn split_live_view(mut body: Value) -> Result<(ViewDocument, Vec<LiveOrderView>)> {
    let orders = body
        .get_mut("orders")
        .map(Value::take)
        .map_or_else(|| Ok(Vec::new()), hex::from_value)
        .context("decode the live view's outstanding orders")?;

    Ok((hex::from_value(body).context("decode the live view document")?, orders))
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
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::panic::{self, AssertUnwindSafe};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use aether_bloomery::{HTTP_READ_TIMEOUT, StageId};
    use serde_json::{Value, json};

    use super::{Client, split_live_view};
    use crate::bloom::Endpoint;

    #[test]
    fn the_live_view_decodes_the_coordinators_hex_digests() {
        // The live defect (ADR-0219, 2026-09-15): `admin status` refused every
        // real `/view` with `invalid type: string "0140bd…", expected an array
        // of length 32`. The coordinator renders each digest as 64 hex
        // characters, while `Digest`'s own `Deserialize` expects the canonical
        // 32-byte array — so a body decoded through `serde_json` rather than
        // the REST edge's codec dies at the first digest it reaches, which is
        // the document's own `mainline`, before any bloom is even looked at.
        let mainline = "01".repeat(32);
        let bloom = "ab".repeat(32);
        let body = json!({
            "mainline": mainline,
            "observed": mainline,
            "spend_quiesce": null,
            "blooms": [],
            "base_alert": null,
            "orders": [{
                "nonce": "dispatch-7265",
                "bloom": bloom,
                "workpiece": "aether.bloomery.composition",
                "stage": "Refine",
            }],
        });

        let (document, orders) = split_live_view(body).expect("the live hex spelling decodes");

        assert_eq!(document.mainline.to_hex(), mainline);
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].bloom.to_hex(), bloom, "an order's digest takes the same spelling");
        assert_eq!(orders[0].stage, StageId::Refine, "and its stage arrives as the name the board prints");
    }

    #[test]
    fn an_idle_view_omits_orders_and_still_decodes() {
        // The route drops `orders` entirely when nothing is running
        // (`skip_serializing_if = "Vec::is_empty"`), so a reader that required
        // the key would refuse every quiet coordinator — the state an operator
        // is most likely to be reading `admin status` in.
        let zero = "00".repeat(32);
        let body = json!({
            "mainline": zero,
            "observed": zero,
            "spend_quiesce": null,
            "blooms": [],
            "base_alert": null,
        });

        let (_, orders) = split_live_view(body).expect("an idle view decodes");

        assert!(orders.is_empty());
    }

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

    #[test]
    fn journal_fixture_resumes_a_body_panic_after_a_request() {
        // A panic in the client body must shut the accept loop down and resume the same panic.
        const DISTINCTIVE: &str = "issue-5498 distinctive journal fixture panic";
        let panicked = panic::catch_unwind(AssertUnwindSafe(|| {
            with_fake(
                |request| match (request.method.as_str(), from_sequence(&request.path)) {
                    ("GET", None) => (200, page(1, false, None)),
                    _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
                },
                |port| {
                    let journal = Client::new(&Endpoint { host: "127.0.0.1".to_owned(), port, token: None })
                        .journal()
                        .expect("served request before panic");
                    assert_eq!(journal.records.iter().map(|record| record.sequence).collect::<Vec<_>>(), vec![1]);
                    assert!(!journal.truncated);
                    assert_eq!(journal.shown, 1);
                    assert_eq!(journal.total_matched, 3);
                    assert_eq!(journal.next_from_sequence, None);
                    panic::panic_any(DISTINCTIVE);
                },
            )
        }));
        let payload = panicked.expect_err("body panic must propagate");
        let message = payload
            .downcast_ref::<&str>()
            .copied()
            .map(str::to_owned)
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .expect("string panic payload");
        assert_eq!(message, DISTINCTIVE);
    }

    #[test]
    fn journal_fixture_serves_a_request_delayed_past_the_old_read_cutoff() {
        // A scheduled client that pauses past the old 2s read cutoff must still be served.
        const RETIRED_READ_TIMEOUT: Duration = Duration::from_secs(2);
        let log = Mutex::new(Vec::new());
        let (armed_tx, armed_rx) = mpsc::sync_channel(1);
        let reply = page(7, false, None);
        let served = reply.clone();

        run_listener(
            |stream| {
                serve_one_when_armed(
                    stream,
                    &|request| match (request.method.as_str(), request.path.as_str()) {
                        ("GET", "/journal?limit=1000") => (200, served.clone()),
                        _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
                    },
                    &log,
                    || {
                        let _ = armed_tx.try_send(());
                    },
                );
            },
            |port| {
                let mut client =
                    TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), HTTP_READ_TIMEOUT)
                        .expect("connect delayed client");
                client.set_nonblocking(false).expect("blocking delayed client");
                client.set_read_timeout(Some(HTTP_READ_TIMEOUT)).expect("delayed client read timeout");
                client.set_write_timeout(Some(HTTP_READ_TIMEOUT)).expect("delayed client write timeout");
                armed_rx.recv_timeout(HTTP_READ_TIMEOUT).expect("fixture armed");
                thread::sleep(RETIRED_READ_TIMEOUT + Duration::from_millis(500));
                client
                    .write_all(b"GET /journal?limit=1000 HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
                    .expect("write delayed request");
                client.flush().expect("flush delayed request");
                let mut response = Vec::new();
                client.read_to_end(&mut response).expect("read delayed response");

                let payload = serde_json::to_vec(&reply).expect("encode reply");
                let mut expected = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                )
                .into_bytes();
                expected.extend_from_slice(&payload);
                assert_eq!(response, expected);
            },
        );

        assert_eq!(
            log.into_inner()
                .expect("log")
                .iter()
                .map(|entry| (entry.method.as_str(), entry.path.as_str()))
                .collect::<Vec<_>>(),
            vec![("GET", "/journal?limit=1000")]
        );
    }

    fn serve_one(stream: TcpStream, handler: &impl Fn(&Recorded) -> (u16, Value), log: &Mutex<Vec<Recorded>>) {
        serve_one_when_armed(stream, handler, log, || {});
    }

    fn serve_one_when_armed(
        mut stream: TcpStream,
        handler: &impl Fn(&Recorded) -> (u16, Value),
        log: &Mutex<Vec<Recorded>>,
        on_armed: impl FnOnce(),
    ) {
        stream.set_nonblocking(false).expect("blocking accepted socket");
        stream.set_read_timeout(Some(HTTP_READ_TIMEOUT)).expect("read timeout");
        stream.set_write_timeout(Some(HTTP_READ_TIMEOUT)).expect("write timeout");
        on_armed();

        let mut buf = Vec::new();
        loop {
            let mut chunk = [0_u8; 1024];
            let n = match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => n,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
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
        let log = Mutex::new(Vec::new());
        let result = run_listener(|stream| serve_one(stream, &handler, &log), body);
        (result, log.into_inner().expect("log"))
    }

    fn run_listener<T>(serve: impl Fn(TcpStream) + Send + Sync, body: impl FnOnce(u16) -> T) -> T {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake coordinator");
        listener.set_nonblocking(true).expect("nonblocking accept");
        let port = listener.local_addr().expect("local addr").port();
        let stop = AtomicBool::new(false);
        let result = thread::scope(|scope| {
            scope.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => serve(stream),
                        Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            // A panic inside `body` must still flip `stop`: otherwise the
            // accept loop never leaves and `thread::scope` waits out the
            // nextest slow-timeout instead of reporting the panic.
            let result = panic::catch_unwind(AssertUnwindSafe(|| body(port)));
            stop.store(true, Ordering::Relaxed);
            result
        });
        result.unwrap_or_else(|payload| panic::resume_unwind(payload))
    }
}
