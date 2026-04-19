// Integration tests for the engine-facing TCP listener. Each test
// binds port 0 so they can run in parallel.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use aether_hub::{EngineRegistry, LogStore, PendingSpawns, SessionRegistry, run_engine_listener};
use aether_hub_protocol::{EngineToHub, Goodbye, Hello, HubToEngine, encode_frame};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn spawn_hub() -> (SocketAddr, EngineRegistry, SessionRegistry) {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let registry = EngineRegistry::new();
    let sessions = SessionRegistry::new();
    let pending = PendingSpawns::new();
    tokio::spawn(run_engine_listener(
        addr,
        registry.clone(),
        sessions.clone(),
        pending,
        LogStore::new(),
    ));
    // Give the listener a beat to bind.
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, registry, sessions)
}

async fn connect(addr: SocketAddr) -> TcpStream {
    TcpStream::connect(addr).await.unwrap()
}

async fn read_frame_async<T: serde::de::DeserializeOwned>(r: &mut TcpStream) -> T {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await.unwrap();
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await.unwrap();
    postcard::from_bytes(&buf).unwrap()
}

async fn write_frame_async<T: serde::Serialize>(w: &mut TcpStream, msg: &T) {
    let bytes = encode_frame(msg);
    w.write_all(&bytes).await.unwrap();
}

fn hello(name: &str) -> EngineToHub {
    EngineToHub::Hello(Hello {
        name: name.into(),
        pid: 1,
        started_unix: 0,
        version: "test".into(),
        kinds: vec![],
    })
}

#[tokio::test]
async fn handshake_assigns_engine_id_and_registers() {
    let (addr, registry, _sessions) = spawn_hub().await;
    let mut stream = connect(addr).await;

    write_frame_async(&mut stream, &hello("test")).await;
    let reply: HubToEngine = read_frame_async(&mut stream).await;
    let engine_id = match reply {
        HubToEngine::Welcome(w) => w.engine_id,
        other => panic!("expected Welcome, got {other:?}"),
    };

    // Registration is visible to the rest of the hub.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let engines = registry.list();
    assert_eq!(engines.len(), 1);
    assert_eq!(engines[0].id, engine_id);
    assert_eq!(engines[0].name, "test");
}

#[tokio::test]
async fn goodbye_deregisters() {
    let (addr, registry, _sessions) = spawn_hub().await;
    let mut stream = connect(addr).await;

    write_frame_async(&mut stream, &hello("bye")).await;
    let _: HubToEngine = read_frame_async(&mut stream).await;
    assert_eq!(registry.len(), 1);

    write_frame_async(
        &mut stream,
        &EngineToHub::Goodbye(Goodbye {
            reason: "test done".into(),
        }),
    )
    .await;

    // Give the hub a moment to tear down.
    for _ in 0..50 {
        if registry.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("engine still registered after Goodbye");
}

#[tokio::test]
async fn non_hello_first_frame_is_rejected() {
    let (addr, registry, _sessions) = spawn_hub().await;
    let mut stream = connect(addr).await;

    write_frame_async(&mut stream, &EngineToHub::Heartbeat).await;

    // Hub drops the connection; a subsequent read yields EOF.
    let mut buf = [0u8; 1];
    let n = stream.read(&mut buf).await.unwrap_or(0);
    assert_eq!(n, 0, "expected EOF after bad first frame");
    assert!(registry.is_empty());
}

#[tokio::test]
async fn hub_sends_periodic_heartbeats() {
    // This test relies on HEARTBEAT_INTERVAL being short enough that a
    // heartbeat arrives within a few seconds.
    let (addr, _registry, _sessions) = spawn_hub().await;
    let mut stream = connect(addr).await;
    write_frame_async(&mut stream, &hello("hb")).await;
    let _: HubToEngine = read_frame_async(&mut stream).await;

    // Keep sending heartbeats so the hub doesn't reap us, and wait for
    // a heartbeat from the hub side.
    let got_heartbeat = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            // Keepalive so the hub's read timeout doesn't fire.
            write_frame_async(&mut stream, &EngineToHub::Heartbeat).await;
            let frame: HubToEngine = read_frame_async(&mut stream).await;
            if matches!(frame, HubToEngine::Heartbeat) {
                return true;
            }
        }
    })
    .await;
    assert!(got_heartbeat.unwrap());
}

#[tokio::test]
async fn silent_engine_is_reaped() {
    let (addr, registry, _sessions) = spawn_hub().await;
    let mut stream = connect(addr).await;
    write_frame_async(&mut stream, &hello("silent")).await;
    let _: HubToEngine = read_frame_async(&mut stream).await;
    assert_eq!(registry.len(), 1);

    // Don't send any more frames; hub reads should time out.
    // READ_TIMEOUT is 15s. Wait up to 20s.
    for _ in 0..200 {
        if registry.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("silent engine was not reaped");
}
