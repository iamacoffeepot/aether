//! Startup-dial bring-up for the per-engine proxy: dial the substrate's
//! `RpcServerCapability`, retrying a refused connection while a
//! freshly-forked substrate comes up. Native-only (owns the outbound
//! `RpcConnection`).

use crate::rpc::{PeerKind, RpcClient, RpcClientError, RpcConnection, RpcInboundReady};
use aether_data::{Kind, KindId, MailboxId};
use aether_substrate::Mail;
use aether_substrate::mail::mailer::Mailer;
use std::io::ErrorKind;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Pause between dial attempts within the connect budget.
const PROXY_CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(50);

/// Dial the substrate's `RpcServerCapability`, building a fresh
/// `on_frame` wake closure per attempt. When `retry` is set, a
/// connection-refused / reset error is retried (after a short
/// pause) until the connect `budget` elapses — a freshly-forked
/// substrate may not have bound its port yet. `budget` of `None` is
/// the wait-forever sentinel: retry until the dial succeeds or hits
/// a terminal error. Handshake / frame errors are always terminal:
/// the peer answered, just wrongly.
pub(super) fn connect_proxy(
    addr: &str,
    mailer: &Arc<Mailer>,
    self_mailbox: MailboxId,
    wake_kind: KindId,
    retry: bool,
    budget: Option<Duration>,
) -> Result<RpcConnection, RpcClientError> {
    // `None` budget → no deadline (wait forever); `Some(d)` → stop
    // retrying once `d` has elapsed.
    let deadline = budget.map(|d| Instant::now() + d);
    loop {
        // The reader sidecar fires `RpcInboundReady` at the proxy's
        // own mailbox after every inbound frame so
        // `on_inbound_ready` drains `conn.inbound` on the
        // dispatcher thread. `RpcClient::connect` consumes the
        // closure, so a retry needs a fresh one.
        let wake_mailer = Arc::clone(mailer);
        let on_frame = move || {
            wake_mailer.push(Mail::new(
                self_mailbox,
                wake_kind,
                RpcInboundReady::default().encode_into_bytes(),
                1,
            ));
        };
        return match RpcClient::connect(
            addr,
            PeerKind::Client {
                client_name: "aether.engine.proxy".to_owned(),
                client_version: env!("CARGO_PKG_VERSION").to_owned(),
            },
            on_frame,
        ) {
            Ok(conn) => Ok(conn),
            Err(e) => {
                let within_budget = deadline.is_none_or(|d| Instant::now() < d);
                if retry && is_transient_connect_error(&e) && within_budget {
                    thread::sleep(PROXY_CONNECT_RETRY_INTERVAL);
                    continue;
                }
                Err(e)
            }
        };
    }
}

/// `true` for the connection-level errors a still-coming-up
/// substrate produces — worth retrying. Handshake / frame errors
/// mean the peer answered wrongly: terminal, never retried.
fn is_transient_connect_error(e: &RpcClientError) -> bool {
    matches!(
        e,
        RpcClientError::Connect(io)
            if matches!(io.kind(), ErrorKind::ConnectionRefused | ErrorKind::ConnectionReset)
    )
}
