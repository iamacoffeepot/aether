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
    /// The transport or the call failed, or the call settled with no outcome.
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

/// Send `request` to `to` and return its `Reply` when the matching
/// `ReplyEvent` arrives, without waiting for the call to settle.
fn ask<Request: Kind, Reply: Kind>(connection: &mut RpcConnection, to: &Recipient, request: &Request) -> Result<Reply> {
    let envelope = MailEnvelope { to: to.clone(), kind: Request::ID, payload: request.encode_into_bytes() };
    let cid = connection.client.call(envelope)?;
    reply_of(cid, Request::NAME, to.path.as_str(), || {
        connection.inbound.recv().context("the engine connection closed mid-call")
    })
}

/// Return the first matching `ReplyEvent` for `cid`, without waiting for its
/// `ReplyEnd`.
///
/// Every request these verbs send has exactly one reply, so the first
/// `ReplyEvent` whose `cid` matches and whose envelope kind is `Reply::ID`
/// is the answer. A `ReplyEnd` for the call that arrives before any reply
/// still ends the wait: with its error when it carries one, and otherwise
/// with a settled-with-no-reply error.
fn reply_of<Reply: Kind>(
    cid: u64,
    request: &str,
    to: &str,
    mut next: impl FnMut() -> Result<WireFrame>,
) -> Result<Reply> {
    loop {
        match next()? {
            WireFrame::ReplyEvent { cid: seen, envelope } => {
                let for_this_call = seen == cid;
                let is_reply = envelope.kind == Reply::ID;
                let matches_call = for_this_call && is_reply;
                if matches_call {
                    return Reply::decode_from_bytes(&envelope.payload)
                        .with_context(|| format!("decoding a {} reply", Reply::NAME));
                }
            }
            WireFrame::ReplyEnd { cid: seen, result } if seen == cid => {
                result.map_err(|error| anyhow!("{request} to {to}: {error:?}"))?;
                return Err(anyhow!("{request} settled with no {} reply", Reply::NAME));
            }
            WireFrame::Bye { reason } => bail!("the engine closed the connection: {reason}"),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::reply_of;
    use aether_bloomery_kinds::ReadHeadResult;
    use aether_data::Kind;
    use aether_rpc::{ReplyEnvelope, RpcError, WireFrame};
    use anyhow::{Context, Result};

    const REQUEST: &str = "aether.bloomery.journal.read_head";
    const TO: &str = "aether.bloomery.journal:primary";

    struct Script<'a> {
        frames: &'a [WireFrame],
        reads: usize,
    }

    impl Script<'_> {
        fn next(&mut self) -> Result<WireFrame> {
            let frame = self.frames.get(self.reads).context("out of frames")?.clone();
            self.reads += 1;
            Ok(frame)
        }
    }

    fn reply_event(cid: u64, reply: &ReadHeadResult) -> WireFrame {
        WireFrame::ReplyEvent {
            cid,
            envelope: ReplyEnvelope { kind: ReadHeadResult::ID, payload: reply.encode_into_bytes() },
        }
    }

    fn settled_end(cid: u64) -> WireFrame {
        WireFrame::ReplyEnd { cid, result: Ok(()) }
    }

    fn failed_end(cid: u64, reason: &str) -> WireFrame {
        WireFrame::ReplyEnd { cid, result: Err(RpcError::Other { reason: reason.to_owned() }) }
    }

    #[test]
    fn returns_on_the_reply_without_waiting_for_settlement() -> Result<()> {
        // Catches waiting for settlement: the frames after the reply are never read.
        let frames = [
            reply_event(7, &ReadHeadResult::Ok { head: 41 }),
            settled_end(7),
            WireFrame::Bye { reason: "closed".to_owned() },
        ];
        let mut script = Script { frames: &frames, reads: 0 };
        let reply: ReadHeadResult = reply_of(7, REQUEST, TO, || script.next())?;
        assert!(matches!(reply, ReadHeadResult::Ok { head: 41 }));
        assert_eq!(script.reads, 1);
        Ok(())
    }

    #[test]
    fn a_failed_call_reports_its_error() {
        // Catches a call that failed hanging or reporting no cause.
        let frames = [failed_end(7, "boom")];
        let mut script = Script { frames: &frames, reads: 0 };
        let result: Result<ReadHeadResult> = reply_of(7, REQUEST, TO, || script.next());
        let error = result.expect_err("a failed call is an error").to_string();
        let keeps_cause = error.contains("boom");
        assert!(keeps_cause, "the error keeps its cause: {error}");
    }

    #[test]
    fn leftover_frames_from_an_earlier_call_are_skipped() -> Result<()> {
        // Catches a left-over frame from an earlier call ending this one.
        let frames = [
            settled_end(3),
            reply_event(3, &ReadHeadResult::Ok { head: 1 }),
            reply_event(7, &ReadHeadResult::Ok { head: 9 }),
        ];
        let mut script = Script { frames: &frames, reads: 0 };
        let reply: ReadHeadResult = reply_of(7, REQUEST, TO, || script.next())?;
        assert!(matches!(reply, ReadHeadResult::Ok { head: 9 }));
        assert_eq!(script.reads, 3);
        Ok(())
    }
}
