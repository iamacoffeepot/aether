use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::event::{AudioEventSender, new_event_channel};
use super::pipeline::{AudioBuildError, try_build_pipeline};
use super::synth::{Synth, synth_rate};

/// The rate a `null` output runs at when the config requests none.
pub const NULL_SAMPLE_RATE: u32 = 48_000;

/// How much audio a `null` output renders per block, and how long it waits
/// between blocks.
const NULL_BLOCK: Duration = Duration::from_millis(10);

/// Blocks per second at [`NULL_BLOCK`].
const NULL_BLOCKS_PER_SECOND: u32 = 100;

/// A `null` output renders stereo, the layout a desktop device opens.
const NULL_CHANNELS: usize = 2;

/// A running output thread: the producer side of its synth's event queue,
/// the rate the synth runs at, and the thread plus the shutdown sender whose
/// drop stops it.
pub struct AudioWorker {
    pub sender: AudioEventSender,
    pub sample_rate: u32,
    pub thread: JoinHandle<()>,
    pub shutdown: mpsc::Sender<()>,
}

/// Spawn the audio worker thread that owns `cpal::Stream` for the
/// cap's lifetime. The worker:
///   1. Builds the cpal pipeline on its own thread (`!Send`
///      constraint).
///   2. Sends the [`AudioEventSender`] back over the init channel.
///   3. Parks on the shutdown channel, holding the stream alive.
///   4. On shutdown sender drop, `recv()` returns and the stream
///      drops on this thread.
///
/// On pipeline build failure, the worker thread exits cleanly and the
/// caller sees the error.
pub fn spawn_audio_worker(requested_sample_rate: Option<u32>) -> Result<AudioWorker, AudioBuildError> {
    spawn_output_thread("aether-audio-cpal", move || {
        let pipeline = try_build_pipeline(requested_sample_rate)?;
        let sender = pipeline.sender.clone();
        let sample_rate = pipeline.sample_rate;
        let run = move |shutdown: mpsc::Receiver<()>| {
            let _ = shutdown.recv();
            drop(pipeline); // cpal::Stream tears down here
        };
        Ok((sender, sample_rate, run))
    })
}

/// Spawn a worker that runs the synth with no device: it renders one
/// [`NULL_BLOCK`] of stereo samples, discards them, and waits one block
/// before the next, so queued events drain on the same schedule a device
/// callback drains them. It stops when the shutdown sender drops.
pub fn spawn_null_worker(sample_rate: u32) -> Result<AudioWorker, AudioBuildError> {
    spawn_output_thread("aether-audio-null", move || {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, synth_rate(sample_rate));
        let frames = usize::try_from(sample_rate / NULL_BLOCKS_PER_SECOND)
            .map_err(|e| AudioBuildError::StreamBuild(format!("null block size: {e}")))?;
        let run = move |shutdown: mpsc::Receiver<()>| {
            let mut block = vec![0.0f32; frames * NULL_CHANNELS];
            synth.fill(&mut block, NULL_CHANNELS);
            while shutdown.recv_timeout(NULL_BLOCK) == Err(RecvTimeoutError::Timeout) {
                synth.fill(&mut block, NULL_CHANNELS);
            }
        };
        Ok((sender, sample_rate, run))
    })
}

/// Spawn the thread that owns one output for the cap's lifetime. `build`
/// runs on that thread, so an output that must stay on the thread that
/// built it (cpal's stream) never crosses; it returns the event sender, the
/// rate, and the body the thread then runs until its shutdown sender drops.
fn spawn_output_thread<B, R>(name: &str, build: B) -> Result<AudioWorker, AudioBuildError>
where
    B: FnOnce() -> Result<(AudioEventSender, u32, R), AudioBuildError> + Send + 'static,
    R: FnOnce(mpsc::Receiver<()>),
{
    let (init_tx, init_rx) = mpsc::channel::<Result<(AudioEventSender, u32), AudioBuildError>>();
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();

    // The audio output thread (a cpal device callback, or the null output's
    // render loop) is owned by the audio backend — not actor work, no ctx,
    // no inbound chain; the audio peripheral runs outside the mail layer.
    #[allow(clippy::disallowed_methods)]
    let thread = thread::Builder::new()
        .name(name.into())
        .spawn(move || match build() {
            Ok((sender, sample_rate, run)) => {
                let _ = init_tx.send(Ok((sender, sample_rate)));
                drop(init_tx);
                run(shutdown_rx);
            }
            Err(e) => {
                let _ = init_tx.send(Err(e));
            }
        })
        .map_err(|e| AudioBuildError::StreamBuild(format!("worker thread spawn failed: {e}")))?;

    match init_rx.recv() {
        Ok(Ok((sender, sample_rate))) => Ok(AudioWorker { sender, sample_rate, thread, shutdown: shutdown_tx }),
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
