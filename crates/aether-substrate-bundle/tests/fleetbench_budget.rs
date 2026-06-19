//! `FleetBench` latency-budget unit checks (issue 2064): the two pure
//! decisions the re-arm read loop owns — lowering a seconds knob to a
//! `Duration` (with the `0 → wait forever` sentinel) and classifying a
//! `read_frame` error as a re-armable socket timeout vs a real failure.
//!
//! These are not heavy: they boot nothing and touch no socket, so they
//! stay out of `mod tests::heavy` and run concurrently. The wire round-trip
//! and the cold-start/reply backstops themselves are exercised by the
//! booted `FleetBench` scenarios (`fleetbench_spawn`, `fleetbench_mail`, …).

mod fleetbench;

mod tests {
    use crate::fleetbench::{cap_from_secs, is_rearm_timeout};
    use aether_codec::frame::FrameError;
    use std::io::{Error as IoError, ErrorKind};
    use std::time::Duration;

    #[test]
    fn cap_from_secs_maps_seconds_and_zero_sentinel() {
        // The only logic here: seconds → `Duration`, with `0` as the
        // "wait forever" sentinel (the gate's `elapsed >= cap` never trips).
        assert_eq!(cap_from_secs(0), Duration::MAX, "0 → wait forever");
        assert_eq!(cap_from_secs(60), Duration::from_mins(1));
        assert_eq!(cap_from_secs(300), Duration::from_mins(5));
    }

    #[test]
    fn rearm_timeout_classification() {
        // A timed-out blocking read re-arms; both platform spellings count.
        assert!(is_rearm_timeout(&FrameError::Io(IoError::from(
            ErrorKind::WouldBlock
        ))));
        assert!(is_rearm_timeout(&FrameError::Io(IoError::from(
            ErrorKind::TimedOut
        ))));
        // A genuine failure does not re-arm — it propagates as an error.
        assert!(!is_rearm_timeout(&FrameError::Io(IoError::from(
            ErrorKind::UnexpectedEof
        ))));
        assert!(!is_rearm_timeout(&FrameError::Io(IoError::from(
            ErrorKind::ConnectionReset
        ))));
    }
}
