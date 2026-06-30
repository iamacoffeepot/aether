//! The cpal output pipeline (ADR-0039). Builds the device stream, hands its
//! callback a [`Synth`], and surfaces the build failures the cap relays.

use core::fmt;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use super::event::{AudioEventSender, new_event_channel};
use super::instrument::{builtin_count, builtin_names};
use super::synth::Synth;

/// Handle to a running cpal pipeline. Lives on the audio worker
/// thread for the entire run — `cpal::Stream` is `!Send` on macOS,
/// so the stream is constructed on, owned by, and dropped from the
/// same thread. Dropping the pipeline silences every voice and tears
/// down the cpal stream.
pub struct AudioPipeline {
    pub sender: AudioEventSender,
    /// The device output rate the synth runs at. The cap reads it back
    /// (via the init channel) as the resample target for track decode
    /// (ADR-0103 §1) — decode happens on the dispatcher, not here.
    pub sample_rate: u32,
    pub _stream: cpal::Stream,
}

#[derive(Debug)]
pub enum AudioBuildError {
    NoDevice,
    RateUnsupported(u32),
    ConfigQuery(String),
    StreamBuild(String),
    StreamPlay(String),
}

impl fmt::Display for AudioBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDevice => write!(f, "no default audio output device"),
            Self::RateUnsupported(r) => write!(f, "requested sample rate {r} Hz unsupported"),
            Self::ConfigQuery(e) => write!(f, "config query failed: {e}"),
            Self::StreamBuild(e) => write!(f, "stream build failed: {e}"),
            Self::StreamPlay(e) => write!(f, "stream play failed: {e}"),
        }
    }
}

pub fn try_build_pipeline(
    requested_sample_rate: Option<u32>,
) -> Result<AudioPipeline, AudioBuildError> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or(AudioBuildError::NoDevice)?;

    let config = match requested_sample_rate {
        Some(rate) => {
            find_config_for_rate(&device, rate).ok_or(AudioBuildError::RateUnsupported(rate))?
        }
        None => device
            .default_output_config()
            .map_err(|e| AudioBuildError::ConfigQuery(e.to_string()))?
            .config(),
    };

    let sample_rate = config.sample_rate;
    let channels = config.channels;

    let (sender, queue) = new_event_channel();
    // Audio sample rates are bounded well below 2^24 — exact in f32.
    #[allow(clippy::cast_precision_loss)]
    let mut synth = Synth::new(queue, sample_rate as f32);

    let stream = device
        .build_output_stream(
            &config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                synth.fill(data, channels as usize);
            },
            |err| {
                tracing::warn!(
                    target: "aether_substrate::audio",
                    error = %err,
                    "cpal stream error",
                );
            },
            None,
        )
        .map_err(|e| AudioBuildError::StreamBuild(e.to_string()))?;

    stream
        .play()
        .map_err(|e| AudioBuildError::StreamPlay(e.to_string()))?;

    tracing::info!(
        target: "aether_substrate::audio",
        sample_rate,
        channels,
        instruments = builtin_count(),
        builtin_names = ?builtin_names(),
        "audio pipeline started",
    );

    Ok(AudioPipeline {
        sender,
        sample_rate,
        _stream: stream,
    })
}

pub fn find_config_for_rate(device: &cpal::Device, rate: u32) -> Option<cpal::StreamConfig> {
    device
        .supported_output_configs()
        .ok()?
        .find(|cfg| rate >= cfg.min_sample_rate() && rate <= cfg.max_sample_rate())
        .map(|cfg| cfg.with_sample_rate(rate).config())
}
