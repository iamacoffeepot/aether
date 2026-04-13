// Substrate-side hub client. Dials an `aether-hub`, performs the
// Hello/Welcome handshake, then runs two background threads:
//
//   - a reader that blocks on `HubToEngine` frames and funnels inbound
//     `Mail` into the scheduler's `MailQueue` after resolving the
//     recipient and kind against the local `Registry`,
//   - a heartbeat writer that sends `EngineToHub::Heartbeat` on a fixed
//     cadence so the hub doesn't reap this connection.
//
// Graceful shutdown is deliberately V0-minimal: when the substrate
// process exits, the OS closes the TCP socket and both threads drop
// out of their respective read/write calls. Per ADR-0006's "substrate
// stays sync" note, this module avoids `tokio` and uses the sync
// framing helpers from `aether-hub-protocol`.

use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::process;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aether_hub_protocol::{
    EngineId, EngineToHub, Hello, HubToEngine, MailFrame, read_frame, write_frame,
};

use crate::mail::Mail;
use crate::queue::MailQueue;
use crate::registry::Registry;

/// Cadence at which this client emits `Heartbeat` to the hub. Must be
/// comfortably below the hub's read timeout (15s) so a single missed
/// tick doesn't trip reaping.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// Live hub connection. Threads are retained so their join handles
/// aren't dropped; they exit when the TCP socket closes.
pub struct HubClient {
    pub engine_id: EngineId,
    _reader: JoinHandle<()>,
    _heartbeat: JoinHandle<()>,
}

impl HubClient {
    /// Dial `addr`, send `Hello`, receive `Welcome`, and spawn the
    /// reader + heartbeat threads. Inbound `Mail` is resolved and
    /// pushed onto `queue`; unknown recipient or kind names are logged
    /// and dropped.
    pub fn connect<A: ToSocketAddrs>(
        addr: A,
        name: impl Into<String>,
        version: impl Into<String>,
        registry: Arc<Registry>,
        queue: Arc<MailQueue>,
    ) -> io::Result<Self> {
        let mut stream = TcpStream::connect(addr)?;
        let hello = EngineToHub::Hello(Hello {
            name: name.into(),
            pid: process::id(),
            started_unix: unix_now(),
            version: version.into(),
        });
        write_frame(&mut stream, &hello).map_err(io::Error::other)?;

        let welcome: HubToEngine = read_frame(&mut stream).map_err(io::Error::other)?;
        let engine_id = match welcome {
            HubToEngine::Welcome(w) => w.engine_id,
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("expected Welcome, got {other:?}"),
                ));
            }
        };
        eprintln!("aether-substrate: hub registered as engine {}", engine_id.0);

        let reader_stream = stream.try_clone()?;
        let heartbeat_stream = stream;
        let _reader = thread::spawn(move || run_reader(reader_stream, registry, queue));
        let _heartbeat = thread::spawn(move || run_heartbeat(heartbeat_stream));

        Ok(Self {
            engine_id,
            _reader,
            _heartbeat,
        })
    }
}

fn run_reader(mut stream: TcpStream, registry: Arc<Registry>, queue: Arc<MailQueue>) {
    loop {
        match read_frame::<_, HubToEngine>(&mut stream) {
            Ok(HubToEngine::Mail(frame)) => dispatch_mail(frame, &registry, &queue),
            Ok(HubToEngine::Heartbeat) => {}
            Ok(HubToEngine::Welcome(_)) => {
                eprintln!("aether-substrate: unexpected post-handshake Welcome, ignoring");
            }
            Ok(HubToEngine::Goodbye(g)) => {
                eprintln!("aether-substrate: hub Goodbye: {}", g.reason);
                return;
            }
            Err(e) => {
                eprintln!("aether-substrate: hub read error: {e}");
                return;
            }
        }
    }
}

fn run_heartbeat(mut stream: TcpStream) {
    loop {
        thread::sleep(HEARTBEAT_INTERVAL);
        if write_frame(&mut stream, &EngineToHub::Heartbeat).is_err() {
            return;
        }
    }
}

fn dispatch_mail(frame: MailFrame, registry: &Registry, queue: &MailQueue) {
    let Some(recipient) = registry.lookup(&frame.recipient_name) else {
        eprintln!(
            "aether-substrate: dropping hub mail to unknown mailbox {:?}",
            frame.recipient_name
        );
        return;
    };
    let Some(kind) = registry.kind_id(&frame.kind_name) else {
        eprintln!(
            "aether-substrate: dropping hub mail of unknown kind {:?}",
            frame.kind_name
        );
        return;
    };
    queue.push(Mail::new(recipient, kind, frame.payload, frame.count));
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
