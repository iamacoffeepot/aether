//! Fleet-runtime helpers for the engines cap: settle a routed call the
//! cap can't satisfy, pick a free localhost RPC port, and resolve the
//! per-engine spawn-dir parent. Native-only (sockets, process env,
//! mail pushes).

use crate::engine::kinds::CallSettled;
use aether_data::{Kind, MailboxId};
use aether_substrate::Mail;
use aether_substrate::mail::mailer::Mailer;
use aether_substrate::mail::{Source, SourceAddr};
use std::env;
use std::io;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;

/// Env override for the parent directory under which the cap
/// allocates per-engine handle-store dirs (issue 1274). Absent →
/// fall through to `dirs::data_dir().join("aether/engines")`, then
/// to `std::env::temp_dir().join("aether-engines")` if no data dir
/// is resolvable.
const ENV_ENGINE_STORE_ROOT: &str = "AETHER_ENGINE_STORE_ROOT";

/// Push a `CallSettled::Err` back to `target` (correlation
/// preserved) so a routed call that the cap can't satisfy — bad
/// `engine_id`, unknown engine — closes with a wire `ReplyEnd`
/// instead of leaving the RPC client hanging.
pub(super) fn settle_err(mailer: &Arc<Mailer>, target: MailboxId, correlation: u64, error: String) {
    mailer.push(
        Mail::new(
            target,
            <CallSettled as Kind>::ID,
            CallSettled::Err { error }.encode_into_bytes(),
            1,
        )
        .with_reply_to(Source::with_correlation(SourceAddr::None, correlation)),
    );
}

/// Bind `127.0.0.1:0`, read the OS-assigned port, drop the
/// listener. A tiny TOCTOU window exists before the substrate
/// rebinds the port, but on localhost it's negligible — and this
/// sidesteps both a wire change to report an ephemeral port back
/// from the substrate and an un-recycled incrementing port pool.
pub(super) fn free_local_port() -> io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// Parent directory under which the cap allocates per-engine
/// handle-store dirs (issue 1274). Priority:
///
/// 1. `AETHER_ENGINE_STORE_ROOT` env override (ops escape hatch).
/// 2. `dirs::data_dir().join("aether/engines")` (cross-platform
///    default — `~/Library/Application Support/aether/engines` on
///    macOS, `$XDG_DATA_HOME/aether/engines` on Linux, etc.).
/// 3. `std::env::temp_dir().join("aether-engines")` if no data
///    dir is resolvable.
// External ops escape hatch (AETHER_ENGINE_STORE_ROOT) for the per-engine
// spawn-dir parent — the directory forked substrates and their handle
// stores live under, resolved in a static spawn helper. #1968 deliberately
// kept this knob inline (separate from the binary-artifact store, which it
// moved onto EngineConfig); it is a process-level deployment override, not
// a cap config field.
#[allow(clippy::disallowed_methods)]
pub(super) fn engine_store_root() -> PathBuf {
    if let Ok(raw) = env::var(ENV_ENGINE_STORE_ROOT)
        && !raw.is_empty()
    {
        return PathBuf::from(raw);
    }
    if let Some(data) = dirs::data_dir() {
        return data.join("aether").join("engines");
    }
    env::temp_dir().join("aether-engines")
}
