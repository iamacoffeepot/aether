//! Two blocking HTTP lanes off the event loop.
//!
//! `live` uses a 1 s timeout for `/view`. `bulk` uses 10 s for later large
//! reads. Both run on a `thread::scope` the shell's caller owns. The shell
//! sends requests and drains replies with `try_recv`.

use std::iter;
use std::sync::mpsc::{self, Receiver, RecvError, SendError, Sender};
use std::thread;
use std::time::Duration;

use crate::dto::{DecodedArtifact, JournalPage, ViewDocument};
use crate::http::{self, Endpoint};
use crate::store::{Lane, ResourceKey};

const LIVE_TIMEOUT: Duration = Duration::from_secs(1);
const BULK_TIMEOUT: Duration = Duration::from_secs(10);

/// One fetch the shell wants a lane to perform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FetchRequest {
    pub key: ResourceKey,
}

/// Body a lane decoded. Variants grow with the resource set.
#[derive(Clone, Debug)]
pub enum ResourceBody {
    View(ViewDocument),
    Journal(JournalPage),
    Artifact(DecodedArtifact),
}

/// Outcome posted back to the shell. The event loop never calls HTTP.
#[derive(Debug)]
pub struct FetchReply {
    pub key: ResourceKey,
    pub outcome: Result<ResourceBody, String>,
}

/// Request senders for the two lanes plus the shared reply receiver.
pub struct FetchLanes {
    live_tx: Sender<FetchRequest>,
    bulk_tx: Sender<FetchRequest>,
    reply_rx: Receiver<FetchReply>,
}

impl FetchLanes {
    /// Start both workers on `scope`. They exit when the request senders drop.
    #[must_use]
    pub fn spawn<'scope>(scope: &'scope thread::Scope<'scope, '_>, endpoint: Endpoint) -> Self {
        let (live_tx, live_rx) = mpsc::channel();
        let (bulk_tx, bulk_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        spawn_lane(scope, "bloomery-console-live", endpoint.clone(), live_rx, reply_tx.clone(), LIVE_TIMEOUT);
        spawn_lane(scope, "bloomery-console-bulk", endpoint, bulk_rx, reply_tx, BULK_TIMEOUT);
        Self { live_tx, bulk_tx, reply_rx }
    }

    pub fn request(&self, key: ResourceKey) -> Result<(), SendError<FetchRequest>> {
        let tx = match key.lane() {
            Lane::Live => &self.live_tx,
            Lane::Bulk => &self.bulk_tx,
        };
        tx.send(FetchRequest { key })
    }

    /// Non-blocking drain. Empty or a closed lane both yield no item.
    pub fn drain(&self) -> impl Iterator<Item = FetchReply> + '_ {
        iter::from_fn(|| self.reply_rx.try_recv().ok())
    }
}

fn spawn_lane<'scope>(
    scope: &'scope thread::Scope<'scope, '_>,
    name: &'static str,
    endpoint: Endpoint,
    requests: Receiver<FetchRequest>,
    replies: Sender<FetchReply>,
    timeout: Duration,
) {
    // `thread::scope`, not `thread::spawn`: raw spawn is disallowed
    // (settlement/trace umbrella). These workers sit below the actor/mail
    // layer and join when the shell drops the request senders.
    let _ = thread::Builder::new()
        .name(name.into())
        .spawn_scoped(scope, move || lane_loop(&endpoint, &requests, &replies, timeout));
}

fn lane_loop(endpoint: &Endpoint, requests: &Receiver<FetchRequest>, replies: &Sender<FetchReply>, timeout: Duration) {
    loop {
        match requests.recv() {
            Ok(FetchRequest { key }) => {
                let outcome = fetch_key(endpoint, key, timeout);
                if replies.send(FetchReply { key, outcome }).is_err() {
                    return;
                }
            }
            Err(RecvError) => return,
        }
    }
}

fn fetch_key(endpoint: &Endpoint, key: ResourceKey, timeout: Duration) -> Result<ResourceBody, String> {
    let path = key.path();
    match key {
        ResourceKey::View => http::get_json::<ViewDocument>(endpoint, &path, timeout)
            .map(ResourceBody::View)
            .map_err(|error| error.to_string()),
        ResourceKey::Journal(_) => http::get_json::<JournalPage>(endpoint, &path, timeout)
            .map(ResourceBody::Journal)
            .map_err(|error| error.to_string()),
        ResourceKey::Artifact(_) => http::get_json::<DecodedArtifact>(endpoint, &path, timeout)
            .map(ResourceBody::Artifact)
            .map_err(|error| error.to_string()),
    }
}

#[cfg(test)]
pub struct FetchProbe {
    live_rx: Receiver<FetchRequest>,
    bulk_rx: Receiver<FetchRequest>,
    reply_tx: Sender<FetchReply>,
}

#[cfg(test)]
impl FetchLanes {
    #[must_use]
    pub fn pair() -> (Self, FetchProbe) {
        let (live_tx, live_rx) = mpsc::channel();
        let (bulk_tx, bulk_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        (Self { live_tx, bulk_tx, reply_rx }, FetchProbe { live_rx, bulk_rx, reply_tx })
    }
}

#[cfg(test)]
impl FetchProbe {
    #[must_use]
    pub fn take_live(&self) -> Option<FetchRequest> {
        self.live_rx.try_recv().ok()
    }

    #[must_use]
    pub fn take_bulk(&self) -> Option<FetchRequest> {
        self.bulk_rx.try_recv().ok()
    }

    /// Push a completed fetch into the shell's reply queue.
    ///
    /// # Panics
    ///
    /// Panics if the shell has dropped its reply receiver.
    pub fn reply(&self, reply: FetchReply) {
        self.reply_tx.send(reply).expect("shell still owns the reply receiver");
    }
}
