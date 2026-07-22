//! Fleet-runtime helpers for the engines cap: settle a routed call the
//! cap can't satisfy, pick a free localhost RPC port, and resolve the
//! per-engine spawn-dir parent. Native-only (sockets, process env,
//! mail pushes).

use aether_data::{Kind, MailboxId};
use aether_rpc::CallSettled;
use aether_substrate::Mail;
use aether_substrate::mail::mailer::Mailer;
use aether_substrate::mail::{Source, SourceAddr};
use std::env;
use std::io;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;

/// Push a `CallSettled::Err` back to `target` (correlation
/// preserved) so a routed call that the cap can't satisfy — bad
/// `engine_id`, unknown engine — closes with a wire `ReplyEnd`
/// instead of leaving the RPC client hanging.
pub fn settle_err(mailer: &Arc<Mailer>, target: MailboxId, correlation: u64, error: String) {
    mailer.push(
        Mail::new(target, <CallSettled as Kind>::ID, CallSettled::Err { error }.encode_into_bytes(), 1)
            .with_reply_to(Source::with_correlation(SourceAddr::None, correlation)),
    );
}

/// Bind `127.0.0.1:0`, read the OS-assigned port, drop the
/// listener. A tiny TOCTOU window exists before the substrate
/// rebinds the port, but on localhost it's negligible — and this
/// sidesteps both a wire change to report an ephemeral port back
/// from the substrate and an un-recycled incrementing port pool.
pub fn free_local_port() -> io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// Parent directory under which the cap allocates per-engine
/// handle-store dirs (issue 1274). Priority:
///
/// 1. `override_dir`, an explicit override (`FleetConfig::fleet_store_root`,
///    resolved from `AETHER_FLEET_STORE_ROOT` / `--hub-engine-store-root`
///    at `FleetServer::init` — the ops escape hatch).
/// 2. `dirs::data_dir().join("aether/engines")` (cross-platform
///    default — `~/Library/Application Support/aether/engines` on
///    macOS, `$XDG_DATA_HOME/aether/engines` on Linux, etc.).
/// 3. `std::env::temp_dir().join("aether-fleets")` if no data
///    dir is resolvable.
pub fn resolve_fleet_store_root(override_dir: Option<&str>) -> PathBuf {
    if let Some(dir) = override_dir.filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(data) = dirs::data_dir() {
        return data.join("aether").join("engines");
    }
    env::temp_dir().join("aether-fleets")
}
