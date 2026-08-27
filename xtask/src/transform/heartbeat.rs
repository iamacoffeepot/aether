//! A construct lane compiling its own gates used to read as a dead child.
//!
//! The beat says the lane process is alive; the sealed wall clock is what
//! bounds a wedged one. Writes fail open: a lane that cannot stamp is the
//! pre-fix lane, never a failed attempt.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BEAT_INTERVAL: Duration = Duration::from_secs(30);
const WAKE_INTERVAL: Duration = Duration::from_millis(500);
const HEARTBEAT_FILE: &str = "heartbeat";

/// RAII guard that stamps `<out>/heartbeat` for as long as the construct
/// lane's post-model stretch is still running.
#[must_use]
pub(super) struct Beat {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Beat {
    /// Spawn the beater over `out_dir`. The first stamp lands before the
    /// worker sleeps, so a phase shorter than [`BEAT_INTERVAL`] still leaves
    /// a file.
    pub(super) fn start(out_dir: &Path) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let out_dir = out_dir.to_path_buf();
        #[allow(clippy::disallowed_methods)] // aether-suppression-request: infra heartbeat timer below the mail layer
        let worker = thread::spawn(move || beat_until_stopped(&out_dir, &worker_stop));
        Self { stop, worker: Some(worker) }
    }
}

impl Drop for Beat {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn beat_until_stopped(out_dir: &Path, stop: &AtomicBool) {
    write_beat(out_dir);
    let mut last = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        thread::sleep(WAKE_INTERVAL);
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if last.elapsed() >= BEAT_INTERVAL {
            write_beat(out_dir);
            last = Instant::now();
        }
    }
}

fn write_beat(out_dir: &Path) {
    let Some(millis) =
        SystemTime::now().duration_since(UNIX_EPOCH).ok().and_then(|since| u64::try_from(since.as_millis()).ok())
    else {
        return;
    };
    // A lane that cannot write its heartbeat is the pre-fix lane, never a failed attempt.
    let _ = fs::write(out_dir.join(HEARTBEAT_FILE), millis.to_string());
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;
    use std::{env, fs, process, thread};

    use super::{Beat, HEARTBEAT_FILE, WAKE_INTERVAL};

    fn scratch_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!("aether-heartbeat-{tag}-{}-{seq}", process::id()));
        fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    #[test]
    fn a_beat_writes_before_its_first_interval_elapses() {
        let out = scratch_dir("first");
        drop(Beat::start(&out));
        assert!(
            out.join(HEARTBEAT_FILE).is_file(),
            "a worker that slept first would leave a phase shorter than one interval with no stamp"
        );
    }

    // Tripwire: a detached worker outliving its lane would keep beating for a
    // run the executor is entitled to cancel.
    #[test]
    fn dropping_the_beat_stops_the_worker() {
        let out = scratch_dir("stop");
        drop(Beat::start(&out));
        let path = out.join(HEARTBEAT_FILE);
        let stamped = fs::metadata(&path).expect("heartbeat exists after drop").modified().expect("mtime");
        thread::sleep(WAKE_INTERVAL + Duration::from_millis(200));
        let later = fs::metadata(&path).expect("heartbeat still exists").modified().expect("mtime");
        assert_eq!(later, stamped, "a stopped worker must not keep beating");
    }

    #[test]
    fn a_beat_over_an_unwritable_directory_is_not_an_error() {
        let parent = scratch_dir("unwritable");
        let blocker = parent.join("not-a-dir");
        fs::write(&blocker, b"x").expect("a file where a directory cannot be created");
        drop(Beat::start(&blocker.join("nested")));
    }
}
