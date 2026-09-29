//! The reply detail a failed run carries. The run sequence itself is driven
//! on the shipped composition, in `aether-chassis-bloomery`'s workspace
//! scenarios.

use std::error::Error;
use std::io;

use super::RunError;
use crate::runtime::engine::EngineError;
use crate::runtime::storage::StorageError;
use crate::runtime::testing::RUN_CONTAINER;

#[test]
fn a_failure_cause_keeps_the_call_and_drops_the_endpoint_the_daemon_message_and_the_source_message()
-> Result<(), Box<dyn Error>> {
    // Catches a cause that falls back to a wrapped error's text, which the
    // driver would record: the endpoint's socket path, the daemon's own
    // message, or the source's own words (a journal root path among them)
    // would reach the journal.
    let socket = "/srv/host-only/docker.sock";
    let message = "no such image: registry.internal.example/secret";
    let root = "/srv/host-only/journal";
    let causes = [
        RunError::Engine {
            call: "reading the daemon's platform".to_owned(),
            error: EngineError::Connect {
                endpoint: format!("unix://{socket}"),
                source: io::Error::new(io::ErrorKind::NotFound, format!("{socket}: missing")),
            },
        },
        RunError::Engine {
            call: format!("starting container {RUN_CONTAINER}"),
            error: EngineError::Status { status: 404, message: message.to_owned() },
        },
        RunError::Storage {
            during: "staging the run's outputs".to_owned(),
            error: StorageError::Refused(format!("{root}/journal.sqlite: disk I/O error")),
        },
    ]
    .map(|error| (error.to_string(), error.cause()));

    for (full, cause) in &causes {
        let cause = cause.as_str();
        for host in [socket, message, root] {
            assert!(!cause.contains(host), "{cause:?} holds {host:?}");
        }
        let call = full.split(':').next().ok_or("an empty full text")?;
        assert!(cause.starts_with(&format!("{call}:")), "{cause:?} does not name the call {call:?}");
    }
    assert!(causes[0].0.contains(socket), "the log text keeps the endpoint: {:?}", causes[0].0);
    assert!(causes[2].0.contains(root), "the log text keeps the source's words: {:?}", causes[2].0);
    assert!(causes[1].1.as_str().ends_with("answered 404"), "{:?}", causes[1].1);
    Ok(())
}
