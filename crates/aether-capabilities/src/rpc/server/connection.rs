//! Connection-side plumbing for the `RpcServerCapability`: the
//! sidecar->dispatcher event type, per-connection state, the
//! per-connection reader loop, and the oversize-frame guard. These are
//! plain items (no actor-macro surface), split out of the cap's identity +
//! runtime modules so the actor core stays navigable.

use super::RpcInboundReady;
use crate::rpc::{RpcError, WireFrame};
use aether_codec::frame::{FrameError, read_frame};
use aether_data::{Kind, KindId, MailboxId};
use aether_substrate::Mail;
use aether_substrate::mail::mailer::Mailer;
use std::io::{self, BufReader};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;

/// Per-connection identifier, monotonic within this cap. Distinct
/// from the OS-level peer addr (one peer may reconnect; ids stay
/// unique for the cap's lifetime).
pub(super) type ConnId = u64;

/// Internal event the accept / reader sidecar threads push to the
/// cap dispatcher via an mpsc. The matching wake-mail kind is
/// [`RpcInboundReady`] (empty payload) — the dispatcher's
/// `on_inbound_ready` handler drains the channel and dispatches
/// per item.
pub(super) enum InboundEvent {
    PeerAccepted {
        stream: TcpStream,
        peer: SocketAddr,
    },
    FrameReceived {
        conn_id: ConnId,
        frame: WireFrame,
    },
    ReaderClosed {
        conn_id: ConnId,
        reason: String,
    },
    /// An inbound frame failed to decode but the reader managed to
    /// keep frame-sync (issue 1271). Today this only fires for a
    /// length-prefix that exceeded the framing cap and whose body
    /// was small enough (`size <= 2 * max`) for the reader to drain
    /// without itself becoming an OOM vector. The dispatcher writes
    /// a `ReplyEnd { cid: 0, result: Err(RpcError::FrameTooLarge) }`
    /// and the connection survives. `cid = 0` is the agreed
    /// sentinel for "wire-level error, no in-flight call id to
    /// match against" — the mcp-side router lifts it as an
    /// out-of-band error against the most recently pending call.
    FrameDecodeError {
        conn_id: ConnId,
        error: RpcError,
    },
    /// An inbound length prefix announced a body larger than
    /// `2 * max_frame_size` (issue 1271). Draining it would
    /// itself defeat the OOM guard the cap was written for, so the
    /// dispatcher writes a structured `Bye` and closes the
    /// connection — the OOM safety property holds, the wire error
    /// is named, and the client gets a clean close instead of a
    /// bare reset.
    FrameDecodeAborted {
        conn_id: ConnId,
        error: RpcError,
    },
}

/// Per-connection state owned by the cap dispatcher. The reader
/// sidecar holds `shutdown` + a clone of `write_half` for the
/// reader-side socket (each thread owns one half of the split).
pub(super) struct ConnState {
    pub(super) peer: SocketAddr,
    /// Dispatcher's half — used for inline writes (`HelloAck`,
    /// `ReplyEvent`, `ReplyEnd`, Pong, Bye).
    pub(super) write_half: TcpStream,
    /// Reader thread's shutdown flag. Cap flips it + shuts down
    /// the read half to wake the blocked `read()`.
    pub(super) shutdown: Arc<AtomicBool>,
    /// Reader thread handle. Joined in `unwire`.
    pub(super) reader_thread: Option<JoinHandle<()>>,
    pub(super) hello_received: bool,
}

/// Per-connection reader thread body. Reads frames from
/// `read_half` and pushes them onto `inbound_tx`; on an oversize
/// inbound frame (`FrameError::FrameTooLarge`) drains the body if
/// it's inside the drain ceiling so the connection survives, or
/// asks the dispatcher to close it with a structured `Bye` if not
/// (iamacoffeepot/aether#1271). Returns when the connection closes
/// (peer EOF, read error, shutdown flag, oversize-abort).
pub(super) fn run_reader_loop(
    read_half: TcpStream,
    conn_id: ConnId,
    shutdown: &AtomicBool,
    inbound_tx: &mpsc::Sender<InboundEvent>,
    mailer: &Arc<Mailer>,
    self_id: MailboxId,
    wake_kind: KindId,
) {
    let mut reader = BufReader::new(read_half);
    loop {
        if shutdown.load(Ordering::Acquire) {
            return;
        }
        match read_frame(&mut reader) {
            Ok(frame) => {
                if inbound_tx
                    .send(InboundEvent::FrameReceived { conn_id, frame })
                    .is_err()
                {
                    return;
                }
                mailer.push(Mail::new(
                    self_id,
                    wake_kind,
                    RpcInboundReady::default().encode_into_bytes(),
                    1,
                ));
            }
            Err(FrameError::Io(io_err)) if io_err.kind() == io::ErrorKind::UnexpectedEof => {
                let _ = inbound_tx.send(InboundEvent::ReaderClosed {
                    conn_id,
                    reason: "eof".into(),
                });
                mailer.push(Mail::new(
                    self_id,
                    wake_kind,
                    RpcInboundReady::default().encode_into_bytes(),
                    1,
                ));
                return;
            }
            Err(FrameError::FrameTooLarge { size, max }) => {
                let outcome = handle_oversize_frame(
                    &mut reader,
                    conn_id,
                    size,
                    max,
                    inbound_tx,
                    mailer,
                    self_id,
                    wake_kind,
                );
                if outcome.is_terminal() {
                    return;
                }
            }
            Err(e) => {
                if shutdown.load(Ordering::Acquire) {
                    return;
                }
                let _ = inbound_tx.send(InboundEvent::ReaderClosed {
                    conn_id,
                    reason: format!("read error: {e}"),
                });
                mailer.push(Mail::new(
                    self_id,
                    wake_kind,
                    RpcInboundReady::default().encode_into_bytes(),
                    1,
                ));
                return;
            }
        }
    }
}

/// Outcome of [`handle_oversize_frame`]. `Continue` means the
/// reader resumed frame-sync; `Terminal` means the read loop must
/// exit (drain failed, partial drain, or oversize-abort the
/// dispatcher is closing the connection for).
enum OversizeOutcome {
    Continue,
    Terminal,
}

impl OversizeOutcome {
    fn is_terminal(&self) -> bool {
        matches!(self, Self::Terminal)
    }
}

/// Body half of [`run_reader_loop`]'s `FrameTooLarge` arm: when
/// the inbound length prefix exceeds the cap, either drain the
/// body (if `size <= 2 * max`) so the stream re-syncs and the
/// connection survives, or post a structured-abort event that
/// asks the dispatcher to close the connection with a `Bye`.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::cast_possible_truncation)]
fn handle_oversize_frame(
    reader: &mut BufReader<TcpStream>,
    conn_id: ConnId,
    size: usize,
    max: usize,
    inbound_tx: &mpsc::Sender<InboundEvent>,
    mailer: &Arc<Mailer>,
    self_id: MailboxId,
    wake_kind: KindId,
) -> OversizeOutcome {
    let drain_ceiling = max.saturating_mul(2);
    if size > drain_ceiling {
        let event = InboundEvent::FrameDecodeAborted {
            conn_id,
            error: RpcError::FrameTooLarge {
                size: size as u64,
                max: max as u64,
            },
        };
        if inbound_tx.send(event).is_err() {
            return OversizeOutcome::Terminal;
        }
        mailer.push(Mail::new(
            self_id,
            wake_kind,
            RpcInboundReady::default().encode_into_bytes(),
            1,
        ));
        return OversizeOutcome::Terminal;
    }
    // `take(size)` bounds the drain so a racy / lying peer can't
    // push us past the cap-2x ceiling on bytes we'll allocate
    // scratch for.
    let mut drain = io::sink();
    let mut bounded = io::Read::take(reader, size as u64);
    let Ok(drained) = io::copy(&mut bounded, &mut drain) else {
        let _ = inbound_tx.send(InboundEvent::ReaderClosed {
            conn_id,
            reason: format!("frame too large drain failed: {size} > {max}"),
        });
        mailer.push(Mail::new(
            self_id,
            wake_kind,
            RpcInboundReady::default().encode_into_bytes(),
            1,
        ));
        return OversizeOutcome::Terminal;
    };
    if (drained as usize) != size {
        // Peer hung up mid-body.
        let _ = inbound_tx.send(InboundEvent::ReaderClosed {
            conn_id,
            reason: format!("frame too large partial drain: {drained}/{size}"),
        });
        mailer.push(Mail::new(
            self_id,
            wake_kind,
            RpcInboundReady::default().encode_into_bytes(),
            1,
        ));
        return OversizeOutcome::Terminal;
    }
    let event = InboundEvent::FrameDecodeError {
        conn_id,
        error: RpcError::FrameTooLarge {
            size: size as u64,
            max: max as u64,
        },
    };
    if inbound_tx.send(event).is_err() {
        return OversizeOutcome::Terminal;
    }
    mailer.push(Mail::new(
        self_id,
        wake_kind,
        RpcInboundReady::default().encode_into_bytes(),
        1,
    ));
    OversizeOutcome::Continue
}
