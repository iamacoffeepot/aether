//! The reclaim thread: frees large buffers off the thread that drops them
//! (ADR-0238 decisions 7 and 8).

use std::io;
use std::sync::mpsc::{self, Sender};

use super::RECLAIM_THRESHOLD_BYTES;
use crate::runtime::infra_thread;

/// Spawn the `aether-blob-reclaim` thread and return its sender. The thread
/// frees each buffer it receives and exits once every sender has dropped,
/// which is when the last store and the last entry are gone. The channel is
/// unbounded: the store never blocks a dropping thread and never leaks a
/// free, so under pressure the queue grows.
pub(super) fn spawn() -> io::Result<Sender<Box<[u8]>>> {
    let (sender, receiver) = mpsc::channel::<Box<[u8]>>();

    infra_thread::spawn("aether-blob-reclaim", move || receiver.into_iter().for_each(drop))?;

    Ok(sender)
}

/// Free `bytes`: on the reclaim thread when they are at least
/// [`RECLAIM_THRESHOLD_BYTES`] long, otherwise here. A send fails only when
/// the thread is gone, and the refused buffer then frees here.
pub(super) fn route(reclaim: &Sender<Box<[u8]>>, bytes: Box<[u8]>) {
    if bytes.len() >= RECLAIM_THRESHOLD_BYTES {
        drop(reclaim.send(bytes));
    }
}
