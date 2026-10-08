//! How a window presents its frames: [`WindowPresentation`], and the
//! validated [`FrameRate`] its capped value carries.

use std::error::Error as StdError;
use std::fmt;
use std::time::Duration;

use aether_data::wire::{Error as WireError, WireDecode, WireEncode};
use aether_data::{LabelNode, Schema, SchemaType};
use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// How a window presents the frames drawn for it.
///
/// Over MCP the unit values are their names (`"Display"`, `"Uncapped"`) and
/// the capped one carries its rate: `{"Capped": {"frames_per_second": 120}}`.
#[derive(aether_data::Schema, Serialize, Deserialize, Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum WindowPresentation {
    /// In step with the display: each frame waits for the display's refresh.
    #[default]
    Display,
    /// As fast as frames are produced, without waiting for the display.
    Uncapped,
    /// At most `frames_per_second` frames a second, paced by the window
    /// event loop and presented without waiting for the display.
    Capped { frames_per_second: FrameRate },
}

/// A capped window's frames a second, from [`Self::MIN`] to [`Self::MAX`]
/// inclusive.
///
/// The ceiling keeps the frame period at or above the millisecond the window
/// event loop's timer resolves. Fallible on construction, wire decode, and
/// `Deserialize`, so every rate in hand has a period the loop can pace.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct FrameRate(u32);

impl FrameRate {
    /// The slowest rate: one frame a second.
    pub const MIN: u32 = 1;
    /// The fastest rate: a one-millisecond frame period.
    pub const MAX: u32 = 1000;

    /// Validate `frames_per_second` against [`Self::MIN`] and [`Self::MAX`].
    ///
    /// # Errors
    ///
    /// [`FrameRateError`] naming the refused rate when it is outside the
    /// range.
    pub const fn new(frames_per_second: u32) -> Result<Self, FrameRateError> {
        if frames_per_second < Self::MIN || frames_per_second > Self::MAX {
            return Err(FrameRateError { frames_per_second });
        }
        Ok(Self(frames_per_second))
    }

    /// The validated rate.
    #[must_use]
    pub const fn frames_per_second(self) -> u32 {
        self.0
    }

    /// The time between two frames at this rate.
    #[must_use]
    pub fn period(self) -> Duration {
        Duration::from_secs(1) / self.0
    }
}

/// [`FrameRate::new`] rejection: the rate that was outside the range.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FrameRateError {
    frames_per_second: u32,
}

impl fmt::Display for FrameRateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid frame rate: {} frames a second is outside {} to {}",
            self.frames_per_second,
            FrameRate::MIN,
            FrameRate::MAX
        )
    }
}

impl StdError for FrameRateError {}

impl Schema for FrameRate {
    const SCHEMA: SchemaType = <u32 as Schema>::SCHEMA;
    const LABEL: Option<&'static str> = None;
    const LABEL_NODE: LabelNode = LabelNode::Anonymous;
}

impl aether_data::CrossesActors for FrameRate {}
impl aether_data::CrossesWire for FrameRate {}

impl WireEncode for FrameRate {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        self.0.encode(out)
    }
}

impl<'de> WireDecode<'de> for FrameRate {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        Self::new(u32::decode(cursor)?).map_err(|error| WireError::Message(format!("aether wire: {error}")))
    }
}

impl Serialize for FrameRate {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(self.0)
    }
}

impl<'de> Deserialize<'de> for FrameRate {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(u32::deserialize(deserializer)?).map_err(DeError::custom)
    }
}

#[cfg(test)]
mod tests {
    use aether_data::wire::encode_to_vec;

    use super::*;

    /// A rate outside the range is refused by every way in. Fails if the
    /// wire decode reads the number without running the constructor's check,
    /// which would hand the event loop a zero rate to divide a second by.
    #[test]
    fn a_rate_outside_the_range_is_refused_by_the_constructor_and_the_decode() {
        for refused in [0, FrameRate::MAX + 1] {
            assert!(FrameRate::new(refused).is_err(), "the constructor refuses {refused}");

            let bytes = encode_to_vec(&refused).expect("a u32 encodes");
            let mut cursor: &[u8] = &bytes;
            assert!(FrameRate::decode(&mut cursor).is_err(), "the decode refuses {refused}");
        }

        for accepted in [FrameRate::MIN, FrameRate::MAX] {
            let bytes = encode_to_vec(&accepted).expect("a u32 encodes");
            let mut cursor: &[u8] = &bytes;
            assert_eq!(FrameRate::decode(&mut cursor).map(FrameRate::frames_per_second), Ok(accepted));
        }
    }
}
