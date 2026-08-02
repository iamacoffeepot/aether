//! Guest-side params injection (ADR-0170): the declare-needs channel a wasm
//! actor's `Params` type uses to ask the component host for load-time facts.
//!
//! A component declares what it needs as fields on a `#[derive(InjectedParams)]`
//! struct, one `#[param("<kind name>")]` per field; the derive records those
//! requests in [`InjectedParams::REQUESTS`], `#[actor]` copies them into the
//! wasm's `aether.kinds.inputs` custom section, and the host validates the whole
//! list against its provider registry before it instantiates anything. What
//! reaches the guest is a bag of kind-tagged byte entries, which the generated
//! [`InjectedParams::from_entries`] decodes field by field into the struct
//! `init` receives.
//!
//! Two properties hold by construction. Every request is required — there is no
//! inject-if-available form, because behaviour that silently differs by host is
//! a footgun — so a value that arrives is a value that is there. And `Params`
//! itself is not a wire kind: only its fields are, which is why a `Params`
//! struct can mix facts from unrelated kind families without a wrapper kind
//! existing for the combination.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use aether_data::wire;
use aether_data::{Kind, ParamEntry, ParamRequest};

/// Why a params bag could not be turned into the actor's `Params` value.
///
/// Both arms are load failures: the shim stages the message through
/// `init_failed_p32` and the load surfaces it as `LoadResult::Err`, so the
/// component never reaches `init` with a partially-injected `Params`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamsError {
    /// The host shipped no entry for a requested kind. Reaching the guest at
    /// all means the host's load-time validation and the section's request
    /// list disagreed — the host's own check should have failed the load
    /// first — or the actor was instantiated through a path that ships no
    /// params (an inline child, or an older host that only knows the
    /// config-only init export).
    Missing { field: &'static str, kind_name: &'static str },
    /// The entry was present but its bytes did not decode as the field's
    /// kind: the provider and the guest disagree on the kind's shape.
    Undecodable { field: &'static str, kind_name: &'static str },
}

impl ParamsError {
    /// A one-line message naming the field, the kind, and which of the two
    /// failures happened — the string the init shim stages for `LoadResult::Err`.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Missing { field, kind_name } => {
                format!("params injection: no host entry for field `{field}` (kind {kind_name})")
            }
            Self::Undecodable { field, kind_name } => {
                format!("params injection: field `{field}` could not decode the host entry as kind {kind_name}")
            }
        }
    }
}

/// A `Params` type that declares which host facts it needs and can be built
/// from the bag the component host ships (ADR-0170).
///
/// Implemented by `#[derive(InjectedParams)]`; `()` implements it as the
/// no-requests case, which is what the `#[actor]` macro synthesizes for the
/// overwhelming majority of components that declare no `type Params`.
pub trait InjectedParams: Sized {
    /// Every host fact this type requests, in field-declaration order.
    /// `#[actor]` writes this slice into the `aether.kinds.inputs` section at
    /// const-eval time, so it is the single source for both the guest's decode
    /// and the host's validation.
    const REQUESTS: &'static [ParamRequest];

    /// Build the value from the host-shipped entries, or explain which field
    /// could not be filled.
    ///
    /// # Errors
    ///
    /// [`ParamsError::Missing`] when no entry carries a requested field's
    /// kind, [`ParamsError::Undecodable`] when one does but its bytes are not
    /// that kind.
    fn from_entries(entries: &[ParamEntry]) -> Result<Self, ParamsError>;
}

/// The no-requests case. Also what `Params = ()` resolves through, so a
/// component that declares no `type Params` keeps loading against a host that
/// ships an empty bag — and against one that ships no params bytes at all.
impl InjectedParams for () {
    const REQUESTS: &'static [ParamRequest] = &[];

    fn from_entries(_entries: &[ParamEntry]) -> Result<Self, ParamsError> {
        Ok(())
    }
}

/// Whether a `#[param("…")]` literal spells the same kind its field's type
/// declares — the const check `#[derive(InjectedParams)]` asserts on.
///
/// The literal is a readable restatement of the field type, so it is held to
/// the type rather than trusted: a request whose name drifts fails to compile
/// at the declaration. Lives here rather than being inlined by the derive so
/// the byte walk exists once, in a crate that owns it.
#[doc(hidden)]
#[must_use]
pub const fn param_name_matches(declared: &str, actual: &str) -> bool {
    let (declared, actual) = (declared.as_bytes(), actual.as_bytes());
    if declared.len() != actual.len() {
        return false;
    }
    let mut index = 0;
    while index < declared.len() {
        if declared[index] != actual[index] {
            return false;
        }
        index += 1;
    }
    true
}

/// Decode one requested field out of the bag. The generated
/// `from_entries` calls this once per `#[param]` field; `field` and the kind's
/// `NAME` ride along only to name the failure.
///
/// # Errors
///
/// [`ParamsError::Missing`] when the bag carries no entry for `K`,
/// [`ParamsError::Undecodable`] when it does but the bytes are not a `K`.
pub fn take_param<K: Kind>(entries: &[ParamEntry], field: &'static str) -> Result<K, ParamsError> {
    let entry =
        entries.iter().find(|entry| entry.kind == K::ID).ok_or(ParamsError::Missing { field, kind_name: K::NAME })?;
    K::decode_from_bytes(&entry.bytes).ok_or(ParamsError::Undecodable { field, kind_name: K::NAME })
}

/// Decode the host's params bag — the aether-wire encoding of a
/// `Vec<ParamEntry>` — out of the bytes the FFI shim was handed.
///
/// An empty slice is the no-params case and yields an empty bag rather than a
/// decode error, mirroring how empty config bytes resolve to `Config::default()`:
/// a host with nothing to inject spends no bytes saying so.
///
/// # Errors
///
/// Returns `Err` with a describable message when non-empty bytes are not a
/// wire-encoded entry list.
pub fn decode_params_bag(bytes: &[u8]) -> Result<Vec<ParamEntry>, ParamsError> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    wire::from_bytes::<Vec<ParamEntry>>(bytes)
        .map_err(|_| ParamsError::Undecodable { field: "<bag>", kind_name: "aether.component.params_bag" })
}

/// Decode the bag and build the actor's `Params` in one step — what every
/// init shim calls, so the two-stage decode isn't repeated per `export!` arm.
///
/// # Errors
///
/// Propagates [`decode_params_bag`]'s bag-level failure and
/// [`InjectedParams::from_entries`]'s per-field one.
pub fn resolve_params<P: InjectedParams>(bytes: &[u8]) -> Result<P, ParamsError> {
    P::from_entries(&decode_params_bag(bytes)?)
}
