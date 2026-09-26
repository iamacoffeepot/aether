//! Stage every batch through the journal's fenced publish, with no head move.
//!
//! A publish that moves no head appends no event, so the fence read once at
//! the start holds across every batch. A stale fence answers `Conflict`, and
//! the batch is resent at the actual sequence: staging is idempotent, so a
//! resend can only store what the first attempt did not.

use aether_bloomery_kinds::{Digest, EncodedArtifact, Publish, PublishResult, ReadHead, ReadHeadResult};
use aether_data::{ActorPath, Kind};
use aether_rpc::{MailEnvelope, PeerKind, Recipient, RpcClient, RpcConnection, WireFrame};
use anyhow::{Context, Result, anyhow, bail};

/// The journal owner's actor path in a Bloomery engine.
const JOURNAL: &str = "aether.bloomery.journal:journal";

/// Send one publish and return the journal's answer.
///
/// The engine connection implements it; the tests implement it over a
/// scratch journal so the loop below runs against real citation checks.
pub(super) trait Stage {
    /// Deliver `publish` and wait for its [`PublishResult`].
    ///
    /// # Errors
    /// The transport failed or the call settled without a result.
    fn stage(&mut self, publish: &Publish) -> Result<PublishResult>;
}

/// Stage `batches` in order, starting at the whole-journal fence `fence`.
///
/// # Errors
/// The journal refused a batch, answered with digests other than those
/// sent, or the transport failed.
pub(super) fn publish(stage: &mut impl Stage, mut fence: u64, batches: Vec<Vec<EncodedArtifact>>) -> Result<()> {
    let count = batches.len();
    for (index, artifacts) in batches.into_iter().enumerate() {
        let number = index + 1;
        let sent: Vec<Digest> = artifacts.iter().map(EncodedArtifact::digest).collect();
        let mut request = Publish::new(artifacts, Vec::new(), fence);

        fence = loop {
            match stage.stage(&request)? {
                PublishResult::Committed { head, artifacts } if artifacts == sent => break head,
                PublishResult::Committed { artifacts, .. } => bail!(
                    "batch {number} of {count}: the journal staged {} digests that differ from the {} sent",
                    artifacts.len(),
                    sent.len()
                ),
                PublishResult::Conflict { actual } if actual == request.expected_seq() => {
                    bail!(
                        "batch {number} of {count}: the journal reported a conflict at the fence {actual} it was sent"
                    )
                }
                PublishResult::Conflict { actual } => {
                    let (artifacts, moves, _) = request.into_parts();
                    request = Publish::new(artifacts, moves, actual);
                }
                PublishResult::Err { message } => bail!("batch {number} of {count}: the journal refused it: {message}"),
            }
        };
        eprintln!("import-commit: staged batch {number} of {count} ({} artifacts)", sent.len());
    }
    Ok(())
}

/// A connection to one Bloomery engine's journal owner over the engine's own
/// RPC port.
pub(super) struct EngineJournal {
    connection: RpcConnection,
    recipient: Recipient,
}

impl EngineJournal {
    /// Dial `127.0.0.1:<rpc_port>` as a client peer.
    ///
    /// # Errors
    /// The dial or the handshake failed.
    pub(super) fn connect(rpc_port: u16) -> Result<Self> {
        let peer = PeerKind::Client {
            client_name: "xtask import-commit".to_owned(),
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
        };
        let connection = RpcClient::connect(&format!("127.0.0.1:{rpc_port}"), peer, || {})
            .with_context(|| format!("dialing the engine on port {rpc_port}"))?;
        Ok(Self { connection, recipient: Recipient::local(ActorPath::new(JOURNAL)?) })
    }

    /// The journal's last stored sequence: the fence every batch starts at.
    ///
    /// # Errors
    /// The journal backend failed or the transport did.
    pub(super) fn read_head(&mut self) -> Result<u64> {
        match self.call(&ReadHead)? {
            ReadHeadResult::Ok { head } => Ok(head),
            ReadHeadResult::Err { message } => bail!("reading the journal head: {message}"),
        }
    }

    /// Send `request` to the journal owner and return its `Reply`, reading
    /// frames until the call's `ReplyEnd`.
    fn call<Request: Kind, Reply: Kind>(&mut self, request: &Request) -> Result<Reply> {
        let envelope =
            MailEnvelope { to: self.recipient.clone(), kind: Request::ID, payload: request.encode_into_bytes() };
        let cid = self.connection.client.call(envelope)?;

        let mut reply = None;
        loop {
            match self.connection.inbound.recv().context("the engine connection closed mid-call")? {
                WireFrame::ReplyEvent { cid: seen, envelope } if seen == cid && envelope.kind == Reply::ID => {
                    let decoded = Reply::decode_from_bytes(&envelope.payload);
                    reply = Some(decoded.with_context(|| format!("decoding a {} reply", Reply::NAME))?);
                }
                WireFrame::ReplyEnd { cid: seen, result } if seen == cid => {
                    result.map_err(|error| anyhow!("{} to {JOURNAL}: {error:?}", Request::NAME))?;
                    return reply.with_context(|| format!("{} settled with no {} reply", Request::NAME, Reply::NAME));
                }
                WireFrame::Bye { reason } => bail!("the engine closed the connection: {reason}"),
                _ => {}
            }
        }
    }
}

impl Stage for EngineJournal {
    fn stage(&mut self, publish: &Publish) -> Result<PublishResult> {
        self.call(publish)
    }
}
