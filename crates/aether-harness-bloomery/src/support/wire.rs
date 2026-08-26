//! Coordinator-facing wire driver: handshake (with retry), cid allocation,
//! typed `call`, view decode, admit, and a tick that drains to `ReplyEnd`.
//!
//! The three scenario harnesses each used to own a copy of this loop. The
//! handshake half retries connect+Hello as one unit (#5193); a private copy
//! that retried TCP only and Hello'd once is how a bind-race stranger used to
//! look like a coordinator bug.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::thread;
use std::time::{Duration, Instant};

use aether_actor::Addressable;
use aether_bloomery::{
    Admit, AdmitResult, BloomId, BloomView, Event, Fact, IdempotencyKey, Outcome, Query, QueryResult, QuerySelector,
    ViewDocument,
};
use aether_chassis_bloomery::ControlCore;
use aether_chassis_bloomery::bloomery::DoctorReport;
use aether_chassis_bloomery::control::PRE_REPLAY_REFUSAL;
use aether_codec::frame::{read_frame, write_frame};
use aether_data::wire::{from_bytes, to_vec};
use aether_data::{Kind, MailboxId};
use aether_rpc::WireFrame;
use serde::Serialize;

use super::client::{call, call_frame, connect_and_handshake};

/// How long [`Wire::await_replayed`] gives the coordinator's boot journal
/// replay. Generous against a journal a restart scenario has already filled,
/// and far short of a hang: an exhausted budget is a panic naming the read the
/// coordinator is still refusing.
const REPLAY_BUDGET: Duration = Duration::from_secs(30);

/// Between re-probes of the projection while the replay is still folding.
const REPLAY_POLL: Duration = Duration::from_millis(20);

/// A live RPC session: handshake already done, cids allocated in order.
pub struct Wire {
    stream: TcpStream,
    cid: u64,
    http_port: Option<u16>,
}

impl Wire {
    /// Connect and handshake as `client_name`, retrying the pair until a
    /// coordinator answers.
    ///
    /// # Panics
    /// No coordinator answered inside the handshake deadline.
    #[must_use]
    pub fn connect(port: u16, client_name: &str) -> Self {
        Self { stream: connect_and_handshake(port, client_name), cid: 1, http_port: None }
    }

    /// Take an already-handshaken stream — the spawn-and-connect path hands
    /// one back beside the child guard.
    #[must_use]
    pub fn from_stream(stream: TcpStream) -> Self {
        Self { stream, cid: 1, http_port: None }
    }

    /// Bind the REST port `GET /view` answers on, so [`doctor`](Self::doctor)
    /// can read the overlay production serves.
    pub fn set_http_port(&mut self, port: u16) {
        self.http_port = Some(port);
    }

    /// Widen the socket read timeout past a scenario's step budget, so a slow
    /// tick reports the budget rather than an io timeout.
    ///
    /// # Panics
    /// The socket refused the timeout.
    pub fn set_read_timeout(&self, timeout: Duration) {
        self.stream.set_read_timeout(Some(timeout)).expect("the fixture socket takes a read timeout");
    }

    /// Issue one typed `Call` to `mailbox` and decode its reply, allocating the
    /// next cid.
    ///
    /// # Panics
    /// As [`call`].
    pub fn call<Req, Reply>(&mut self, mailbox: MailboxId, request: &Req) -> Reply
    where
        Req: Kind + Serialize,
        Reply: Kind,
    {
        self.cid += 1;
        call(&mut self.stream, self.cid, mailbox, request)
    }

    /// Wait for the coordinator's boot journal replay to fold, so every later
    /// read on this wire is served rather than refused.
    ///
    /// The control core refuses every read until its snapshot is the one the
    /// journal describes — deliberately, because an empty projection served
    /// during that window is indistinguishable from a genuinely quiet fleet.
    /// The refusal is therefore not a fault to assert into but a state to await,
    /// the way a substrate scenario polls for an effect no causal chain of its
    /// own can settle: the flag never goes back, so one await is what makes the
    /// window unobservable for every read that follows on this wire.
    ///
    /// # Panics
    /// The replay did not finish inside `REPLAY_BUDGET`, or a read was
    /// refused for some other reason.
    pub fn await_replayed(&mut self) {
        let deadline = Instant::now() + REPLAY_BUDGET;
        loop {
            match self.read_document() {
                Ok(_) => return,
                Err(refusal) => assert!(
                    refusal.contains(PRE_REPLAY_REFUSAL),
                    "the projection read was refused for a reason that is not the boot replay: {refusal}"
                ),
            }
            assert!(
                Instant::now() < deadline,
                "the coordinator's boot journal replay did not finish inside {REPLAY_BUDGET:?}; \
                 every read stays refused until the projection is the one the journal describes"
            );
            thread::sleep(REPLAY_POLL);
        }
    }

    /// The whole projection, right now.
    ///
    /// Strict about a refusal, including the boot-replay one: the harness awaits
    /// the replay once at boot, so a scenario step that meets that window is
    /// reading over a wire nobody waited on rather than watching a coordinator
    /// misbehave, and the panic says so.
    ///
    /// # Panics
    /// The query was refused or its reply did not decode.
    pub fn view(&mut self) -> ViewDocument {
        match self.read_document() {
            Ok(document) => document,
            Err(refusal) if refusal.contains(PRE_REPLAY_REFUSAL) => {
                panic!("this wire read the projection without awaiting the boot journal replay: {refusal}")
            }
            Err(refusal) => panic!("the projection read was refused: {refusal}"),
        }
    }

    /// One document read: the decoded projection, or the control core's refusal.
    ///
    /// # Panics
    /// The reply was neither a document nor a refusal, or it did not decode.
    fn read_document(&mut self) -> Result<ViewDocument, String> {
        let query = Query { selector: QuerySelector::Document };
        match self.call::<_, QueryResult>(control_mailbox(), &query) {
            QueryResult::Document { document } => Ok(from_bytes(&document).expect("the projection decodes")),
            QueryResult::Err { error } => Err(error),
            other => panic!("expected a document reply, got {other:?}"),
        }
    }

    /// The doctor's latest pass, as `GET /view` overlays it.
    ///
    /// `None` when the REST port was never bound, the ingress has not
    /// answered yet, or this pass has not published a report.
    #[must_use]
    pub fn doctor(&self) -> Option<DoctorReport> {
        let port = self.http_port?;
        let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
        let request = format!("GET /view HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).ok()?;
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).ok()?;
        let text = String::from_utf8_lossy(&bytes);
        let body = text.split("\r\n\r\n").nth(1)?;
        let value: serde_json::Value = serde_json::from_str(body).ok()?;
        value.get("doctor").cloned().and_then(|doctor| serde_json::from_value(doctor).ok())
    }

    /// One bloom's view.
    ///
    /// # Panics
    /// The projection holds no such bloom.
    pub fn bloom(&mut self, bloom: BloomId) -> BloomView {
        self.view()
            .blooms
            .into_iter()
            .find(|view| view.id == bloom)
            .unwrap_or_else(|| panic!("the projection holds no bloom {bloom:?}"))
    }

    /// Admit one reducer fact through the control core's wire ingress.
    ///
    /// # Panics
    /// The control core refused the admit, or its outcome did not decode.
    pub fn admit(&mut self, key: &str, fact: Fact) -> Outcome {
        let event = Event { idempotency_key: IdempotencyKey(key.to_owned()), fact };
        let admit = Admit { event: to_vec(&event).expect("a reducer event encodes") };
        match self.call::<_, AdmitResult>(control_mailbox(), &admit) {
            AdmitResult::Ok { outcome } => from_bytes::<Outcome>(&outcome).expect("the outcome decodes"),
            AdmitResult::Err { error } => panic!("the admit was refused: {error}"),
        }
    }

    /// Dispatch one reactor's tick and wait for its causal chain to settle.
    /// A tick carries no reply, so this drains to `ReplyEnd`.
    ///
    /// # Panics
    /// The stream faulted, carried a foreign cid, or ended in an error.
    pub fn tick<K: Kind + Serialize>(&mut self, mailbox: MailboxId, wake: &K) {
        self.cid += 1;
        write_frame(&mut self.stream, &call_frame(self.cid, mailbox, wake))
            .expect("the tick reaches the coordinator's RPC ingress");
        loop {
            match read_frame(&mut self.stream).expect("the coordinator answers the tick") {
                WireFrame::ReplyEvent { cid, .. } => assert_eq!(cid, self.cid, "ReplyEvent cid mismatch"),
                WireFrame::ReplyEnd { cid, result } => {
                    assert_eq!(cid, self.cid, "ReplyEnd cid mismatch");
                    result.expect("the tick's causal chain settled without a fault");
                    return;
                }
                other => panic!("unexpected frame for tick {}: {other:?}", self.cid),
            }
        }
    }
}

/// The native control core's mailbox, resolved through its own addressing
/// identity — the same way the reactors resolve it.
#[must_use]
pub fn control_mailbox() -> MailboxId {
    <ControlCore as Addressable>::resolve(0, ())
}
