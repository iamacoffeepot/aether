//! Real-process load and churn coverage for both `aether.tcp` session
//! lineages. Correctness is exact; timing and handler-cost values are emitted
//! only as diagnostics in `target/fleetbench-metrics/tcp-load.json`.

mod fleetbench;

mod tests {
    use std::collections::BTreeSet;
    use std::env::{self, VarError};
    use std::fs;
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::path::Path;
    use std::process::Command;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use aether_capabilities::tcp::{
        BindListener, BindListenerResult, ListListeners, ListListenersResult, SessionDataReady, SessionWrite,
        UnbindListener, UnbindListenerResult,
    };
    use aether_codec::frame::max_frame_size;
    use aether_data::{EngineId, Kind};
    use aether_kinds::{CostRow, CostTail, CostTailResult};
    use aether_test_fixtures_kinds::{
        CollectTcpLoadSnapshot, ConfigureTcpLoadProbe, StartTcpConnectLoad, TcpLoadSessionSnapshot, TcpLoadSnapshot,
        TcpLoadTopology,
    };
    use serde::Serialize;

    use crate::fleetbench::{FleetBench, dist_component_available, poll_until};

    const FIXTURE_STEM: &str = "aether_test_fixtures_bundle";
    const FIXTURE_EXPORT: &str = "test.tcp_load_probe";
    const LISTENER_NAME: &str = "fleetbench-tcp-load";

    const DEFAULT_CONNECTIONS: usize = 3;
    const DEFAULT_FRAMES_PER_CONNECTION: usize = 8;
    const DEFAULT_FRAME_BYTES: usize = 256;
    const DEFAULT_CHURN_ROUNDS: usize = 2;
    const DEFAULT_COMPLETION_TIMEOUT_MILLIS: u64 = 15_000;

    const MAX_CONNECTIONS: usize = 32;
    const MAX_FRAMES_PER_CONNECTION: usize = 1_024;
    const MAX_FRAME_BYTES: usize = 1024 * 1024;
    const MAX_CHURN_ROUNDS: usize = 16;
    const MAX_COMPLETION_TIMEOUT_MILLIS: u64 = 120_000;
    const MAX_TOTAL_FRAMES: usize = 100_000;
    const MAX_TOTAL_PAYLOAD_BYTES: usize = 128 * 1024 * 1024;

    #[derive(Clone, Debug, Serialize)]
    struct TcpLoadProfile {
        connections: usize,
        frames_per_connection: usize,
        frame_bytes: usize,
        churn_rounds: usize,
        completion_timeout_millis: u64,
    }

    impl TcpLoadProfile {
        fn resolve() -> Self {
            let profile = Self {
                connections: strict_usize("AETHER_TCP_LOAD_CONNECTIONS", DEFAULT_CONNECTIONS, MAX_CONNECTIONS),
                frames_per_connection: strict_usize(
                    "AETHER_TCP_LOAD_FRAMES_PER_CONNECTION",
                    DEFAULT_FRAMES_PER_CONNECTION,
                    MAX_FRAMES_PER_CONNECTION,
                ),
                frame_bytes: strict_usize("AETHER_TCP_LOAD_FRAME_BYTES", DEFAULT_FRAME_BYTES, MAX_FRAME_BYTES),
                churn_rounds: strict_usize("AETHER_TCP_LOAD_CHURN_ROUNDS", DEFAULT_CHURN_ROUNDS, MAX_CHURN_ROUNDS),
                completion_timeout_millis: strict_u64(
                    "AETHER_TCP_LOAD_COMPLETION_TIMEOUT_MILLIS",
                    DEFAULT_COMPLETION_TIMEOUT_MILLIS,
                    MAX_COMPLETION_TIMEOUT_MILLIS,
                ),
            };
            assert!(
                profile.frame_bytes < max_frame_size(),
                "AETHER_TCP_LOAD_FRAME_BYTES={} must remain below the active AETHER_MAX_FRAME_SIZE={} cap",
                profile.frame_bytes,
                max_frame_size(),
            );

            let throughput_frames = profile
                .connections
                .checked_mul(profile.frames_per_connection)
                .and_then(|value| value.checked_mul(2))
                .expect("tcp load throughput frame total overflowed usize");
            let churn_frames = profile
                .connections
                .checked_mul(profile.churn_rounds)
                .and_then(|value| value.checked_mul(2))
                .expect("tcp load churn frame total overflowed usize");
            let total_frames =
                throughput_frames.checked_add(churn_frames).expect("tcp load total frame count overflowed usize");
            assert!(
                total_frames <= MAX_TOTAL_FRAMES,
                "resolved tcp load profile requests {total_frames} total frames; maximum is {MAX_TOTAL_FRAMES}",
            );
            let total_payload_bytes =
                total_frames.checked_mul(profile.frame_bytes).expect("tcp load total payload bytes overflowed usize");
            assert!(
                total_payload_bytes <= MAX_TOTAL_PAYLOAD_BYTES,
                "resolved tcp load profile requests {total_payload_bytes} payload bytes; maximum is \
                 {MAX_TOTAL_PAYLOAD_BYTES}",
            );
            profile
        }

        fn timeout(&self) -> Duration {
            Duration::from_millis(self.completion_timeout_millis)
        }
    }

    #[allow(clippy::disallowed_methods)]
    fn strict_usize(name: &str, default: usize, maximum: usize) -> usize {
        match env::var(name) {
            Ok(raw) => {
                let value =
                    raw.parse::<usize>().unwrap_or_else(|_| panic!("{name} must be a positive integer, got {raw:?}"));
                assert!(value > 0, "{name} must be greater than zero");
                assert!(value <= maximum, "{name}={value} exceeds the maximum {maximum}");
                value
            }
            Err(VarError::NotPresent) => default,
            Err(error) => panic!("{name} could not be read: {error}"),
        }
    }

    #[allow(clippy::disallowed_methods)]
    fn strict_u64(name: &str, default: u64, maximum: u64) -> u64 {
        match env::var(name) {
            Ok(raw) => {
                let value =
                    raw.parse::<u64>().unwrap_or_else(|_| panic!("{name} must be a positive integer, got {raw:?}"));
                assert!(value > 0, "{name} must be greater than zero");
                assert!(value <= maximum, "{name}={value} exceeds the maximum {maximum}");
                value
            }
            Err(VarError::NotPresent) => default,
            Err(error) => panic!("{name} could not be read: {error}"),
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum SocketWorkerTopology {
        Accepted,
        Outbound,
    }

    impl SocketWorkerTopology {
        const fn marker(self) -> u8 {
            match self {
                Self::Accepted => 0xA5,
                Self::Outbound => 0x5A,
            }
        }
    }

    #[derive(Clone, Debug)]
    struct SocketWorkerTraffic {
        connection_index: usize,
        successful_frame_count: usize,
        successful_payload_bytes: usize,
        round_trip_micros: Vec<u64>,
    }

    #[derive(Debug)]
    struct SocketWorkerResult {
        traffic: SocketWorkerTraffic,
        socket_eof: bool,
    }

    struct RunningSocketWorkers {
        ready_rx: mpsc::Receiver<SocketWorkerTraffic>,
        release: Vec<mpsc::Sender<()>>,
        joins: Vec<thread::JoinHandle<SocketWorkerResult>>,
    }

    impl RunningSocketWorkers {
        fn await_traffic(&self, connection_count: usize, timeout: Duration) -> Vec<SocketWorkerTraffic> {
            let deadline = Instant::now() + timeout;
            let mut traffic = Vec::with_capacity(connection_count);
            while traffic.len() < connection_count {
                let remaining = deadline.saturating_duration_since(Instant::now());
                traffic.push(
                    self.ready_rx.recv_timeout(remaining).unwrap_or_else(|error| {
                        panic!("socket workers did not finish traffic before deadline: {error}")
                    }),
                );
            }
            traffic.sort_by_key(|result| result.connection_index);
            traffic
        }

        fn close_and_join(self) -> Vec<SocketWorkerResult> {
            for release in self.release {
                release.send(()).expect("socket worker remains live until close release");
            }
            self.joins.into_iter().map(|join| join.join().expect("socket worker completes without panic")).collect()
        }
    }

    fn connect_accepted_streams(port: u16, connection_count: usize) -> Vec<TcpStream> {
        (0..connection_count)
            .map(|_| TcpStream::connect(("127.0.0.1", port)).expect("connect accepted worker"))
            .collect()
    }

    fn start_outbound_workers(
        listener: &TcpListener,
        connection_count: usize,
        frame_count: usize,
        frame_bytes: usize,
        timeout: Duration,
    ) -> RunningSocketWorkers {
        let streams = (0..connection_count).map(|_| listener.accept().expect("accept outbound probe connection").0);
        start_workers(streams, SocketWorkerTopology::Outbound, frame_count, frame_bytes, timeout)
    }

    #[allow(clippy::disallowed_methods, reason = "test-owned blocking loopback peers run outside the actor runtime")]
    fn start_workers(
        streams: impl Iterator<Item = TcpStream>,
        topology: SocketWorkerTopology,
        frame_count: usize,
        frame_bytes: usize,
        timeout: Duration,
    ) -> RunningSocketWorkers {
        let (ready_tx, ready_rx) = mpsc::channel();
        let mut release = Vec::new();
        let mut joins = Vec::new();
        for (connection_index, stream) in streams.enumerate() {
            let (release_tx, release_rx) = mpsc::channel();
            release.push(release_tx);
            let ready_tx = ready_tx.clone();
            joins.push(thread::spawn(move || {
                run_socket_worker(
                    stream,
                    topology,
                    connection_index,
                    frame_count,
                    frame_bytes,
                    timeout,
                    &ready_tx,
                    &release_rx,
                )
            }));
        }
        drop(ready_tx);
        RunningSocketWorkers { ready_rx, release, joins }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_socket_worker(
        mut stream: TcpStream,
        topology: SocketWorkerTopology,
        connection_index: usize,
        frame_count: usize,
        frame_bytes: usize,
        timeout: Duration,
        ready_tx: &mpsc::Sender<SocketWorkerTraffic>,
        release_rx: &mpsc::Receiver<()>,
    ) -> SocketWorkerResult {
        stream.set_read_timeout(Some(timeout)).expect("set socket worker read timeout");
        stream.set_write_timeout(Some(timeout)).expect("set socket worker write timeout");
        stream.set_nodelay(true).expect("disable loopback Nagle delay");

        let mut round_trip_micros = Vec::with_capacity(frame_count);
        for frame_index in 0..frame_count {
            let body = frame_body(topology, connection_index, frame_index, frame_bytes);
            let started = Instant::now();
            write_body_frame(&mut stream, &body);
            let echoed = read_body_frame(&mut stream);
            round_trip_micros.push(micros(started.elapsed()));
            assert_eq!(echoed, body, "socket worker receives the exact body it sent");
        }

        let traffic = SocketWorkerTraffic {
            connection_index,
            successful_frame_count: frame_count,
            successful_payload_bytes: frame_count
                .checked_mul(frame_bytes)
                .expect("per-worker payload total fits usize"),
            round_trip_micros,
        };
        ready_tx.send(traffic.clone()).expect("load scenario waits for worker traffic");
        release_rx.recv_timeout(timeout).expect("load scenario releases worker before timeout");

        stream.shutdown(Shutdown::Write).expect("worker half-closes its write side");
        let mut trailing = [0_u8; 1];
        let socket_eof = stream.read(&mut trailing).expect("worker observes peer close") == 0;
        SocketWorkerResult { traffic, socket_eof }
    }

    fn frame_body(
        topology: SocketWorkerTopology,
        connection_index: usize,
        frame_index: usize,
        frame_bytes: usize,
    ) -> Vec<u8> {
        let connection = u64::try_from(connection_index).expect("connection index fits u64").to_le_bytes();
        let frame = u64::try_from(frame_index).expect("frame index fits u64").to_le_bytes();
        let mut body = Vec::with_capacity(frame_bytes);
        body.push(topology.marker());
        body.extend_from_slice(&connection);
        body.extend_from_slice(&frame);
        body.truncate(frame_bytes);
        while body.len() < frame_bytes {
            let offset = u8::try_from(body.len() % 251).expect("modulo result fits u8");
            body.push(topology.marker().wrapping_add(offset));
        }
        body
    }

    fn write_body_frame(stream: &mut TcpStream, body: &[u8]) {
        let frame_bytes = u32::try_from(body.len()).expect("tcp load body fits four-byte prefix");
        stream.write_all(&frame_bytes.to_le_bytes()).expect("write tcp load frame prefix");
        stream.write_all(body).expect("write tcp load frame body");
    }

    fn read_body_frame(stream: &mut TcpStream) -> Vec<u8> {
        let mut prefix = [0_u8; 4];
        stream.read_exact(&mut prefix).expect("read echoed frame prefix");
        let frame_bytes = u32::from_le_bytes(prefix) as usize;
        assert!(frame_bytes < max_frame_size(), "echoed frame remains below the active cap");
        let mut body = vec![0_u8; frame_bytes];
        stream.read_exact(&mut body).expect("read echoed frame body");
        body
    }

    fn micros(duration: Duration) -> u64 {
        u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
    }

    #[derive(Clone, Copy, Debug, Serialize)]
    enum TcpLoadMetricPhase {
        AcceptedThroughput,
        OutboundThroughput,
        Churn,
    }

    #[derive(Clone, Debug, Serialize)]
    #[allow(clippy::struct_field_names, reason = "serialized diagnostic fields spell their microsecond unit")]
    struct RoundTripLatencyMicros {
        minimum_micros: u64,
        median_micros: u64,
        p95_micros: u64,
        maximum_micros: u64,
    }

    #[derive(Clone, Debug, Serialize)]
    struct TcpHandlerCostMetrics {
        session_path: String,
        kind_name: String,
        mean_nanos: u64,
        mad_nanos: u64,
        samples: u64,
    }

    #[derive(Clone, Debug, Serialize)]
    struct TcpLoadPhaseMetrics {
        phase: TcpLoadMetricPhase,
        successful_connection_count: usize,
        successful_frame_count: usize,
        successful_payload_bytes: usize,
        elapsed_micros: u64,
        payload_bytes_per_second: f64,
        connections_per_second: f64,
        round_trip_latency_micros: RoundTripLatencyMicros,
        handler_costs: Vec<TcpHandlerCostMetrics>,
    }

    impl TcpLoadPhaseMetrics {
        fn from_traffic(
            phase: TcpLoadMetricPhase,
            elapsed: Duration,
            traffic: &[SocketWorkerTraffic],
            handler_costs: Vec<TcpHandlerCostMetrics>,
        ) -> Self {
            let successful_frame_count = traffic.iter().map(|worker| worker.successful_frame_count).sum();
            let successful_payload_bytes = traffic.iter().map(|worker| worker.successful_payload_bytes).sum();
            let round_trip_micros =
                traffic.iter().flat_map(|worker| worker.round_trip_micros.iter().copied()).collect::<Vec<_>>();
            let elapsed_seconds = elapsed.as_secs_f64().max(f64::EPSILON);
            Self {
                phase,
                successful_connection_count: traffic.len(),
                successful_frame_count,
                successful_payload_bytes,
                elapsed_micros: micros(elapsed),
                payload_bytes_per_second: f64::from(
                    u32::try_from(successful_payload_bytes).expect("bounded payload total fits u32"),
                ) / elapsed_seconds,
                connections_per_second: f64::from(
                    u32::try_from(traffic.len()).expect("bounded connection count fits u32"),
                ) / elapsed_seconds,
                round_trip_latency_micros: latency_summary(round_trip_micros),
                handler_costs,
            }
        }
    }

    fn latency_summary(mut samples_micros: Vec<u64>) -> RoundTripLatencyMicros {
        assert!(!samples_micros.is_empty(), "every load phase records round-trip samples");
        samples_micros.sort_unstable();
        let p95_index = (samples_micros.len() * 95).div_ceil(100).saturating_sub(1);
        RoundTripLatencyMicros {
            minimum_micros: samples_micros[0],
            median_micros: samples_micros[samples_micros.len() / 2],
            p95_micros: samples_micros[p95_index],
            maximum_micros: *samples_micros.last().expect("samples are non-empty"),
        }
    }

    #[derive(Debug, Serialize)]
    struct TcpLoadReport {
        build_revision: String,
        profile: TcpLoadProfile,
        phases: Vec<TcpLoadPhaseMetrics>,
    }

    fn snapshot(bench: &mut FleetBench, engine: EngineId, probe_addr: &str) -> TcpLoadSnapshot {
        let replies = bench.send(engine, probe_addr, &CollectTcpLoadSnapshot);
        let reply = match replies.as_slice() {
            [reply] => reply,
            other => panic!("CollectTcpLoadSnapshot expected one reply, got {}", other.len()),
        };
        assert_eq!(reply.kind, TcpLoadSnapshot::ID, "snapshot query replies with TcpLoadSnapshot");
        TcpLoadSnapshot::decode_from_bytes(&reply.payload).expect("decode TcpLoadSnapshot")
    }

    #[allow(clippy::too_many_arguments, reason = "phase assertion keeps its independent exact dimensions explicit")]
    fn wait_for_sessions(
        bench: &mut FleetBench,
        engine: EngineId,
        probe_addr: &str,
        topology: TcpLoadTopology,
        previous_names: &BTreeSet<String>,
        connection_count: usize,
        frame_count: usize,
        frame_bytes: usize,
    ) -> Vec<TcpLoadSessionSnapshot> {
        let expected_payload_bytes =
            u64::try_from(frame_count.checked_mul(frame_bytes).expect("expected per-session payload total fits usize"))
                .expect("expected per-session payload total fits u64");
        let expected_frames = u64::try_from(frame_count).expect("frame count fits u64");
        let mut matched = None;
        assert!(
            poll_until(|| {
                let current = snapshot(bench, engine, probe_addr);
                assert!(
                    current.connect_failures.is_empty(),
                    "outbound connect failures: {:?}",
                    current.connect_failures
                );
                let sessions = current
                    .sessions
                    .into_iter()
                    .filter(|session| session.topology == topology && !previous_names.contains(&session.session_name))
                    .collect::<Vec<_>>();
                if sessions.len() == connection_count
                    && sessions.iter().all(|session| {
                        session.established
                            && session.received_frame_count == expected_frames
                            && session.received_payload_bytes == expected_payload_bytes
                    })
                {
                    matched = Some(sessions);
                    true
                } else {
                    false
                }
            }),
            "probe did not report the exact {topology:?} session/frame/byte totals",
        );
        matched.expect("successful poll captured matching sessions")
    }

    fn session_names(snapshot: &TcpLoadSnapshot, topology: TcpLoadTopology) -> BTreeSet<String> {
        snapshot
            .sessions
            .iter()
            .filter(|session| session.topology == topology)
            .map(|session| session.session_name.clone())
            .collect()
    }

    fn next_accepted_index(snapshot: &TcpLoadSnapshot) -> u64 {
        snapshot
            .sessions
            .iter()
            .filter(|session| session.topology == TcpLoadTopology::Accepted)
            .filter_map(|session| session.session_name.strip_prefix("conn-")?.parse::<u64>().ok())
            .max()
            .map_or(0, |index| index + 1)
    }

    fn wait_for_live_accepted_paths(
        bench: &mut FleetBench,
        engine: EngineId,
        first_index: u64,
        connection_count: usize,
    ) {
        assert!(
            poll_until(|| {
                (0..connection_count).all(|offset| {
                    let offset = u64::try_from(offset).expect("connection offset fits u64");
                    let path = accepted_path(&format!("conn-{}", first_index + offset));
                    let replies = bench.send(engine, &path, &CostTail { kind: None });
                    matches!(replies.as_slice(), [reply] if reply.kind == CostTailResult::ID)
                })
            }),
            "accepted session paths did not become live before traffic release",
        );
    }

    fn accepted_path(session_name: &str) -> String {
        format!("aether.tcp/aether.tcp.listener:{LISTENER_NAME}/aether.tcp.session:{session_name}")
    }

    fn outbound_path(session_name: &str) -> String {
        format!("aether.tcp/aether.tcp.session:{session_name}")
    }

    fn sample_live_session(bench: &mut FleetBench, engine: EngineId, path: &str) -> Vec<TcpHandlerCostMetrics> {
        let replies = bench.send(engine, path, &CostTail { kind: None });
        let reply = match replies.as_slice() {
            [reply] => reply,
            other => panic!("live CostTail at {path:?} expected one reply, got {}", other.len()),
        };
        assert_eq!(reply.kind, CostTailResult::ID, "live CostTail replies with CostTailResult");
        let rows = match CostTailResult::decode_from_bytes(&reply.payload).expect("decode live CostTailResult") {
            CostTailResult::Ok { rows } => rows,
            CostTailResult::Err { error } => panic!("CostTail at {path:?} failed: {error}"),
        };
        [SessionDataReady::ID, SessionWrite::ID].into_iter().map(|kind| required_cost_row(&rows, kind, path)).collect()
    }

    fn required_cost_row(rows: &[CostRow], kind: aether_data::KindId, path: &str) -> TcpHandlerCostMetrics {
        let row = rows
            .iter()
            .find(|row| row.kind_id == kind)
            .unwrap_or_else(|| panic!("CostTail at {path:?} omitted required handler {kind:?}; rows: {rows:?}"));
        assert!(row.samples > 0, "CostTail handler {kind:?} at {path:?} must have executed");
        TcpHandlerCostMetrics {
            session_path: path.to_owned(),
            kind_name: row.kind_name.clone().unwrap_or_else(|| format!("kind-{}", kind.0)),
            mean_nanos: row.mean_nanos,
            mad_nanos: row.mad_nanos,
            samples: row.samples,
        }
    }

    fn assert_sessions_closed(
        bench: &mut FleetBench,
        engine: EngineId,
        probe_addr: &str,
        sessions: &[TcpLoadSessionSnapshot],
    ) {
        let names = sessions.iter().map(|session| session.session_name.clone()).collect::<BTreeSet<_>>();
        assert!(
            poll_until(|| {
                snapshot(bench, engine, probe_addr)
                    .sessions
                    .iter()
                    .filter(|session| names.contains(&session.session_name))
                    .all(|session| session.closed)
            }),
            "probe did not record one close transition for every released session",
        );
    }

    fn assert_tombstoned(bench: &mut FleetBench, engine: EngineId, path: &str) {
        let replies = bench.send(engine, path, &CostTail { kind: None });
        assert!(
            replies.is_empty(),
            "CostTail at formerly-live tombstoned path {path:?} must settle with zero replies, got {}",
            replies.len(),
        );
    }

    fn assert_worker_close(results: &[SocketWorkerResult], expected_frames: usize, expected_bytes: usize) {
        assert!(results.iter().all(|result| result.socket_eof), "every released peer observes socket EOF");
        assert!(
            results.iter().all(|result| {
                result.traffic.successful_frame_count == expected_frames
                    && result.traffic.successful_payload_bytes == expected_bytes
            }),
            "every worker reports exact completed frames and bytes",
        );
    }

    fn bind_listener(bench: &mut FleetBench, engine: EngineId, consumer: aether_data::MailboxId) -> u16 {
        let replies = bench.send(
            engine,
            "aether.tcp",
            &BindListener {
                addr: "127.0.0.1:0".to_owned(),
                name: Some(LISTENER_NAME.to_owned()),
                consumer: Some(consumer),
            },
        );
        let reply = match replies.as_slice() {
            [reply] => reply,
            other => panic!("BindListener expected one reply, got {}", other.len()),
        };
        match BindListenerResult::decode_from_bytes(&reply.payload).expect("decode BindListenerResult") {
            BindListenerResult::Ok { listener_name, local_port, .. } => {
                assert_eq!(listener_name, LISTENER_NAME);
                local_port
            }
            BindListenerResult::Err { reason, .. } => panic!("BindListener failed: {reason}"),
        }
    }

    fn list_listeners(bench: &mut FleetBench, engine: EngineId) -> ListListenersResult {
        let replies = bench.send(engine, "aether.tcp", &ListListeners::default());
        let reply = match replies.as_slice() {
            [reply] => reply,
            other => panic!("ListListeners expected one reply, got {}", other.len()),
        };
        ListListenersResult::decode_from_bytes(&reply.payload).expect("decode ListListenersResult")
    }

    fn unbind_listener(bench: &mut FleetBench, engine: EngineId) {
        let replies = bench.send(engine, "aether.tcp", &UnbindListener { listener_name: LISTENER_NAME.to_owned() });
        let reply = match replies.as_slice() {
            [reply] => reply,
            other => panic!("UnbindListener expected one reply, got {}", other.len()),
        };
        match UnbindListenerResult::decode_from_bytes(&reply.payload).expect("decode UnbindListenerResult") {
            UnbindListenerResult::Ok { listener_name } => assert_eq!(listener_name, LISTENER_NAME),
            UnbindListenerResult::Err { reason, .. } => panic!("UnbindListener failed: {reason}"),
        }
    }

    #[allow(clippy::disallowed_methods, reason = "test artifact records the external CI revision when available")]
    fn build_revision() -> String {
        env::var("GITHUB_SHA").ok().filter(|revision| !revision.is_empty()).unwrap_or_else(|| {
            let output =
                Command::new("git").args(["rev-parse", "HEAD"]).output().expect("read build revision with git");
            assert!(output.status.success(), "git rev-parse HEAD succeeds");
            String::from_utf8(output.stdout).expect("git revision is utf8").trim().to_owned()
        })
    }

    #[allow(clippy::print_stdout, reason = "the load scenario intentionally prints its structured metrics")]
    fn write_report(report: &TcpLoadReport) {
        let json = serde_json::to_string_pretty(report).expect("serialize tcp load metrics");
        println!("{json}");
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/fleetbench-metrics/tcp-load.json");
        fs::create_dir_all(path.parent().expect("metrics path has parent")).expect("create fleetbench metrics dir");
        fs::write(&path, format!("{json}\n")).expect("write tcp load metrics artifact");
    }

    #[test]
    #[allow(clippy::too_many_lines, reason = "one scenario preserves the accepted/outbound/churn lifecycle ordering")]
    fn fleetbench_tcp_load_covers_accepted_outbound_and_churn() {
        if !dist_component_available(FIXTURE_STEM) {
            return;
        }
        let profile = TcpLoadProfile::resolve();
        let mut bench = FleetBench::start();
        let engine = bench.spawn_headless();
        let probe = bench.load_full_export(engine, FIXTURE_STEM, FIXTURE_EXPORT);
        assert!(list_listeners(&mut bench, engine).listeners.is_empty(), "listener baseline is empty");
        assert!(
            bench
                .send(engine, &probe.addr, &ConfigureTcpLoadProbe { listener_name: LISTENER_NAME.to_owned() },)
                .is_empty(),
            "probe configuration is fire-and-settle",
        );
        let local_port = bind_listener(&mut bench, engine, probe.mailbox_id);

        let accepted_baseline = snapshot(&mut bench, engine, &probe.addr);
        let accepted_before = session_names(&accepted_baseline, TcpLoadTopology::Accepted);
        let accepted_first_index = next_accepted_index(&accepted_baseline);
        let accepted_started = Instant::now();
        let accepted_streams = connect_accepted_streams(local_port, profile.connections);
        wait_for_live_accepted_paths(&mut bench, engine, accepted_first_index, profile.connections);
        let accepted_workers = start_workers(
            accepted_streams.into_iter(),
            SocketWorkerTopology::Accepted,
            profile.frames_per_connection,
            profile.frame_bytes,
            profile.timeout(),
        );
        let accepted_traffic = accepted_workers.await_traffic(profile.connections, profile.timeout());
        let accepted_elapsed = accepted_started.elapsed();
        let accepted_sessions = wait_for_sessions(
            &mut bench,
            engine,
            &probe.addr,
            TcpLoadTopology::Accepted,
            &accepted_before,
            profile.connections,
            profile.frames_per_connection,
            profile.frame_bytes,
        );
        let accepted_costs = accepted_sessions
            .iter()
            .flat_map(|session| sample_live_session(&mut bench, engine, &accepted_path(&session.session_name)))
            .collect();
        let accepted_results = accepted_workers.close_and_join();
        assert_worker_close(
            &accepted_results,
            profile.frames_per_connection,
            profile.frames_per_connection * profile.frame_bytes,
        );
        assert_sessions_closed(&mut bench, engine, &probe.addr, &accepted_sessions);
        for session in &accepted_sessions {
            assert_tombstoned(&mut bench, engine, &accepted_path(&session.session_name));
        }

        let outbound_listener = TcpListener::bind("127.0.0.1:0").expect("bind outbound load listener");
        let outbound_addr = outbound_listener.local_addr().expect("read outbound listener addr");
        let outbound_before = session_names(&snapshot(&mut bench, engine, &probe.addr), TcpLoadTopology::Outbound);
        let outbound_started = Instant::now();
        assert!(
            bench
                .send(
                    engine,
                    &probe.addr,
                    &StartTcpConnectLoad {
                        addr: outbound_addr.to_string(),
                        connection_count: u32::try_from(profile.connections).expect("connection count fits u32"),
                        session_name_prefix: "throughput-outbound".to_owned(),
                    },
                )
                .is_empty(),
            "outbound load trigger is fire-and-settle",
        );
        let outbound_workers = start_outbound_workers(
            &outbound_listener,
            profile.connections,
            profile.frames_per_connection,
            profile.frame_bytes,
            profile.timeout(),
        );
        let outbound_traffic = outbound_workers.await_traffic(profile.connections, profile.timeout());
        let outbound_elapsed = outbound_started.elapsed();
        let outbound_sessions = wait_for_sessions(
            &mut bench,
            engine,
            &probe.addr,
            TcpLoadTopology::Outbound,
            &outbound_before,
            profile.connections,
            profile.frames_per_connection,
            profile.frame_bytes,
        );
        let outbound_costs = outbound_sessions
            .iter()
            .flat_map(|session| sample_live_session(&mut bench, engine, &outbound_path(&session.session_name)))
            .collect();
        let outbound_results = outbound_workers.close_and_join();
        assert_worker_close(
            &outbound_results,
            profile.frames_per_connection,
            profile.frames_per_connection * profile.frame_bytes,
        );
        assert_sessions_closed(&mut bench, engine, &probe.addr, &outbound_sessions);
        for session in &outbound_sessions {
            assert_tombstoned(&mut bench, engine, &outbound_path(&session.session_name));
        }

        let churn_started = Instant::now();
        let mut churn_traffic = Vec::new();
        let mut churn_costs = Vec::new();
        for round in 0..profile.churn_rounds {
            let baseline = snapshot(&mut bench, engine, &probe.addr);
            let before = session_names(&baseline, TcpLoadTopology::Accepted);
            let first_index = next_accepted_index(&baseline);
            let streams = connect_accepted_streams(local_port, profile.connections);
            wait_for_live_accepted_paths(&mut bench, engine, first_index, profile.connections);
            let workers = start_workers(
                streams.into_iter(),
                SocketWorkerTopology::Accepted,
                1,
                profile.frame_bytes,
                profile.timeout(),
            );
            churn_traffic.extend(workers.await_traffic(profile.connections, profile.timeout()));
            let sessions = wait_for_sessions(
                &mut bench,
                engine,
                &probe.addr,
                TcpLoadTopology::Accepted,
                &before,
                profile.connections,
                1,
                profile.frame_bytes,
            );
            for session in &sessions {
                churn_costs.extend(sample_live_session(&mut bench, engine, &accepted_path(&session.session_name)));
            }
            let results = workers.close_and_join();
            assert_worker_close(&results, 1, profile.frame_bytes);
            assert_sessions_closed(&mut bench, engine, &probe.addr, &sessions);
            for session in &sessions {
                assert_tombstoned(&mut bench, engine, &accepted_path(&session.session_name));
            }

            let listener = TcpListener::bind("127.0.0.1:0").expect("bind outbound churn listener");
            let addr = listener.local_addr().expect("read outbound churn addr");
            let before = session_names(&snapshot(&mut bench, engine, &probe.addr), TcpLoadTopology::Outbound);
            let prefix = format!("churn-outbound-{round}");
            assert!(
                bench
                    .send(
                        engine,
                        &probe.addr,
                        &StartTcpConnectLoad {
                            addr: addr.to_string(),
                            connection_count: u32::try_from(profile.connections).expect("connection count fits u32"),
                            session_name_prefix: prefix,
                        },
                    )
                    .is_empty(),
                "outbound churn trigger is fire-and-settle",
            );
            let workers =
                start_outbound_workers(&listener, profile.connections, 1, profile.frame_bytes, profile.timeout());
            churn_traffic.extend(workers.await_traffic(profile.connections, profile.timeout()));
            let sessions = wait_for_sessions(
                &mut bench,
                engine,
                &probe.addr,
                TcpLoadTopology::Outbound,
                &before,
                profile.connections,
                1,
                profile.frame_bytes,
            );
            for session in &sessions {
                churn_costs.extend(sample_live_session(&mut bench, engine, &outbound_path(&session.session_name)));
            }
            let results = workers.close_and_join();
            assert_worker_close(&results, 1, profile.frame_bytes);
            assert_sessions_closed(&mut bench, engine, &probe.addr, &sessions);
            for session in &sessions {
                assert_tombstoned(&mut bench, engine, &outbound_path(&session.session_name));
            }
        }
        let churn_elapsed = churn_started.elapsed();

        unbind_listener(&mut bench, engine);
        assert!(list_listeners(&mut bench, engine).listeners.is_empty(), "listener map returns to its baseline");

        write_report(&TcpLoadReport {
            build_revision: build_revision(),
            profile,
            phases: vec![
                TcpLoadPhaseMetrics::from_traffic(
                    TcpLoadMetricPhase::AcceptedThroughput,
                    accepted_elapsed,
                    &accepted_traffic,
                    accepted_costs,
                ),
                TcpLoadPhaseMetrics::from_traffic(
                    TcpLoadMetricPhase::OutboundThroughput,
                    outbound_elapsed,
                    &outbound_traffic,
                    outbound_costs,
                ),
                TcpLoadPhaseMetrics::from_traffic(
                    TcpLoadMetricPhase::Churn,
                    churn_elapsed,
                    &churn_traffic,
                    churn_costs,
                ),
            ],
        });
    }
}
