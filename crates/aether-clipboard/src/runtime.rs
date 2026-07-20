//! Native runtime backends and handlers for `aether.clipboard`.

use super::{
    ClipboardCapability, ClipboardConfig, GetClipboardText, GetClipboardTextResult, SetClipboardText,
    SetClipboardTextResult,
};
use aether_actor::runtime;

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;

/// Mutable text clipboard backend owned by the capability dispatcher thread.
pub trait ClipboardBackend: Send {
    fn get_text(&mut self) -> Result<String, String>;
    fn set_text(&mut self, text: String) -> Result<(), String>;
}

/// Operating-system clipboard backend.
pub struct SystemClipboard {
    clipboard: arboard::Clipboard,
}

impl SystemClipboard {
    fn new() -> Result<Self, arboard::Error> {
        arboard::Clipboard::new().map(|clipboard| Self { clipboard })
    }
}

impl ClipboardBackend for SystemClipboard {
    fn get_text(&mut self) -> Result<String, String> {
        self.clipboard.get_text().map_err(|error| error.to_string())
    }

    fn set_text(&mut self, text: String) -> Result<(), String> {
        self.clipboard.set_text(text).map_err(|error| error.to_string())
    }
}

/// Deterministic process-local clipboard backend used by `SubstrateBench`.
#[derive(Default)]
pub struct InMemoryClipboard {
    text: String,
}

impl ClipboardBackend for InMemoryClipboard {
    fn get_text(&mut self) -> Result<String, String> {
        Ok(self.text.clone())
    }

    fn set_text(&mut self, text: String) -> Result<(), String> {
        self.text = text;
        Ok(())
    }
}

/// Runtime state for [`ClipboardCapability`].
pub struct ClipboardCapabilityState {
    backend: Box<dyn ClipboardBackend>,
}

#[runtime]
impl NativeActor for ClipboardCapability {
    type State = ClipboardCapabilityState;
    type Config = ClipboardConfig;

    const NAMESPACE: &'static str = "aether.clipboard";

    fn init(config: ClipboardConfig, _ctx: &mut NativeInitCtx<'_>) -> Result<ClipboardCapabilityState, BootError> {
        let backend: Box<dyn ClipboardBackend> = match config {
            ClipboardConfig::System => {
                Box::new(SystemClipboard::new().map_err(|error| BootError::Other(Box::new(error)))?)
            }
            ClipboardConfig::InMemory => Box::new(InMemoryClipboard::default()),
        };
        Ok(ClipboardCapabilityState { backend })
    }

    #[handler::single]
    fn on_get_text(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: GetClipboardText,
    ) -> GetClipboardTextResult {
        match state.backend.get_text() {
            Ok(text) => GetClipboardTextResult::Ok { text },
            Err(error) => GetClipboardTextResult::Err { error },
        }
    }

    #[handler::single]
    fn on_set_text(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: SetClipboardText,
    ) -> SetClipboardTextResult {
        match state.backend.set_text(mail.text) {
            Ok(()) => SetClipboardTextResult::Ok,
            Err(error) => SetClipboardTextResult::Err { error },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_clipboard_starts_empty_then_round_trips_text() {
        let mut backend = InMemoryClipboard::default();
        assert_eq!(backend.get_text().expect("empty clipboard is readable"), "");

        backend.set_text("copied text".to_owned()).expect("in-memory set succeeds");
        assert_eq!(backend.get_text().expect("set clipboard is readable"), "copied text",);
    }
}
