//! The compiler cache every lane build runs through, when the host has one
//! (#4894).
//!
//! No lane shares a target directory with another, on purpose: reusing cargo's
//! incremental state across divergent source is unsound, and the shared
//! directory that avoids the cold cost instead grows without bound. The verify
//! lane builds in its dispatch worktree's own tree and a model lane's child
//! builds in a throwaway under the run's scratch, and both pay the same price
//! for it — every lap recompiles the whole dependency tree, and a sweep re-pays
//! it on the next lane.
//!
//! sccache is sound exactly where a shared target directory is not. It keys each
//! compiler invocation by content — the source bytes, the flags, the hashes of
//! what it links against — and returns a cached artifact only on an exact match,
//! so two lanes on divergent source cannot collide by construction: the failure
//! mode is a miss, never a wrong artifact.
//!
//! What that buys depends on where the lane builds, and the boundary is sharper
//! than it looks. Cargo names its dependency search path and output directory on
//! every `rustc` invocation (`-L dependency=<target>/debug/deps`, `--out-dir`),
//! and those are part of what sccache hashes. So a lane that rebuilds at the
//! **same** path re-pays nothing after a sweep — every third-party dependency
//! included — while a lane at a **new** path misses on the dependencies too, not
//! merely on the workspace crates. Measured over `aether-data`'s tree, twelve
//! compilations: a swept rebuild at the same path hit all twelve, and the same
//! rebuild one directory over hit none. Canonical per-slot build paths are what
//! turn that into a cross-lane win; this is the half that has to be wired first,
//! and the counters below are what make the follow-on measurable rather than
//! anecdotal.
//!
//! It is host tooling, not a dependency of this workspace. A host without
//! sccache builds exactly as it did before rather than failing a lane over a
//! binary nobody promised it — and where the cache lives and how large it may
//! grow stay the host's own `SCCACHE_DIR` / `SCCACHE_CACHE_SIZE`, which this
//! deliberately does not overwrite.

use std::process::{Command, Stdio};

use serde::Serialize;

/// sccache's program name — the value `RUSTC_WRAPPER` points at, and the program
/// the counters are read back from.
const SCCACHE: &str = "sccache";

/// The lane build environment sccache needs, mirroring what `ci.yml` exports for
/// the same reason.
///
/// Incremental compilation is off because sccache cannot cache an incremental
/// artifact — with `-C incremental` in the invocation every compile is a miss,
/// which would leave the wrapper as pure overhead. The lane gives up nothing by
/// it: a target directory created for one lap has no prior incremental state to
/// reuse anyway.
const LANE_BUILD_ENV: [(&str, &str); 2] = [("RUSTC_WRAPPER", SCCACHE), ("CARGO_INCREMENTAL", "0")];

/// The key lane evidence carries the counters under.
const EVIDENCE_KEY: &str = "sccache";

/// The host's sccache, held across one lane run.
pub(super) struct CompilerCache {
    /// Where the counters stood when the lane started. sccache's server reports
    /// its lifetime totals, and a daemon that has been up all day says nothing
    /// about this lap — so the run reports what it moved them by.
    opening: Counters,
}

/// What sccache served a lane run: the receipts the calibration ledger counts
/// reclaimed seconds from, rather than inferring them from wall clock.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
pub(super) struct Counters {
    hits: u64,
    misses: u64,
}

impl Counters {
    /// This reading less the `opening` one — the lane's own share of a server's
    /// lifetime totals.
    ///
    /// Saturating, because a server restarted mid-lane resets its counters and a
    /// negative count is not something lane evidence can carry; the honest
    /// reading of a reset is that this lane can prove nothing about what it drew.
    fn since(self, opening: Self) -> Self {
        Self { hits: self.hits.saturating_sub(opening.hits), misses: self.misses.saturating_sub(opening.misses) }
    }
}

impl CompilerCache {
    /// What this lane's own compilations drew from the cache, or `None` when the
    /// closing counters could not be read.
    pub(super) fn served(&self) -> Option<Counters> {
        read_counters().map(|closing| closing.since(self.opening))
    }
}

/// The host's sccache, or `None` when it has none.
///
/// The probe is the counter read itself: a host that answers it has a server
/// that runs, and the answer is the opening reading every later delta is taken
/// against. Everything else — no binary on `PATH`, a server that will not start,
/// output this cannot parse — is a host with no usable cache, and leaves the
/// lane building exactly as it did before rather than failing it.
pub(super) fn detect() -> Option<CompilerCache> {
    read_counters().map(|opening| CompilerCache { opening })
}

/// Point `command`'s build at `cache`, when the host has one. A `None` cache
/// leaves the command's environment untouched.
pub(super) fn export(cache: Option<&CompilerCache>, command: &mut Command) {
    if cache.is_some() {
        command.envs(LANE_BUILD_ENV.iter().copied());
    }
}

/// Stamp what sccache `served` onto a model lane's evidence envelope.
///
/// Presence-driven like the envelope's other optional channels: a host without
/// sccache stamps no key at all, so a reader sees "this host has no cache"
/// rather than a zero that reads as a cache which served nothing.
pub(super) fn stamp(evidence: &mut serde_json::Value, served: Option<Counters>) {
    if let Some(served) = served
        && let Some(object) = evidence.as_object_mut()
    {
        object.insert(EVIDENCE_KEY.to_owned(), serde_json::json!(served));
    }
}

/// Ask sccache for its counters.
fn read_counters() -> Option<Counters> {
    let stats = Command::new(SCCACHE)
        .args(["--show-stats", "--stats-format=json"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|stats| stats.status.success())?;
    parse_counters(&String::from_utf8_lossy(&stats.stdout))
}

/// The hit and miss totals in one `--stats-format=json` document.
///
/// Tripwire: the lane runs against whatever sccache the host installed, and a
/// document this cannot read must come back as nothing rather than as zeroes. A
/// reader that fell back to zero on an unfamiliar shape would file a version skew
/// in the ledger as a cache that served nothing, and the receipts would read as
/// evidence against the very thing they exist to measure.
fn parse_counters(stdout: &str) -> Option<Counters> {
    let document = serde_json::from_str::<serde_json::Value>(stdout).ok()?;
    let stats = document.get("stats")?;
    Some(Counters { hits: counter(stats.get("cache_hits")?)?, misses: counter(stats.get("cache_misses")?)? })
}

/// One counter's total. sccache reports each as a per-language object, so the
/// total is its `counts` map summed — a Rust-only lane populates one entry, but
/// summing costs nothing and does not pin the lane to that being true.
fn counter(value: &serde_json::Value) -> Option<u64> {
    Some(value.get("counts")?.as_object()?.values().filter_map(serde_json::Value::as_u64).sum())
}

#[cfg(test)]
mod tests {
    use super::{Counters, parse_counters, stamp};

    // The counters come out of the document sccache actually prints — the shape
    // below is `--stats-format=json` output with the fields this does not read
    // trimmed away.
    #[test]
    fn the_counters_are_the_per_language_totals_summed() {
        let counted = parse_counters(
            r#"{"stats":{"cache_hits":{"counts":{"Rust":41,"C/C++":1},"adv_counts":{}},
                        "cache_misses":{"counts":{"Rust":7},"adv_counts":{}}}}"#,
        );
        assert_eq!(counted, Some(Counters { hits: 42, misses: 7 }));

        let quiet = parse_counters(r#"{"stats":{"cache_hits":{"counts":{}},"cache_misses":{"counts":{}}}}"#);
        assert_eq!(quiet, Some(Counters { hits: 0, misses: 0 }), "a lane that compiled nothing counted nothing");
    }

    // Tripwire: a document this cannot read is not a cache that served nothing.
    // Falling back to zero would file a version skew in the ledger as evidence
    // against the cache, which is the one wrong answer these receipts can give.
    #[test]
    fn an_unreadable_document_reports_nothing_rather_than_zero() {
        assert_eq!(parse_counters("sccache: command not found"), None, "non-JSON output is not a reading");
        assert_eq!(parse_counters(r#"{"stats":{"cache_hits":{"counts":{}}}}"#), None, "a missing counter is not zero");
        assert_eq!(parse_counters("{}"), None, "a document with no stats object is not a reading");
    }

    // A server restarted mid-lane reports totals below the opening reading. The
    // delta must not wrap into an astronomical hit count that would credit the
    // lane with reclaiming time it never did.
    #[test]
    fn a_counter_reset_mid_lane_reports_no_reclaim_rather_than_wrapping() {
        let opening = Counters { hits: 900, misses: 40 };
        let after_restart = Counters { hits: 3, misses: 1 };

        assert_eq!(after_restart.since(opening), Counters { hits: 0, misses: 0 });
        assert_eq!(Counters { hits: 912, misses: 47 }.since(opening), Counters { hits: 12, misses: 7 });
    }

    // The evidence channel is presence-driven: a host with no sccache must stamp
    // no key, because a zeroed one reads as a cache that served nothing — the
    // opposite conclusion about the host from the one that is true.
    #[test]
    fn a_host_without_sccache_stamps_no_evidence_key() {
        let mut absent = serde_json::json!({ "command": "construct.implement" });
        stamp(&mut absent, None);
        assert!(absent.get("sccache").is_none(), "no cache means no key at all");

        let mut present = serde_json::json!({ "command": "construct.implement" });
        stamp(&mut present, Some(Counters { hits: 12, misses: 7 }));
        assert_eq!(present["sccache"], serde_json::json!({ "hits": 12, "misses": 7 }));
    }
}
