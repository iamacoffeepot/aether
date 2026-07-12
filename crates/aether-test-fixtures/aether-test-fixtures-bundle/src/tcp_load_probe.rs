//! Consumer and echo meter for the real-process `aether.tcp` load scenario.

// Handler payloads follow the by-value dispatch ABI even when the body only
// borrows their fields.
#![allow(clippy::needless_pass_by_value)]

use aether_actor::{ActorInitError, Manual, OutboundReply, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_capabilities::tcp::{ConnectResult, SessionClosed, SessionData, TcpCapability, TcpWasmExt};
use aether_test_fixtures_kinds::{
    CollectTcpLoadSnapshot, ConfigureTcpLoadProbe, StartTcpConnectLoad, TcpLoadSessionSnapshot, TcpLoadSnapshot,
    TcpLoadTopology,
};

#[derive(Default)]
pub struct TcpLoadProbe {
    listener_name: Option<String>,
    sessions: Vec<TcpLoadSessionSnapshot>,
    connect_failures: Vec<String>,
}

impl TcpLoadProbe {
    fn session_index(&self, topology: TcpLoadTopology, session_name: &str) -> Option<usize> {
        self.sessions.iter().position(|session| session.topology == topology && session.session_name == session_name)
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
        self.sessions.push(TcpLoadSessionSnapshot {
            topology,
            session_name: session_name.to_owned(),
            established: topology == TcpLoadTopology::Accepted,
            received_frame_count: 0,
            received_payload_bytes: 0,
            closed: false,
        });
        self.sessions.len() - 1
    }
}

#[actor]
impl WasmActor for TcpLoadProbe {
    const NAMESPACE: &'static str = "test.tcp_load_probe";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self::default())
    }

    #[handler::single]
    fn on_configure(&mut self, _ctx: &mut WasmCtx<'_>, configure: ConfigureTcpLoadProbe) {
        self.listener_name = Some(configure.listener_name);
    }

    #[handler::single]
    fn on_start_connect_load(&mut self, ctx: &mut WasmCtx<'_>, start: StartTcpConnectLoad) {
        let tcp = ctx.actor::<TcpCapability>();
        for index in 0..start.connection_count {
            let session_name = format!("{}-{index}", start.session_name_prefix);
            self.ensure_session(TcpLoadTopology::Outbound, &session_name);
            tcp.connect(&start.addr, Some(&session_name), Some(ctx.mailbox_id()));
        }
    }

    #[handler::single]
    fn on_connect_result(&mut self, _ctx: &mut WasmCtx<'_>, result: ConnectResult) {
        match result {
            ConnectResult::Ok { session_name, .. } => {
                let index = self.ensure_session(TcpLoadTopology::Outbound, &session_name);
                self.sessions[index].established = true;
            }
            ConnectResult::Err { addr, reason } => self.connect_failures.push(format!("{addr}: {reason}")),
        }
    }

    #[handler::single]
    fn on_session_data(&mut self, ctx: &mut WasmCtx<'_>, data: SessionData) {
        let topology = self.topology_for(&data.session_name);
        let index = self.ensure_session(topology, &data.session_name);
        self.sessions[index].established = true;
        self.sessions[index].received_frame_count += 1;
        self.sessions[index].received_payload_bytes +=
            u64::try_from(data.bytes.len()).expect("tcp load payload length fits u64");

        let body_bytes = u32::try_from(data.bytes.len()).expect("tcp load frame body fits the four-byte prefix");
        let mut framed = Vec::with_capacity(4 + data.bytes.len());
        framed.extend_from_slice(&body_bytes.to_le_bytes());
        framed.extend_from_slice(&data.bytes);

        let tcp = ctx.actor::<TcpCapability>();
        match topology {
            TcpLoadTopology::Accepted => tcp.session_write(
                self.listener_name.as_deref().expect("tcp load probe configured before accepted traffic"),
                &data.session_name,
                &framed,
            ),
            TcpLoadTopology::Outbound => tcp.connect_session_write(&data.session_name, &framed),
        }
    }

    #[handler::single]
    fn on_session_closed(&mut self, _ctx: &mut WasmCtx<'_>, closed: SessionClosed) {
        let topology = self.topology_for(&closed.session_name);
        let index = self.ensure_session(topology, &closed.session_name);
        self.sessions[index].closed = true;
    }

    #[handler::manual]
    fn on_collect_snapshot(&mut self, ctx: &mut WasmCtx<'_, Manual>, _query: CollectTcpLoadSnapshot) {
        if ctx.reply_target().is_some() {
            ctx.reply(&TcpLoadSnapshot {
                sessions: self.sessions.clone(),
                connect_failures: self.connect_failures.clone(),
            });
        }
    }
}
