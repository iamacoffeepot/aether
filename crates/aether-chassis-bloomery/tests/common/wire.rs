//! Coordinator-facing wire driver: handshake (with retry), cid allocation,
//! typed `call`, view decode, admit, and a tick that drains to `ReplyEnd`.
//!
//! The three scenario harnesses each used to own a copy of this loop. The
//! handshake half retries connect+Hello as one unit (#5193); a private copy
//! that retried TCP only and Hello'd once is how a bind-race stranger used to
//! look like a coordinator bug.

use std::net::TcpStream;
use std::time::Duration;

use aether_actor::Addressable;
use aether_bloomery::{
    Admit, AdmitResult, BloomId, BloomView, Event, Fact, IdempotencyKey, Outcome, Query, QueryResult, ViewDocument,
};
use aether_chassis_bloomery::ControlCore;
use aether_codec::frame::{read_frame, write_frame};
use aether_data::wire::{from_bytes, to_vec};
use aether_data::{Kind, MailboxId};
use aether_rpc::WireFrame;
use serde::Serialize;

use super::client::{call, call_frame, connect_and_handshake};

/// A live RPC session: handshake already done, cids allocated in order.
pub struct Wire {
    stream: TcpStream,
    cid: u64,
}

impl Wire {
    /// Connect and handshake as `client_name`, retrying the pair until a
    /// coordinator answers.
    ///
    /// # Panics
    /// No coordinator answered inside the handshake deadline.
    #[must_use]
    pub fn connect(port: u16, client_name: &str) -> Self {
        Self { stream: connect_and_handshake(port, client_name), cid: 1 }
    }

    /// Take an already-handshaken stream — the spawn-and-connect path hands
    /// one back beside the child guard.
    #[must_use]
    pub fn from_stream(stream: TcpStream) -> Self {
        Self { stream, cid: 1 }
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

    /// The whole projection, right now.
    ///
    /// # Panics
    /// The query was refused or its reply did not decode.
    pub fn view(&mut self) -> ViewDocument {
        let query = Query { bloom: None, release: None, calibration: false };
        match self.call::<_, QueryResult>(control_mailbox(), &query) {
            QueryResult::Document { document } => from_bytes(&document).expect("the projection decodes"),
            other => panic!("expected a document reply, got {other:?}"),
        }
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
pub fn control_mailbox() -> MailboxId {
    <ControlCore as Addressable>::resolve(0, ())
}
