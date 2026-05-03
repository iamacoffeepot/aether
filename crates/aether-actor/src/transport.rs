//! ADR-0074 §Decision: the actor SDK's split point. Every byte
//! crossing the actor↔chassis boundary is funnelled through one
//! `MailTransport` impl chosen at compile time by the consumer crate
//! — `aether-component`'s `WasmTransport` (delegates to `_p32` host
//! fns) for WASM guests, `aether-substrate`'s `NativeTransport`
//! (owned by each native capability) for native actors.
//!
//! Trait methods take `&self`, not associated functions. WasmTransport
//! is a ZST, so its `&self` is unused and the dispatch lowers to a
//! direct call to the matching host fn — no overhead. NativeTransport
//! is a regular struct each capability owns, holding the per-actor
//! state (mailer + self mailbox + inbox + correlation counter +
//! overflow queue) directly as fields. No thread-locals, no
//! install/uninstall ceremony — the type system carries the actor
//! binding through the `&T` references threaded into `Sink::send`,
//! `Ctx<'a, T>`, and the `wait_reply` / handle helpers.

/// The five operations every transport must provide. Signatures
/// mirror the `_p32` FFI in `aether-component::raw` byte-for-byte —
/// pointer/length pairs become `&[u8]` / `&mut [u8]` slices but the
/// integer parameters and return codes are untouched, so a transport
/// impl can forward verbatim and the SDK can read return codes
/// uniformly. ADR-0042's `wait_reply` sentinel codes (`-1` timeout,
/// `-2` buffer too small, `-3` cancelled, `>= 0` bytes written) are
/// the trait contract; impls must preserve them.
pub trait MailTransport {
    /// Push a typed payload at `recipient`. `bytes` is the wire
    /// encoding of the payload (cast for `#[repr(C)]` kinds, postcard
    /// for schema-shaped kinds — `Kind::encode_into_bytes` already
    /// resolves which). `count` is `1` for a single send and N for a
    /// batch (cast-only, see `Sink::send_many`).
    ///
    /// Return `0` on success; non-zero is reserved for transport-
    /// specific failure surfaces. Today only the wasm transport
    /// returns non-zero (substrate-side `register` lookup miss); the
    /// native transport collapses any channel-send failure into the
    /// same scalar.
    fn send_mail(&self, recipient: u64, kind: u64, bytes: &[u8], count: u32) -> u32;

    /// Reply to the originator of the mail currently being dispatched
    /// (ADR-0013). `sender` is the per-instance handle the dispatcher
    /// threaded onto `Ctx` at receive time; the substrate routes it
    /// to the right Claude session, sibling component, or remote
    /// engine mailbox.
    fn reply_mail(&self, sender: u32, kind: u64, bytes: &[u8], count: u32) -> u32;

    /// Deposit a migration bundle for a future replacement instance
    /// (ADR-0016). Only meaningful inside `on_replace`. `bytes` are
    /// copied into a substrate-owned buffer immediately, so the
    /// caller is free to drop the slice on return. Returns `0` on
    /// success; non-zero on substrate rejection (today: 1 MiB cap
    /// exceeded, or internal OOB — both component bugs).
    ///
    /// Native actors don't have a `replace_component`-style hot
    /// reload path (only wasm components do). The native transport
    /// returns a non-zero error sentinel so a misuse is loud.
    fn save_state(&self, version: u32, bytes: &[u8]) -> u32;

    /// Block the actor's thread until a mail of `expected_kind` (and,
    /// when `expected_correlation != 0`, also that correlation id)
    /// arrives, then copy up to `out.len()` bytes of its payload into
    /// `out` (ADR-0042). `timeout_ms` is clamped substrate-side to
    /// 30s.
    ///
    /// Returns `>= 0` = bytes written, `-1` = timeout, `-2` = payload
    /// larger than `out` (mail re-parked for retry), `-3` = the host
    /// tore the actor down mid-wait. Any other negative is reserved
    /// for future sentinels and surfaces through `WaitError::decode`
    /// in the SDK wrapper so a reader sees the unknown rc by name.
    fn wait_reply(
        &self,
        expected_kind: u64,
        out: &mut [u8],
        timeout_ms: u32,
        expected_correlation: u64,
    ) -> i32;

    /// Correlation id the host minted for this actor's most recent
    /// `send_mail` call (ADR-0042). `0` before any send. Sync
    /// wrappers use it to filter `wait_reply` to "the reply for the
    /// request I just sent" rather than "any reply of this kind."
    fn prev_correlation(&self) -> u64;
}
