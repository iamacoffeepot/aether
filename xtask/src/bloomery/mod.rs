//! A client for one Bloomery engine's unit journal and bundle driver, over the
//! engine's own RPC port (#7252).
//!
//! The lanes that drive an engine from outside it (`import-commit` and the
//! `muse` verbs) share it. The two traits are the seam their tests use: the
//! [`Engine`] connection implements them by sending the journal owner the
//! matching mail, and a test implements them over a scratch journal, so the
//! lane's own logic runs against real citation checks and real entries.
//!
//! - [`client`] dials the engine and addresses the journal owner, the driver, and
//!   the workspace actor (`import`).
//! - [`reads`] pages events, waits on the head, reads artifacts in batches, and
//!   finds a head's latest move.

mod client;
mod reads;

use aether_bloomery_kinds::{
    Publish, PublishResult, ReadArtifacts, ReadArtifactsResult, ReadEvents, ReadEventsResult, WatchHead,
    WatchHeadResult,
};
use aether_codec::frame::install_max_frame_size;
use aether_rpc::FrameSizeConfig;
use anyhow::Result;

pub use client::Engine;
pub use reads::{decode, decode_entry, latest_moves, load, page, parse_digest, read_each, read_value, wait_past};

/// Send one fenced publish and return the journal's answer.
///
/// The engine connection implements it; the tests implement it over a
/// scratch journal so a lane runs against real citation checks.
pub trait Stage {
    /// Deliver `publish` and wait for its [`PublishResult`].
    ///
    /// # Errors
    /// The transport or the call failed, or the call settled with no result.
    fn stage(&mut self, publish: &Publish) -> Result<PublishResult>;
}

/// Send one of the journal's reads and return its answer.
///
/// The engine connection implements it; the tests implement it over a
/// scratch journal holding the entries a lane follows.
pub trait Reads {
    /// Deliver `request` and wait for its [`ReadEventsResult`].
    ///
    /// # Errors
    /// The transport or the call failed, or the call settled with no result.
    fn read_events(&mut self, request: ReadEvents) -> Result<ReadEventsResult>;

    /// Deliver `request` and wait for its [`WatchHeadResult`], which the
    /// journal sends once its head passes `request.after`.
    ///
    /// # Errors
    /// The transport or the call failed, or the call settled with no result.
    fn watch_head(&mut self, request: WatchHead) -> Result<WatchHeadResult>;

    /// Deliver `request` and wait for its [`ReadArtifactsResult`].
    ///
    /// # Errors
    /// The transport or the call failed, or the call settled with no result.
    fn read_artifacts(&mut self, request: &ReadArtifacts) -> Result<ReadArtifactsResult>;
}

/// Send `request`, resending it at the journal's actual sequence on each
/// stale-fence conflict, and return the first answer that is not one.
///
/// A stale fence writes nothing, and a resend carries the same artifacts and
/// moves, so it writes exactly what the first attempt would have. A conflict
/// reported at the very fence the request was sent at cannot be cleared by a
/// resend and is returned as is.
///
/// # Errors
/// The transport failed.
pub fn publish_at_fence(stage: &mut impl Stage, mut request: Publish) -> Result<PublishResult> {
    loop {
        match stage.stage(&request)? {
            PublishResult::Conflict { actual } if actual != request.expected_seq() => {
                let (artifacts, moves, _) = request.into_parts();
                request = Publish::new(artifacts, moves, actual);
            }
            answer => return Ok(answer),
        }
    }
}

/// Install the frame cap the engine and `aether-mcp` share, so a lane's
/// batch budget matches what the engine will read.
///
/// # Errors
/// The frame-size knob did not resolve.
pub fn install_frame_cap() -> Result<()> {
    install_max_frame_size(FrameSizeConfig::try_from_env()?.to_max_frame_size());
    Ok(())
}
