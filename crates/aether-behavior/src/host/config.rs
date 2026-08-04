//! The behavior host's boot config and control-mail vocabulary (ADR-0137,
//! issue 2687).
//!
//! [`HostConfig`] rides the by-value `WasmActor::Config` path — the host is
//! a wasm actor, so its config crosses the spawn boundary as encoded bytes
//! and is handed to `init` by value (as `PanelConfig` is to the reference
//! widget panel), never the ADR-0090 Resolver derive (unreachable without
//! `aether-substrate`, which this crate's boundary forbids). Since #2878, a
//! bare load supplies [`HostConfig::default()`]; composite reload still
//! reconstructs a typed-config inline child from its *real* retained config
//! bytes (#2694), so this default is only the initial no-config boot value.

use alloc::string::String;
use alloc::vec::Vec;

use aether_data::{Kind, KindId, Schema};
use serde::{Deserialize, Serialize};

/// The wrapped child the host interposes on: the child actor's type tag
/// (the `hash(NAMESPACE)` `u64` — `ActorTypeTag::of::<W>().0` on the kit
/// side, the SDK-sanctioned hash), its subname, and its pre-encoded config
/// bytes. Stored as a raw `u64` because [`ChildSpec`] is a `Schema`-derived
/// config that wire-encodes and persists cleanly; the host wraps it as
/// `ActorTypeTag(type_tag)` at the spawn call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Schema, Default)]
pub struct ChildSpec {
    /// `hash(NAMESPACE)` of the wrapped actor type. For a stock
    /// `aether-kit-widget` widget, `aether_kit_widget::WidgetKind::type_tag()`
    /// produces this value without linking that crate's `runtime` feature.
    pub type_tag: u64,
    /// The wrapped child's subname within the cluster.
    pub subname: String,
    /// The wrapped child's `Config` encoded to its wire shape (empty for a
    /// `Config = ()` child).
    #[serde(with = "aether_data::bytes")]
    pub config: Vec<u8>,
}

/// Where the host's script bytes come from at boot / on a swap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Schema, Default)]
pub enum ScriptSource {
    /// Boot wrapper-transparent — no script until a `load_script` /
    /// `set_script` swaps one in.
    #[default]
    None,
    /// The script bytes inline (the kit's `set_script` path, or a config
    /// that ships the wasm directly).
    Inline(Vec<u8>),
    /// Fetch the script from a substrate I/O namespace at boot
    /// (`aether.fs.read`).
    FsRef {
        /// The `aether.fs` namespace prefix (`"save"`, `"assets"`, `"config"`).
        namespace: String,
        /// The path within the namespace.
        path: String,
    },
}

/// The behavior host's boot config (ADR-0137). Handed to `init` by value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Kind, Schema)]
#[kind(name = "aether.behavior.host_config")]
pub struct HostConfig {
    /// The wrapped child the host spawns and interposes on.
    pub child: ChildSpec,
    /// The initial script source.
    pub script: ScriptSource,
    /// Fuel budget per filter call — reset before every call, so a script
    /// that overruns it traps and fails open rather than wedging the host.
    pub fuel_per_call: u64,
    /// After this many consecutive traps the script is disabled (pure
    /// passthrough) until the next `load_script` / `set_script`.
    pub disable_after_traps: u32,
    /// The kind id whose arrival down-lane the host maps onto the reserved
    /// FRAME sentinel (the script's per-frame hook). `0` disables the frame
    /// mapping. Configurable rather than hard-wired to a widget kind so the SDK
    /// keeps no `aether-kit-widget` dependency — the widget crate's
    /// `WidgetKind::BehaviorHost` arm sets this to its own `Collect` id.
    pub frame_trigger: u64,
    /// Low-rate mirror-kind ids always offered to SDK dispatch even when the
    /// script manifest does not declare a handler for them.
    pub mirror_kinds: Vec<u64>,
}

impl HostConfig {
    /// Default fuel budget per filter call (~1M — generous for a small
    /// intercept, bounded enough that a runaway loop traps promptly).
    pub const DEFAULT_FUEL_PER_CALL: u64 = 1_000_000;

    /// Default consecutive-trap threshold before the script is disabled.
    pub const DEFAULT_DISABLE_AFTER_TRAPS: u32 = 3;

    /// The configured frame-trigger kind, or `None` when the mapping is off.
    #[must_use]
    pub fn frame_trigger_kind(&self) -> Option<KindId> {
        (self.frame_trigger != 0).then_some(KindId(self.frame_trigger))
    }

    /// Whether `kind` belongs to the configured always-offer mirror set.
    #[must_use]
    pub fn is_mirror_kind(&self, kind: KindId) -> bool {
        self.mirror_kinds.contains(&kind.0)
    }
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            child: ChildSpec::default(),
            script: ScriptSource::default(),
            fuel_per_call: Self::DEFAULT_FUEL_PER_CALL,
            disable_after_traps: Self::DEFAULT_DISABLE_AFTER_TRAPS,
            frame_trigger: 0,
            mirror_kinds: Vec::new(),
        }
    }
}

/// `aether.behavior.load_script` — swap the running script for one fetched
/// from an `aether.fs` namespace. Replies `LoadScriptResult` once the read
/// settles through the behavior host's request context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Kind, Schema)]
#[kind(name = "aether.behavior.load_script")]
pub struct LoadScript {
    /// The `aether.fs` namespace prefix.
    pub namespace: String,
    /// The path within the namespace.
    pub path: String,
}

/// `aether.behavior.set_script` — swap the running script for inline bytes.
/// The synchronous counterpart of `LoadScript`; the reply *is* the handler's
/// return value (`#[handler::single]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Kind, Schema)]
#[kind(name = "aether.behavior.set_script")]
pub struct SetScript {
    /// The replacement script's wasm bytes.
    #[serde(with = "aether_data::bytes")]
    pub bytes: Vec<u8>,
}

/// Reply to `LoadScript` / `SetScript` — mirrors `aether.fs.read_result`'s
/// Ok/Err shape. `Ok` reports the resident script's byte count; `Err`
/// carries the failure text, and the prior running script is kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Kind, Schema)]
#[kind(name = "aether.behavior.load_script_result")]
pub enum LoadScriptResult {
    /// The swap succeeded; `resident_bytes` is the new script's size.
    Ok {
        /// Byte length of the now-resident script.
        resident_bytes: u64,
    },
    /// The swap failed (bad bytes, validation error, read error); the prior
    /// script keeps running.
    Err {
        /// Human-readable failure detail.
        error: String,
    },
}
