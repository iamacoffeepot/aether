//! The engine connection: one RPC client peer addressing one unit's journal
//! owner and the bundle driver.

use aether_bloomery_kinds::{
    Call, CallOutcome, EncodedArtifact, Publish, PublishResult, ReadArtifacts, ReadArtifactsResult, ReadEvents,
    ReadEventsResult, ReadHead, ReadHeadResult, Stage as StageArtifacts, StageResult, UnitKey, WatchHead,
    WatchHeadResult,
};
use aether_data::{ErasedActorPath, Kind};
use aether_rpc::{MailEnvelope, PeerKind, Recipient, RpcClient, RpcConnection, WireFrame};
use anyhow::{Context, Result, anyhow, bail};

use super::{Reads, Stage};

/// The bundle driver's canonical path: one driver per engine.
const DRIVER: &str = "aether.bloomery.driver:driver";

/// A connection to one Bloomery engine over its own RPC port, addressing the
/// journal owner of one unit and the bundle driver.
pub struct Engine {
    connection: RpcConnection,
    journal: Recipient,
    driver: Recipient,
}

impl Engine {
    /// Dial `127.0.0.1:<rpc_port>` as the client peer `client_name`,
    /// addressing the journal owner of `unit` at its canonical path,
    /// `aether.bloomery.journal:<key>` (ADR-0240 D8), and the driver at
    /// `aether.bloomery.driver:driver`.
    ///
    /// # Errors
    /// The dial or the handshake failed.
    pub fn connect(rpc_port: u16, unit: &UnitKey, client_name: &str) -> Result<Self> {
        let peer = PeerKind::Client {
            client_name: client_name.to_owned(),
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
        };
        let connection = RpcClient::connect(&format!("127.0.0.1:{rpc_port}"), peer, || {})
            .with_context(|| format!("dialing the engine on port {rpc_port}"))?;

        let journal = Recipient::local(ErasedActorPath::new(&format!("aether.bloomery.journal:{unit}"))?);
        let driver = Recipient::local(ErasedActorPath::new(DRIVER)?);
        Ok(Self { connection, journal, driver })
    }

    /// The journal's last stored sequence: the fence a publish starts at.
    ///
    /// # Errors
    /// The journal backend failed or the transport did.
    pub fn read_head(&mut self) -> Result<u64> {
        match ask(&mut self.connection, &self.journal, &ReadHead)? {
            ReadHeadResult::Ok { head } => Ok(head),
            ReadHeadResult::Err { message } => bail!("reading the journal head: {message}"),
        }
    }

    /// Store `artifacts` content-addressed through the journal's unfenced
    /// `stage`, with no event and no head move.
    ///
    /// # Errors
    /// The journal refused the stage (a dangling or mistyped citation, or a
    /// backend failure), or the transport failed.
    pub fn stage_artifacts(&mut self, artifacts: Vec<EncodedArtifact>) -> Result<()> {
        match ask(&mut self.connection, &self.journal, &StageArtifacts::new(artifacts))? {
            StageResult::Staged => Ok(()),
            StageResult::Err { message } => bail!("the journal refused the stage: {message}"),
        }
    }

    /// Ask the driver to run `call` and return its one outcome.
    ///
    /// # Errors
    /// The transport failed or the call settled without an outcome.
    pub fn call_program(&mut self, call: &Call) -> Result<CallOutcome> {
        ask(&mut self.connection, &self.driver, call)
    }
}

impl Stage for Engine {
    fn stage(&mut self, publish: &Publish) -> Result<PublishResult> {
        ask(&mut self.connection, &self.journal, publish)
    }
}

impl Reads for Engine {
    fn read_events(&mut self, request: ReadEvents) -> Result<ReadEventsResult> {
        ask(&mut self.connection, &self.journal, &request)
    }

    fn watch_head(&mut self, request: WatchHead) -> Result<WatchHeadResult> {
        ask(&mut self.connection, &self.journal, &request)
    }

    fn read_artifacts(&mut self, request: &ReadArtifacts) -> Result<ReadArtifactsResult> {
        ask(&mut self.connection, &self.journal, request)
    }
}

/// Send `request` to `to` and return its `Reply`, reading frames until the
/// call's `ReplyEnd`.
fn ask<Request: Kind, Reply: Kind>(connection: &mut RpcConnection, to: &Recipient, request: &Request) -> Result<Reply> {
    let envelope = MailEnvelope { to: to.clone(), kind: Request::ID, payload: request.encode_into_bytes() };
    let cid = connection.client.call(envelope)?;

    let mut reply = None;
    loop {
        match connection.inbound.recv().context("the engine connection closed mid-call")? {
            WireFrame::ReplyEvent { cid: seen, envelope } if seen == cid && envelope.kind == Reply::ID => {
                let decoded = Reply::decode_from_bytes(&envelope.payload);
                reply = Some(decoded.with_context(|| format!("decoding a {} reply", Reply::NAME))?);
            }
            WireFrame::ReplyEnd { cid: seen, result } if seen == cid => {
                result.map_err(|error| anyhow!("{} to {}: {error:?}", Request::NAME, to.path))?;
                return reply.with_context(|| format!("{} settled with no {} reply", Request::NAME, Reply::NAME));
            }
            WireFrame::Bye { reason } => bail!("the engine closed the connection: {reason}"),
            _ => {}
        }
    }
}
