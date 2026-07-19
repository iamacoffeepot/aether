use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use super::event::AudioEventSender;
use super::pipeline::{AudioBuildError, try_build_pipeline};

/// Spawn the audio worker thread that owns `cpal::Stream` for the
/// cap's lifetime. The worker:
///   1. Builds the cpal pipeline on its own thread (`!Send`
///      constraint).
///   2. Sends the [`AudioEventSender`] back over the init channel.
///   3. Parks on the shutdown channel, holding the stream alive.
///   4. On shutdown sender drop, `recv()` returns and the stream
///      drops on this thread.
///
/// Returns the producer side of the synth event queue plus the
/// worker thread + shutdown sender for the cap to manage. On
/// pipeline build failure, the worker thread exits cleanly and the
/// caller sees the error.
pub fn spawn_audio_worker(
    requested_sample_rate: Option<u32>,
) -> Result<(AudioEventSender, u32, JoinHandle<()>, mpsc::Sender<()>), AudioBuildError> {
    let (init_tx, init_rx) = mpsc::channel::<Result<(AudioEventSender, u32), AudioBuildError>>();
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();

    // cpal device-callback thread, owned by the audio backend — not actor work,
    // no ctx, no inbound chain; the audio peripheral runs outside the mail layer.
    #[allow(clippy::disallowed_methods)]
    let thread = thread::Builder::new()
        .name("aether-audio-cpal".into())
        .spawn(move || {
            match try_build_pipeline(requested_sample_rate) {
                Ok(pipeline) => {
                    let _ = init_tx.send(Ok((pipeline.sender.clone(), pipeline.sample_rate)));
                    drop(init_tx);
                    let _ = shutdown_rx.recv();
                    drop(pipeline); // cpal::Stream tears down here
                }
                Err(e) => {
                    let _ = init_tx.send(Err(e));
                }
            }
        })
        .map_err(|e| AudioBuildError::StreamBuild(format!("worker thread spawn failed: {e}")))?;

    match init_rx.recv() {
        Ok(Ok((sender, sample_rate))) => Ok((sender, sample_rate, thread, shutdown_tx)),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            Err(AudioBuildError::StreamBuild("audio worker closed channel before init".to_string()))
        }
    }
}
