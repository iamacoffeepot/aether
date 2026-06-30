//! ADR-0103 §1 decode/resample core for the audio capability.
//!
//! A pure function of bytes: take a WAV file's bytes plus the device
//! sample rate, and return a flat mono `f32` PCM buffer already at the
//! device rate — the same index-walk shape the synth voices have, so the
//! callback-side hot path is a position walk with no per-sample decode or
//! resample. WAV is the v1 container (decoded with `hound`); the two
//! sample-format arms that cover real-world assets are 16-bit integer PCM
//! and 32-bit float PCM. Multi-channel files are downmixed to mono by
//! averaging the channels (ADR-0103 parks stereo persistence), then the
//! mono stream is linearly resampled from the file's rate to the target
//! rate.
//!
//! Decode + resample run off the realtime path (the ADR-0093
//! blocking-dispatch worker, never the cpal callback), so a whole-file
//! pass is fine — a three-minute mono track at 48 kilohertz is roughly 35
//! megabytes. Track playback (#1678) and the future sampled-instrument
//! banks (#1679) share this core.

use std::error::Error;
use std::fmt;
use std::io::Cursor;

/// Why decoding an audio asset failed (ADR-0103 §2). Surfaced to the
/// requester as the `Err` arm of `PlayTrackResult` — a typo'd path is the
/// common agent failure, so a bad file comes back loud rather than
/// logged-and-dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
// pub(crate) is its true minimal reach (re-exported / used across the crate's modules); redundant_pub_crate sees only the private-module ancestor.
#[allow(clippy::redundant_pub_crate)]
pub(crate) enum DecodeError {
    /// `hound` could not parse the container or header — a truncated
    /// file, a non-WAV byte stream, or a corrupt chunk table. Carries
    /// `hound`'s own message.
    Malformed(String),
    /// The WAV parsed but its sample format is outside the v1 subset
    /// (16-bit integer PCM or 32-bit float PCM). Carries the offending
    /// `format + bit depth` so the agent can re-encode.
    UnsupportedFormat(String),
    /// The asset decoded to no samples (empty file, zero channels, or a
    /// zero sample rate) — nothing to play.
    Empty,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(detail) => write!(f, "malformed WAV: {detail}"),
            Self::UnsupportedFormat(detail) => {
                write!(f, "unsupported WAV sample format: {detail}")
            }
            Self::Empty => write!(f, "asset decoded to no samples"),
        }
    }
}

impl Error for DecodeError {}

/// Decode a WAV asset to a flat mono `f32` PCM buffer at `target_rate`.
///
/// Dispatches on the container's sample format (16-bit integer PCM
/// normalised by `i16::MIN`'s magnitude, or 32-bit float PCM read as-is),
/// averages the channels down to mono, then linearly resamples from the
/// file's rate to `target_rate`. Pure: no I/O, no allocation beyond the
/// returned buffer's working set. Errors are loud — an unsupported format
/// or a malformed header surfaces as a [`DecodeError`] the cap relays.
pub(super) fn decode_wav_to_mono(bytes: &[u8], target_rate: u32) -> Result<Vec<f32>, DecodeError> {
    let cursor = Cursor::new(bytes);
    let mut reader =
        hound::WavReader::new(cursor).map_err(|e| DecodeError::Malformed(e.to_string()))?;
    let spec = reader.spec();
    let channels = usize::from(spec.channels.max(1));
    let source_rate = spec.sample_rate;
    if source_rate == 0 {
        return Err(DecodeError::Empty);
    }

    // Read the interleaved samples as `f32` in `[-1.0, 1.0]`, dispatching
    // on `(format, bit depth)`. 16-bit ints scale by 32768 (the magnitude
    // of `i16::MIN`), 32-bit floats are already in range.
    let interleaved: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Int, 16) => reader
            .samples::<i16>()
            .map(|s| s.map(|v| f32::from(v) / 32_768.0))
            .collect::<Result<_, _>>()
            .map_err(|e| DecodeError::Malformed(e.to_string()))?,
        (hound::SampleFormat::Float, 32) => reader
            .samples::<f32>()
            .collect::<Result<_, _>>()
            .map_err(|e| DecodeError::Malformed(e.to_string()))?,
        (format, bits) => {
            return Err(DecodeError::UnsupportedFormat(format!(
                "{format:?} {bits}-bit"
            )));
        }
    };

    if interleaved.is_empty() {
        return Err(DecodeError::Empty);
    }

    let mono = downmix_to_mono(&interleaved, channels);
    let resampled = resample_linear(&mono, source_rate, target_rate);
    if resampled.is_empty() {
        return Err(DecodeError::Empty);
    }
    Ok(resampled)
}

/// Average `channels` interleaved samples per frame down to one mono
/// stream. A single-channel input is already mono and copies through.
fn downmix_to_mono(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    let frame_count = interleaved.len() / channels;
    // Channel count is a small positive integer — the reciprocal is exact
    // enough for an average; the cast can't lose meaningful precision.
    #[allow(clippy::cast_precision_loss)]
    let inv = 1.0 / channels as f32;
    let mut mono = Vec::with_capacity(frame_count);
    for frame in 0..frame_count {
        let start = frame * channels;
        let sum: f32 = interleaved[start..start + channels].iter().sum();
        mono.push(sum * inv);
    }
    mono
}

/// Linearly resample a mono stream from `source_rate` to `target_rate`.
/// Equal rates (or a stream too short to interpolate) copy through. The
/// output length scales by `target_rate / source_rate`; each output
/// sample reads a fractional source position and lerps its two
/// neighbours.
fn resample_linear(mono: &[f32], source_rate: u32, target_rate: u32) -> Vec<f32> {
    if source_rate == target_rate || mono.len() < 2 {
        return mono.to_vec();
    }
    let ratio = f64::from(target_rate) / f64::from(source_rate);
    // Sample counts and rates are bounded well below 2^53 — the f64 math
    // is exact, and the rounded length is non-negative.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let out_len = (mono.len() as f64 * ratio).round() as usize;
    let last = mono.len() - 1;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        // Map output index back to a fractional source position.
        #[allow(clippy::cast_precision_loss)]
        let src_pos = i as f64 / ratio;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let idx = src_pos.floor() as usize;
        #[allow(clippy::cast_possible_truncation)]
        let frac = (src_pos - src_pos.floor()) as f32;
        let a = mono[idx.min(last)];
        let b = mono[(idx + 1).min(last)];
        out.push((b - a).mul_add(frac, a));
    }
    out
}

/// Encode a mono `f32` stream into in-memory 16-bit-int WAV bytes at
/// `rate`. Shared test fixture: the decode unit tests and the audio cap's
/// handler tests both synthesize WAV assets this way rather than shipping
/// a binary fixture.
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwrap: in-memory WAV fixtures never fail to encode"
)]
pub(super) fn wav_int16_mono(samples: &[f32], rate: u32) -> Vec<u8> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut buf = Vec::new();
    {
        let cursor = Cursor::new(&mut buf);
        let mut writer = hound::WavWriter::new(cursor, spec).unwrap();
        for &s in samples {
            #[allow(clippy::cast_possible_truncation)]
            let v = (s * 32_767.0) as i16;
            writer.write_sample(v).unwrap();
        }
        writer.finalize().unwrap();
    }
    buf
}

// Index-to-float casts in the fixture builders are exact over the small
// sample counts the tests use; the unwraps are test-setup (in-memory WAV
// fixtures never fail to encode).
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::cast_precision_loss)]
mod tests {
    use super::*;

    /// Encode interleaved stereo `f32` into in-memory 32-bit-float WAV
    /// bytes — exercises both the float arm and the downmix.
    fn wav_float32_stereo(left: &[f32], right: &[f32], rate: u32) -> Vec<u8> {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut buf = Vec::new();
        {
            let cursor = Cursor::new(&mut buf);
            let mut writer = hound::WavWriter::new(cursor, spec).unwrap();
            for (&l, &r) in left.iter().zip(right.iter()) {
                writer.write_sample(l).unwrap();
                writer.write_sample(r).unwrap();
            }
            writer.finalize().unwrap();
        }
        buf
    }

    #[test]
    fn int16_mono_decodes_at_matching_rate_passes_through() {
        let samples: Vec<f32> = (0..100).map(|i| (i as f32 / 100.0) - 0.5).collect();
        let bytes = wav_int16_mono(&samples, 48_000);
        let pcm = decode_wav_to_mono(&bytes, 48_000).expect("decodes");
        assert_eq!(pcm.len(), samples.len(), "equal rate keeps the length");
        // 16-bit quantisation: within one LSB of the original.
        for (got, want) in pcm.iter().zip(samples.iter()) {
            assert!((got - want).abs() < 1.0e-3, "{got} vs {want}");
        }
    }

    #[test]
    fn upsample_doubles_the_sample_count() {
        let samples: Vec<f32> = (0..50).map(|i| (i as f32 * 0.1).sin()).collect();
        let bytes = wav_int16_mono(&samples, 24_000);
        let pcm = decode_wav_to_mono(&bytes, 48_000).expect("decodes");
        assert_eq!(pcm.len(), samples.len() * 2, "2x rate doubles the count");
    }

    #[test]
    fn downsample_halves_the_sample_count() {
        let samples: Vec<f32> = (0..80).map(|i| (i as f32 * 0.1).sin()).collect();
        let bytes = wav_int16_mono(&samples, 48_000);
        let pcm = decode_wav_to_mono(&bytes, 24_000).expect("decodes");
        assert_eq!(pcm.len(), samples.len() / 2, "half rate halves the count");
    }

    #[test]
    fn float32_stereo_downmixes_to_the_channel_average() {
        // Left ramps up, right ramps down — the mono average is the
        // midpoint of the two at each frame.
        let left: Vec<f32> = (0..20).map(|i| i as f32 / 20.0).collect();
        let right: Vec<f32> = (0..20).map(|i| -(i as f32) / 20.0).collect();
        let bytes = wav_float32_stereo(&left, &right, 48_000);
        let pcm = decode_wav_to_mono(&bytes, 48_000).expect("decodes");
        assert_eq!(pcm.len(), left.len());
        for (i, got) in pcm.iter().enumerate() {
            let want = f32::midpoint(left[i], right[i]);
            assert!((got - want).abs() < 1.0e-6, "frame {i}: {got} vs {want}");
        }
    }

    #[test]
    fn malformed_header_is_an_error_not_a_panic() {
        let garbage = vec![0u8, 1, 2, 3, 4, 5, 6, 7];
        assert!(matches!(
            decode_wav_to_mono(&garbage, 48_000),
            Err(DecodeError::Malformed(_))
        ));
    }

    #[test]
    fn empty_byte_stream_is_malformed() {
        assert!(matches!(
            decode_wav_to_mono(&[], 48_000),
            Err(DecodeError::Malformed(_))
        ));
    }
}
