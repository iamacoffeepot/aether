//! Key focus: which key subscribers of one window are sent its key and text
//! events (ADR-0248 §9).
//!
//! Each window has one key focus slot, empty or held by one actor with a
//! [`KeyFocusScope`]. An actor takes a window's slot by mailing
//! [`TakeKeyFocus`] to `aether.window`, and the sender is the holder. While a
//! slot is held, that window's `Key`, `KeyRelease`, `TextInput` and
//! `ImePreedit` events are sent only to the key subscribers inside the
//! holder's scope. Key focus is separate from operating-system window focus,
//! which `aether.window.focus` asks for and `aether.window.focus_changed`
//! reports.

use aether_actor::ActorPath;
use serde::{Deserialize, Serialize};

use crate::WindowInstance;

/// How far a key focus holder's hold reaches among a window's key
/// subscribers.
#[derive(aether_data::Schema, Serialize, Deserialize, Copy, Clone, Debug, PartialEq, Eq)]
pub enum KeyFocusScope {
    /// The holder alone.
    Actor,
    /// The holder and every actor beneath it in lineage.
    Subtree,
}

/// Take key focus in `window` for the sending actor, with `scope`.
///
/// `window` is the window's canonical path, the typed path every window
/// event, `aether.window.list` and `aether.window.opened` carry: an event's
/// `window` is the path to pass, and [`WindowInstance::path`] writes the same
/// path from a window's name. A path whose leaf is not
/// `aether.window.instance` does not decode, so a take for something that
/// cannot be a window never reaches the manager. The type proves what the
/// path names and nothing about liveness, so any window path is accepted and
/// an actor may take for a window that has not opened yet. The latest take in
/// a window wins: the actor it replaces is sent [`KeyFocusLost`] and the
/// sender [`KeyFocusGained`], both naming the window. A take by the window's
/// holder changes its scope and sends nothing. The take cannot fail and has
/// no reply.
///
/// The manager's handler requires [`KeyFocusHolder`] of its sender (ADR-0231
/// §11): `ctx.send::<WindowCapability>(&TakeKeyFocus { .. })` builds only for
/// an actor that handles both notices, and mail whose sender does not, or that
/// has no actor sender, is refused before the handler runs.
#[aether_data::kind(name = "aether.window.take_key_focus", eq)]
pub struct TakeKeyFocus {
    pub window: ActorPath<WindowInstance>,
    pub scope: KeyFocusScope,
}

/// Release the sending actor's key focus in `window`.
///
/// The slot empties, nothing is handed back, and the sender is sent
/// [`KeyFocusLost`] naming the window. A release from an actor that is not
/// that window's holder changes nothing and sends nothing.
#[aether_data::kind(name = "aether.window.release_key_focus", eq)]
pub struct ReleaseKeyFocus {
    pub window: ActorPath<WindowInstance>,
}

/// Sent to an actor when its take made it the key focus holder of `window`.
#[aether_data::kind(name = "aether.window.key_focus_gained", eq)]
pub struct KeyFocusGained {
    pub window: ActorPath<WindowInstance>,
}

/// Sent to an actor when it stops being the key focus holder of `window`:
/// another actor took the slot, it released the slot, or the window closed.
#[aether_data::kind(name = "aether.window.key_focus_lost", eq)]
pub struct KeyFocusLost {
    pub window: ActorPath<WindowInstance>,
}

/// What the window sends a key focus holder: the two notices, both silent.
///
/// The take and release handlers require it of their sender, so the window
/// holds each slot's holder as a `ProtocolRef<KeyFocusHolder>` and its notices
/// always reach a handler.
#[aether_actor::protocol]
pub trait KeyFocusHolder {
    /// The actor became a window's key focus holder.
    fn gained(mail: KeyFocusGained);
    /// The actor stopped being a window's key focus holder.
    fn lost(mail: KeyFocusLost);
}
