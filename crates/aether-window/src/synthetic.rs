//! Deterministic test-only `aether.window` identity and injection vocabulary.

use aether_actor::actor;
use aether_data::KindId;
use aether_kinds::MonitorNotice;
use serde::{Deserialize, Serialize};

use crate::{
    CloseWindow, CloseWindowResult, CreateWindow, CreateWindowResult, FocusWindow, FocusWindowResult, ListWindows,
    ListWindowsResult, RequestWindowRedraw, RequestWindowRedrawResult, SetWindowMode, SetWindowModeResult,
    SetWindowTitle, SetWindowTitleResult, SubscribeWindow, SubscribeWindowResult, SubscribeWindowSelf,
    UnsubscribeAllWindows, UnsubscribeWindow, UnsubscribeWindowSelf, WindowId,
};

/// Raw, already-encoded window event injected through the synthetic runtime.
///
/// The runtime deliberately has one handler for this envelope rather than a
/// handler or cached id for every public window event kind.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.inject_event")]
pub struct InjectWindowEvent {
    pub window: WindowId,
    pub kind: KindId,
    pub payload: Vec<u8>,
}

/// Deterministic in-memory implementation of the neutral window mailbox.
#[actor(singleton, runtime::synthetic)]
pub struct SyntheticWindowCapability;
