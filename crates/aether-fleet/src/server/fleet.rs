//! Fleet-runtime helpers for the engines cap: settle a routed call the
//! cap can't satisfy, pick a free localhost RPC port, resolve the
//! per-engine spawn-dir parent, and reclaim the per-engine dirs under it.
//! Native-only (sockets, process env, mail pushes).

use aether_data::{EngineId, Kind, MailboxId, Uuid};
use aether_rpc::CallSettled;
use aether_substrate::Mail;
use aether_substrate::mail::mailer::Mailer;
use aether_substrate::mail::{Source, SourceAddr};
use std::env;
use std::fs;
use std::io;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
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

/// The dir a spawn materializes one engine's binary into: `engine_id`'s
/// hyphenless uuid under the fleet store root. The single place that shape
/// is written, so the fork and the reap cannot name different dirs.
pub fn engine_dir(root: &Path, engine_id: EngineId) -> PathBuf {
    root.join(engine_id.0.simple().to_string())
}

/// Reclaim every per-engine dir under `root` left behind by an earlier hub
/// process, returning how many were removed (issue 5502).
///
/// Run once at cap init, where "left behind" is unambiguous: a hub that has
/// just started supervises nothing, so every engine dir under its root
/// belongs to a process that is gone. It is the backstop for the deaths
/// that could not reclaim their own dir — a hub killed outright, or a host
/// that refuses to unlink a running image — not the mechanism; supervision
/// reaps its own dirs as engines leave.
///
/// Only entries whose name parses as a uuid are touched. The root is an
/// operator-settable path that can be pointed anywhere, so the sweep
/// removes strictly what this cap itself creates and steps over anything
/// else sharing the directory. Failures are logged and skipped: a dir that
/// resists removal is the status quo, never a boot failure.
///
/// Two hubs sharing one root would sweep each other's dirs here. That
/// configuration is already broken for an older reason — both mint engine
/// ids from 1, so they materialize into the same dir names — and the fix
/// for it is the per-hub root the config already offers, not a narrower
/// sweep.
pub fn sweep_engine_dirs(root: &Path) -> usize {
    let Ok(entries) = fs::read_dir(root) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter(|entry| entry.file_name().to_str().is_some_and(|name| Uuid::parse_str(name).is_ok()))
        .filter(|entry| match fs::remove_dir_all(entry.path()) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(
                    target: "aether_substrate::fleet_server",
                    error = ?e.kind(),
                    "engine store sweep: could not reclaim a leftover engine dir",
                );
                false
            }
        })
        .count()
}
