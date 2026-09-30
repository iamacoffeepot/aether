//! Consumer and echo meter for the real-process `aether.tcp` load scenario.

// Handler payloads follow the by-value dispatch ABI even when the body only
// borrows their fields.
#![allow(clippy::needless_pass_by_value)]

use aether_actor::{ActorInitError, CoveredBy, ProtocolRef, WasmActor, WasmCtx, WasmInitCtx, actor, protocol};
use aether_tcp::{
    BindListenerResult, BindListenerSelf, ConnectResult, ConnectSelf, SessionClosed, SessionData, SessionWrite,
    TcpCapability, TcpConsumer,
};
use aether_test_fixtures_kinds::{
    CollectTcpLoadSnapshot, ConfigureTcpLoadProbe, StartTcpConnectLoad, TcpLoadSessionSnapshot, TcpLoadSnapshot,
    TcpLoadTopology,
};

/// What the probe sends a session: the echo of each frame it received. The
/// session arrives only as the stamped sender of its `SessionData`, so the
/// probe casts it to this protocol once, on its first frame (ADR-0231 §4).
#[protocol]
trait SessionWriter {
    fn write(mail: SessionWrite);
}

/// One session the probe has seen: the snapshot it reports, and the session's
/// writer once its first frame has arrived.
struct Session {
    snapshot: TcpLoadSessionSnapshot,
    writer: Option<ProtocolRef<SessionWriter>>,
}

#[derive(Default)]
pub struct TcpLoadProbe {
    local_port: Option<u16>,
    sessions: Vec<Session>,
    connect_failures: Vec<String>,
}

impl TcpLoadProbe {
    fn session_index(&self, topology: TcpLoadTopology, session_name: &str) -> Option<usize> {
        self.sessions
            .iter()
            .position(|session| session.snapshot.topology == topology && session.snapshot.session_name == session_name)
    }

    fn topology_for(&self, session_name: &str) -> TcpLoadTopology {
        if self.session_index(TcpLoadTopology::Outbound, session_name).is_some() {
            TcpLoadTopology::Outbound
        } else {
            TcpLoadTopology::Accepted
        }
    }

    fn ensure_session(&mut self, topology: TcpLoadTopology, session_name: &str) -> usize {
        if let Some(index) = self.session_index(topology, session_name) {
            return index;
        }
        self.sessions.push(Session {
            snapshot: TcpLoadSessionSnapshot {
                topology,
                session_name: session_name.to_owned(),
                established: topology == TcpLoadTopology::Accepted,
                received_frame_count: 0,
                received_payload_bytes: 0,
                closed: false,
            },
            writer: None,
        });
        self.sessions.len() - 1
    }
}

#[actor(root, depends(TcpCapability))]
impl WasmActor for TcpLoadProbe {
    const NAMESPACE: &'static str = "test.tcp_load_probe";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self::default())
    }

    /// Bind the named listener with this probe as the consumer; the bind
    /// reply lands in [`Self::on_bind_result`] inside the same chain, so the
    /// configure call settles with the port known.
    #[handler::single]
    fn on_configure(&mut self, ctx: &mut WasmCtx<'_, Self>, configure: ConfigureTcpLoadProbe) {
        ctx.send::<TcpCapability>(&BindListenerSelf {
            addr: "127.0.0.1:0".to_owned(),
            name: Some(configure.listener_name),
        });
    }

    #[handler::single]
    fn on_bind_result(&mut self, _ctx: &mut WasmCtx<'_>, result: BindListenerResult) {
        match result {
            BindListenerResult::Ok { local_port, .. } => self.local_port = Some(local_port),
            BindListenerResult::Err { addr, error } => self.connect_failures.push(format!("bind {addr}: {error}")),
        }
    }

    #[handler::single]
    fn on_start_connect_load(&mut self, ctx: &mut WasmCtx<'_, Self>, start: StartTcpConnectLoad) {
        for index in 0..start.connection_count {
            let session_name = format!("{}-{index}", start.session_name_prefix);
            self.ensure_session(TcpLoadTopology::Outbound, &session_name);
            ctx.send::<TcpCapability>(&ConnectSelf { addr: start.addr.clone(), name: Some(session_name) });
        }
    }

    #[handler::single]
    fn on_connect_result(&mut self, _ctx: &mut WasmCtx<'_>, result: ConnectResult) {
        match result {
            ConnectResult::Ok { session_name, .. } => {
                let index = self.ensure_session(TcpLoadTopology::Outbound, &session_name);
                self.sessions[index].snapshot.established = true;
            }
            ConnectResult::Err { addr, error } => self.connect_failures.push(format!("{addr}: {error}")),
        }
    }

    #[handler::single]
    fn on_session_data(&mut self, ctx: &mut WasmCtx<'_>, data: SessionData) {
        let topology = self.topology_for(&data.session_name);
        let index = self.ensure_session(topology, &data.session_name);
        let session = &mut self.sessions[index];
        session.snapshot.established = true;
        session.snapshot.received_frame_count += 1;
        session.snapshot.received_payload_bytes +=
            u64::try_from(data.bytes.len()).expect("tcp load payload length fits u64");

        let body_bytes = u32::try_from(data.bytes.len()).expect("tcp load frame body fits the four-byte prefix");
        let mut framed = Vec::with_capacity(4 + data.bytes.len());
        framed.extend_from_slice(&body_bytes.to_le_bytes());
        framed.extend_from_slice(&data.bytes);

        // The session that delivered the frame is the stamped sender, so the
        // echo goes back to it whichever topology spawned it. It is cast on
        // the session's first frame and kept.
        if session.writer.is_none() {
            session.writer = ctx.sender().and_then(|sender| ctx.cast(sender));
        }
        if let Some(writer) = session.writer {
            ctx.send_to(writer, &SessionWrite { bytes: framed });
        }
    }

    #[handler::single]
    fn on_session_closed(&mut self, _ctx: &mut WasmCtx<'_>, closed: SessionClosed) {
        let topology = self.topology_for(&closed.session_name);
        let index = self.ensure_session(topology, &closed.session_name);
        self.sessions[index].snapshot.closed = true;
    }

    #[handler::single]
    fn on_collect_snapshot(&mut self, _ctx: &mut WasmCtx<'_>, _query: CollectTcpLoadSnapshot) -> TcpLoadSnapshot {
        TcpLoadSnapshot {
            sessions: self.sessions.iter().map(|session| session.snapshot.clone()).collect(),
            connect_failures: self.connect_failures.clone(),
            local_port: self.local_port,
        }
    }
}

// The probe binds and dials with `BindListenerSelf` / `ConnectSelf`, and the
// tcp cap casts the sender to `TcpConsumer` at receipt (ADR-0231 §4), refusing
// a sender whose published rows lack either silent row. Checking coverage here
// turns a handler change that would make that cast refuse the probe into a
// build error instead of a FleetHarness run that times out waiting for frames.
const _: () = {
    const fn covered<P: CoveredBy<R>, R>() {}
    covered::<TcpConsumer, TcpLoadProbe>();
};
