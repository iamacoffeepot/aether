//! The engine connection: one RPC client peer addressing one unit's journal
//! owner, the bundle driver, and the workspace actor.

use aether_actor::ActorPath;
use aether_bloomery_journal::JournalActor;
use aether_bloomery_kinds::{
    ArtifactStorage, Call, CallOutcome, EncodedArtifact, Publish, PublishResult, ReadArtifacts, ReadArtifactsResult,
    ReadEvents, ReadEventsResult, ReadHead, ReadHeadResult, Stage as StageArtifacts, StageResult, Tree, UnitKey,
    WatchHead, WatchHeadResult,
};
use aether_bloomery_workspace::{ImageRef, Import, ImportError, ImportResult, WorkspaceCapability};
use aether_data::{ErasedActorPath, Kind, Ref};
use aether_rpc::{MailEnvelope, PeerKind, Recipient, RpcClient, RpcConnection, WireFrame};
use anyhow::{Context, Result, anyhow, bail};

use super::{Reads, Stage};

/// The bundle driver's canonical path: one driver per engine.
const DRIVER: &str = "aether.bloomery.driver:driver";

/// A connection to one Bloomery engine over its own RPC port, addressing the
/// journal owner of one unit, the bundle driver, and the workspace actor.
pub struct Engine {
    connection: RpcConnection,
    unit: UnitKey,
    journal: Recipient,
    driver: Recipient,
    workspace: Recipient,
}

impl Engine {
    /// Dial `127.0.0.1:<rpc_port>` as the client peer `client_name`,
    /// addressing the journal owner of `unit` at its canonical path,
    /// `aether.bloomery.journal:<key>` (ADR-0240 D8), and the driver at
    /// `aether.bloomery.driver:driver`, and the workspace actor at its root
    /// path, `aether.bloomery.workspace`.
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
        let workspace = Recipient::local(ActorPath::<WorkspaceCapability>::root().as_erased().clone());
        Ok(Self { connection, unit: unit.clone(), journal, driver, workspace })
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

    /// Import `image` through the workspace actor, staging its filesystem as a
    /// tree into this unit's journal, and return the tree.
    ///
    /// The `source` the workspace stages into is the unit's journal owner,
    /// built as a typed path: `JournalActor` covers `ArtifactStorage`, which
    /// the compiler checks here and the receiver proves again at decode.
    ///
    /// # Errors
    /// The workspace refused or failed the import, or the transport failed.
    pub fn import(&mut self, image: &ImageRef) -> Result<Ref<Tree>> {
        let source = ActorPath::<JournalActor>::instance(self.unit.as_load_name()).narrow::<ArtifactStorage>();
        let request = Import { image: image.clone(), source };

        match ask(&mut self.connection, &self.workspace, &request)? {
            ImportResult::Ok { tree } => Ok(tree),
            ImportResult::Err(ImportError::Failed { detail }) => bail!("importing {}: {detail:?}", image.as_str()),
            ImportResult::Err(ImportError::Source(refused)) => {
                bail!("importing {}: the journal path did not prove: {refused:?}", image.as_str())
            }
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
