//! Runtime half of one trusted player connection.

#![allow(clippy::needless_pass_by_value)]

use std::collections::BTreeMap;
use std::mem;

use aether_actor::runtime;
use aether_codec::frame::encode_frame;
use aether_data::{Kind, MailboxId, wire};
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;

use super::{PlayerSessionActor, PlayerSessionConfig, PollResult, SessionClosed, SessionData, TickBundle};
use crate::game::player::{PlayerFrame, WIRE_VERSION};
use crate::game::{MoveIntent, Poll, Spawn};
use crate::tcp::{TcpCapability, TcpNativeExt};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionPhase {
    Handshake,
    CatchingUp,
    Active,
    Closed,
}

pub struct PlayerSessionState {
    listener_name: String,
    session_name: String,
    peer: String,
    turn_sim_mailbox: MailboxId,
    interval_nanos: u64,
    max_pending_live_bundles: usize,
    session_identity: MailboxId,
    phase: SessionPhase,
    pending_live_bundles: BTreeMap<u64, TickBundle>,
    last_sent_tick: Option<u64>,
}

#[runtime]
impl NativeActor for PlayerSessionActor {
    type State = PlayerSessionState;
    type Config = PlayerSessionConfig;
    const NAMESPACE: &'static str = "aether.game.player.session";

    fn init(config: PlayerSessionConfig, ctx: &mut NativeInitCtx<'_>) -> Result<PlayerSessionState, BootError> {
        Ok(PlayerSessionState {
            listener_name: config.listener_name,
            session_name: config.session_name,
            peer: config.peer,
            turn_sim_mailbox: config.turn_sim_mailbox,
            interval_nanos: config.interval_nanos,
            max_pending_live_bundles: config.max_pending_live_bundles,
            session_identity: ctx.self_id(),
            phase: SessionPhase::Handshake,
            pending_live_bundles: BTreeMap::new(),
            last_sent_tick: None,
        })
    }

    #[handler::single]
    fn on_session_data(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: SessionData) {
        if mail.session_name != state.session_name || mail.peer != state.peer {
            state.close(ctx, "tcp session metadata changed".into());
            return;
        }

        let frame = match wire::from_bytes::<PlayerFrame>(&mail.bytes) {
            Ok(frame) => frame,
            Err(error) => {
                state.close(ctx, format!("invalid player frame: {error}"));
                return;
            }
        };

        match state.phase {
            SessionPhase::Handshake => state.handle_handshake(ctx, frame),
            SessionPhase::CatchingUp => state.close(ctx, "player frame arrived before HelloAck".into()),
            SessionPhase::Active => state.handle_active(ctx, frame),
            SessionPhase::Closed => {}
        }
    }

    #[handler::single]
    fn on_session_closed(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: SessionClosed) {
        if mail.session_name == state.session_name {
            state.phase = SessionPhase::Closed;
            ctx.shutdown();
        }
    }

    #[handler::single]
    fn on_poll_result(state: &mut Self::State, ctx: &mut NativeCtx<'_>, result: PollResult) {
        // Replies carry `SourceAddr::None` with the original request
        // correlation; the configured simulation was selected when this actor
        // sent the tracked `Poll`, so phase is the reply admission gate here.
        if state.phase != SessionPhase::CatchingUp {
            return;
        }

        state.write(
            ctx,
            &PlayerFrame::HelloAck {
                wire_version: WIRE_VERSION,
                session_identity: state.session_identity,
                tick: result.current_tick,
                interval_nanos: state.interval_nanos,
            },
        );

        let mut retained = BTreeMap::new();
        for bundle in result.bundles {
            retained.insert(bundle.tick, bundle);
        }
        for (_, bundle) in retained {
            state.emit_bundle(ctx, bundle);
        }

        state.phase = SessionPhase::Active;
        let watermark = result.current_tick;
        let live = mem::take(&mut state.pending_live_bundles);
        for (_, bundle) in live.into_iter().filter(|(tick, _)| *tick > watermark) {
            state.emit_bundle(ctx, bundle);
        }
    }

    #[handler::single]
    fn on_tick_bundle(state: &mut Self::State, ctx: &mut NativeCtx<'_>, bundle: TickBundle) {
        match state.phase {
            SessionPhase::CatchingUp => {
                if !state.pending_live_bundles.contains_key(&bundle.tick)
                    && state.pending_live_bundles.len() >= state.max_pending_live_bundles
                {
                    state.close(
                        ctx,
                        format!(
                            "catch-up live bundle capacity {} exceeded by tick {}",
                            state.max_pending_live_bundles, bundle.tick
                        ),
                    );
                    return;
                }
                state.pending_live_bundles.insert(bundle.tick, bundle);
            }
            SessionPhase::Active => state.emit_bundle(ctx, bundle),
            SessionPhase::Handshake | SessionPhase::Closed => {}
        }
    }
}

impl PlayerSessionState {
    fn handle_handshake(&mut self, ctx: &mut NativeCtx<'_>, frame: PlayerFrame) {
        let PlayerFrame::Hello { wire_version, client_name: _ } = frame else {
            self.close(ctx, "expected Hello as first player frame".into());
            return;
        };
        if wire_version != WIRE_VERSION {
            self.close(ctx, format!("wire_version mismatch: client={wire_version}, server={WIRE_VERSION}"));
            return;
        }

        self.phase = SessionPhase::CatchingUp;
        let poll = Poll { since_tick: 0 };
        let _ = ctx.send_envelope_tracked(self.turn_sim_mailbox, Poll::ID, &poll.encode_into_bytes());
    }

    fn handle_active(&mut self, ctx: &mut NativeCtx<'_>, frame: PlayerFrame) {
        match frame {
            PlayerFrame::Intent { kind, payload } if kind == Spawn::ID => {
                let Some(mut spawn) = Spawn::decode_from_bytes(&payload) else {
                    tracing::warn!(
                        target: "aether_substrate::game",
                        session = %self.session_name,
                        peer = %self.peer,
                        "player session dropped malformed spawn intent",
                    );
                    return;
                };
                spawn.entity_id = self.session_identity.0;
                let _ = ctx.send_envelope_tracked(self.turn_sim_mailbox, Spawn::ID, &spawn.encode_into_bytes());
            }
            PlayerFrame::Intent { kind, payload } if kind == MoveIntent::ID => {
                let Some(mut intent) = MoveIntent::decode_from_bytes(&payload) else {
                    tracing::warn!(
                        target: "aether_substrate::game",
                        session = %self.session_name,
                        peer = %self.peer,
                        "player session dropped malformed move intent",
                    );
                    return;
                };
                intent.entity_id = self.session_identity.0;
                let _ = ctx.send_envelope_tracked(self.turn_sim_mailbox, MoveIntent::ID, &intent.encode_into_bytes());
            }
            PlayerFrame::Intent { kind, .. } => {
                tracing::warn!(
                    target: "aether_substrate::game",
                    session = %self.session_name,
                    peer = %self.peer,
                    kind = %kind,
                    "player session dropped non-allowlisted intent kind",
                );
            }
            PlayerFrame::Close { reason } => self.close(ctx, format!("client close: {reason}")),
            PlayerFrame::Hello { .. }
            | PlayerFrame::HelloAck { .. }
            | PlayerFrame::Fact { .. }
            | PlayerFrame::Beacon { .. } => self.close(ctx, "unexpected client player frame".into()),
        }
    }

    fn emit_bundle(&mut self, ctx: &mut NativeCtx<'_>, bundle: TickBundle) {
        if self.last_sent_tick.is_some_and(|tick| bundle.tick <= tick) {
            return;
        }

        let tick = bundle.tick;
        if !self.write(ctx, &PlayerFrame::Fact { kind: TickBundle::ID, payload: bundle.encode_into_bytes() }) {
            return;
        }
        if !self.write(
            ctx,
            &PlayerFrame::Beacon {
                tick,
                server_nanos: ctx.mailer().now_nanos().0,
                interval_nanos: self.interval_nanos,
            },
        ) {
            return;
        }
        self.last_sent_tick = Some(tick);
    }

    fn write(&self, ctx: &mut NativeCtx<'_>, frame: &PlayerFrame) -> bool {
        match encode_frame(frame) {
            Ok(bytes) => {
                ctx.actor::<TcpCapability>().session_write(&self.listener_name, &self.session_name, &bytes);
                true
            }
            Err(error) => {
                tracing::warn!(
                    target: "aether_substrate::game",
                    session = %self.session_name,
                    peer = %self.peer,
                    error = %error,
                    "player session frame encode failed",
                );
                false
            }
        }
    }

    fn close(&mut self, ctx: &mut NativeCtx<'_>, reason: String) {
        if self.phase == SessionPhase::Closed {
            return;
        }
        let _ = self.write(ctx, &PlayerFrame::Close { reason });
        ctx.actor::<TcpCapability>().session_close(&self.listener_name, &self.session_name);
        self.phase = SessionPhase::Closed;
        ctx.shutdown();
    }
}
