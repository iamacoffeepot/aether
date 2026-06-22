//! Sampled instrument banks (ADR-0103 §4). The resident PCM bank, the
//! repitched sample voice, and the off-realtime SFZ + WAV bank assembly the
//! `load_instrument` loader runs.

use std::io::Cursor;
use std::sync::Arc;

use aether_data::Source;

use super::decode::decode_wav_to_mono;
use super::sfz::{SfzLoop, SfzRegion};
use super::voice::BankStage;

/// Attack ramp (seconds) wrapping a sample voice — a short swell so a
/// re-pitched recording doesn't click on at full level (ADR-0103 §6,
/// the partial bank's ramp shape).
pub const SAMPLE_ATTACK_SECS: f32 = 0.003;

/// Release ramp (seconds) on `note_off` for a sample voice — the
/// damper that ends a held note faster than the sample's natural decay.
pub const SAMPLE_RELEASE_SECS: f32 = 0.08;

/// Base amplitude of a sample voice before velocity scaling. Sampled
/// recordings already carry their own level; this trims headroom so a
/// dense chord doesn't clip past the soft-clip.
pub const SAMPLE_BASE_AMP: f32 = 0.6;

/// A region's sustain loop in device-rate coordinates (ADR-0103 §6).
/// The SFZ frame offsets are scaled at bank assembly by the load-time
/// resample ratio into these fractional positions, so the kernel wrap
/// interpolates sub-sample and rounding never lands a click. `start`
/// and `end` index the region's device-rate PCM; the voice cycles the
/// half-open `[start, end)` interval while it sounds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SampleLoop {
    pub start: f32,
    pub end: f32,
}

/// One region of a sampled instrument bank (ADR-0103 §5/§6): a
/// device-rate mono recording plus the inclusive MIDI key range it
/// covers, the inclusive velocity range it answers to, the root pitch
/// it was recorded at (so a voice repitches by
/// `2^((pitch − pitch_keycenter) / 12)`), and an optional sustain loop.
/// The PCM is `Arc`'d so every region naming the same sample shares one
/// buffer and a spawned voice holds a cheap reference, not a copy.
#[derive(Clone, Debug)]
pub struct SampleRegion {
    pub lokey: u8,
    pub hikey: u8,
    pub lovel: u8,
    pub hivel: u8,
    pub pitch_keycenter: u8,
    pub pcm: Arc<[f32]>,
    /// The sustain loop, or `None` for a full-decay region that plays
    /// once and ends when its sample runs out.
    pub loop_region: Option<SampleLoop>,
}

/// A loaded sampled-instrument bank (ADR-0103 §4/§5): the regions to
/// select between by `(pitch, velocity)`, the name derived from the
/// `.sfz` filename, and the total decoded PCM the bank holds resident
/// (reported in the load reply — there is no unload in v1).
#[derive(Debug)]
pub struct SampleBank {
    pub name: String,
    pub regions: Vec<SampleRegion>,
    pub resident_bytes: usize,
}

impl SampleBank {
    /// The first region whose key and velocity ranges both contain
    /// `(pitch, velocity)`, or `None` when the note falls in a gap the
    /// bank doesn't cover (the `note_on` then drops).
    pub fn select(&self, pitch: u8, velocity: u8) -> Option<&SampleRegion> {
        self.regions.iter().find(|r| {
            (r.lokey..=r.hikey).contains(&pitch) && (r.lovel..=r.hivel).contains(&velocity)
        })
    }
}

/// The sample voice kernel (ADR-0103 §6): walk the region's device-rate
/// PCM at a repitched rate with linear interpolation, wrapped in the
/// same short attack / `note_off`-release ramp the partial bank uses.
/// An unlooped region ends when its sample runs out (full-decay,
/// piano-class sets). A looped region cycles `[loop_start, loop_end)`
/// while it sounds — interpolating across the seam back to
/// `loop_start` — and holds the note indefinitely, ending only once the
/// `note_off` release ramp completes (the loop keeps cycling beneath
/// the fade).
#[derive(Clone, Debug)]
pub struct SampleVoice {
    /// Device-rate mono PCM of the selected region, shared with the
    /// bank.
    pub pcm: Arc<[f32]>,
    /// Fractional read position into `pcm`, advanced by `rate` each
    /// output sample.
    pub pos: f32,
    /// Playback rate ratio `2^((pitch − pitch_keycenter) / 12)` — the
    /// repitch from the region's root note. The PCM is already at the
    /// device rate, so this is the only resampling the hot loop does.
    pub rate: f32,
    /// Velocity-scaled amplitude the interpolated sample is multiplied
    /// by.
    pub amplitude: f32,
    /// The sustain loop bounds (device-rate fractional positions), or
    /// `None` for an unlooped region.
    pub loop_region: Option<SampleLoop>,
    /// Attack / release ramp, the partial bank's shape.
    pub stage: BankStage,
    pub attack_s: f32,
    pub release_s: f32,
    /// Set once an unlooped region's read position walks off the end of
    /// the PCM, or the release ramp completes. A looped voice never sets
    /// it from exhaustion — it ends through the release ramp.
    pub finished: bool,
}

impl SampleVoice {
    pub fn new(pitch: u8, velocity: u8, region: &SampleRegion) -> Self {
        let semitones = f32::from(pitch) - f32::from(region.pitch_keycenter);
        let rate = (semitones / 12.0).exp2();
        let v = f32::from(velocity) / 127.0;
        // Drop a loop whose bounds collapsed (defensive — assembly only
        // emits `start < end`): a non-positive span has no cycle.
        let loop_region = region
            .loop_region
            .filter(|lp| lp.end > lp.start && lp.start >= 0.0);
        Self {
            pcm: Arc::clone(&region.pcm),
            pos: 0.0,
            rate,
            amplitude: SAMPLE_BASE_AMP * v,
            loop_region,
            stage: BankStage::Attack { t: 0.0 },
            attack_s: SAMPLE_ATTACK_SECS,
            release_s: SAMPLE_RELEASE_SECS,
            finished: false,
        }
    }

    pub fn note_off(&mut self) {
        let from_level = match self.stage {
            BankStage::Attack { t } => {
                if self.attack_s > 0.0 {
                    (t / self.attack_s).clamp(0.0, 1.0)
                } else {
                    1.0
                }
            }
            BankStage::Sustain => 1.0,
            BankStage::Release { .. } | BankStage::Done => return,
        };
        self.stage = BankStage::Release { t: 0.0, from_level };
    }

    pub fn done(&self) -> bool {
        self.finished || matches!(self.stage, BankStage::Done)
    }

    /// Advance the attack/release ramp one sample, returning its current
    /// level — the partial bank's ramp logic over the sample voice's
    /// own attack/release times.
    pub fn advance_ramp(&mut self, dt: f32) -> f32 {
        match &mut self.stage {
            BankStage::Attack { t } => {
                *t += dt;
                if self.attack_s <= 0.0 || *t >= self.attack_s {
                    self.stage = BankStage::Sustain;
                    1.0
                } else {
                    *t / self.attack_s
                }
            }
            BankStage::Sustain => 1.0,
            BankStage::Release { t, from_level } => {
                *t += dt;
                if self.release_s <= 0.0 || *t >= self.release_s {
                    self.stage = BankStage::Done;
                    0.0
                } else {
                    *from_level * (1.0 - (*t / self.release_s))
                }
            }
            BankStage::Done => 0.0,
        }
    }

    // Read position and PCM lengths are bounded well below 2^24 for any
    // sane sample, so the index-to-float and float-to-index casts in the
    // looped / unlooped readers are exact and non-negative on the hot
    // path.
    pub fn next_sample(&mut self, dt: f32) -> f32 {
        let ramp = self.advance_ramp(dt);
        if self.finished {
            return 0.0;
        }
        let len = self.pcm.len();
        if len == 0 {
            self.finished = true;
            return 0.0;
        }
        match self.loop_region {
            Some(lp) => self.next_looped(lp, len, ramp),
            None => self.next_unlooped(len, ramp),
        }
    }

    /// The unlooped read: linear interpolation over the PCM, ending the
    /// voice once the read position walks off the end (ADR-0103 §6).
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn next_unlooped(&mut self, len: usize, ramp: f32) -> f32 {
        let i = self.pos.floor() as usize;
        if i >= len {
            self.finished = true;
            return 0.0;
        }
        let a = self.pcm[i];
        let b = self.pcm[(i + 1).min(len - 1)];
        let frac = self.pos - i as f32;
        let s = (b - a).mul_add(frac, a) * self.amplitude * ramp;
        self.pos += self.rate;
        if self.pos >= len as f32 {
            // The held note's sample ran out — an unlooped voice ends.
            self.finished = true;
        }
        s
    }

    /// The looped read (ADR-0103 §6): interpolate within `[loop_start,
    /// loop_end)`, wrapping back to `loop_start` at the seam so the
    /// interpolation reads `loop_start` as the post-seam neighbour and
    /// produces no discontinuity beyond interpolation error. The voice
    /// never ends from exhaustion here — only the release ramp retires
    /// it (the loop keeps cycling beneath the fade).
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn next_looped(&mut self, lp: SampleLoop, len: usize, ramp: f32) -> f32 {
        // `pos < loop_end <= len` holds going in, so `i` is in range.
        let i = (self.pos.floor() as usize).min(len - 1);
        let a = self.pcm[i];
        // The interpolation neighbour is the next frame — but if that
        // frame reaches or crosses `loop_end`, the loop wraps, so read
        // `loop_start` instead (the seam neighbour).
        let next_index = if (i + 1) as f32 >= lp.end {
            (lp.start.floor() as usize).min(len - 1)
        } else {
            i + 1
        };
        let b = self.pcm[next_index];
        let frac = self.pos - i as f32;
        let s = (b - a).mul_add(frac, a) * self.amplitude * ramp;

        self.pos += self.rate;
        if self.pos >= lp.end {
            // Wrap the overshoot back into the loop region. Modulo the
            // loop length so a rate larger than the span still lands in
            // `[loop_start, loop_end)` in O(1).
            let loop_len = lp.end - lp.start;
            let over = self.pos - lp.end;
            let wrapped = (over / loop_len).floor().mul_add(-loop_len, over);
            self.pos = lp.start + wrapped;
        }
        s
    }
}

/// A `load_instrument` request parked while its `.sfz` `aether.fs.read`
/// is in flight (ADR-0103 §2/§5). Keyed in
/// [`AudioCapability::pending_instruments`] by the echoed
/// `(namespace, path)` of the `.sfz`. Only the original requester's
/// reply route lives here — the namespace / path come back on the
/// `ReadResult`, and the bank's name is derived from the `.sfz` path.
pub struct PendingInstrument {
    pub source: Source,
}

/// One unique sample a bank assembly is fetching: the path as written
/// in the `.sfz` (resolved against `default_path`), the fs path it is
/// read from (joined with the `.sfz`'s own directory), and its bytes
/// once the `aether.fs.read` lands.
pub struct SampleSlot {
    pub sample_rel: String,
    pub fs_path: String,
    pub bytes: Option<Vec<u8>>,
}

/// A bank load in progress: the `.sfz` parsed into regions, fanning out
/// one `aether.fs.read` per unique referenced sample, assembling when
/// the last reply lands (ADR-0103 §2). Keyed in
/// [`AudioCapability::assemblies`] by a minted id; the per-sample reads
/// correlate back to it through [`AudioCapability::pending_samples`].
pub struct BankAssembly {
    /// The original `load_instrument` requester — the
    /// `LoadInstrumentResult` reply routes here.
    pub source: Source,
    /// The fs namespace the `.sfz` and its samples live in (shared).
    pub namespace: String,
    /// The `.sfz` path — echoed on an `Err` reply for correlation.
    pub sfz_path: String,
    /// Bank name, derived from the `.sfz` filename stem.
    pub name: String,
    /// The parsed regions; each names a `sample_rel` resolved at
    /// assembly time to its decoded PCM.
    pub regions: Vec<SfzRegion>,
    /// The unique samples, fetched in parallel.
    pub samples: Vec<SampleSlot>,
    /// How many samples are still missing their bytes; the bank
    /// assembles when this reaches zero.
    pub remaining: usize,
}

/// Completion context the bank-assembly dispatch carries so the
/// `#[handler(task)]` arm can build the `Err` reply (`Ok` carries the
/// assembled bank's own name / id / bytes). Mirrors
/// [`TrackDecodeContext`] for the load path.
pub struct BankAssemblyContext {
    pub namespace: String,
    pub path: String,
}

/// Output of the bank-assembly dispatch worker — the assembled,
/// device-rate bank behind an `Arc`, or a human-readable decode failure
/// to relay as `LoadInstrumentResult::Err`.
pub type BankAssemblyOutput = Result<Arc<SampleBank>, String>;

/// Decode every unique sample to device-rate mono PCM and assemble the
/// bank (ADR-0103 §6). Pure + `Send` so it runs on the blocking-dispatch
/// worker, off the realtime path. A failed decode aborts with a
/// human-readable reason the cap relays as `LoadInstrumentResult::Err`.
pub fn assemble_bank(
    name: String,
    regions: &[SfzRegion],
    sample_bytes: &[(String, Vec<u8>)],
    target_rate: u32,
) -> BankAssemblyOutput {
    // Decode each unique sample, carrying its source rate so loop frame
    // offsets can be scaled by the same resample ratio applied to the
    // PCM (ADR-0103 §6).
    let mut decoded: Vec<(String, Arc<[f32]>, u32)> = Vec::with_capacity(sample_bytes.len());
    let mut resident_bytes = 0usize;
    for (rel, bytes) in sample_bytes {
        let pcm =
            decode_wav_to_mono(bytes, target_rate).map_err(|e| format!("sample {rel}: {e}"))?;
        let source_rate = wav_source_rate(bytes).map_err(|e| format!("sample {rel}: {e}"))?;
        resident_bytes += pcm.len() * size_of::<f32>();
        decoded.push((rel.clone(), Arc::from(pcm.as_slice()), source_rate));
    }

    let mut bank_regions = Vec::with_capacity(regions.len());
    for region in regions {
        let (pcm, source_rate) = decoded
            .iter()
            .find(|(rel, _, _)| rel == &region.sample)
            .map(|(_, pcm, source_rate)| (Arc::clone(pcm), *source_rate))
            .ok_or_else(|| format!("region references unfetched sample {}", region.sample))?;
        let loop_region = region
            .loop_spec
            .and_then(|lp| scale_loop(lp, source_rate, target_rate, pcm.len()));
        bank_regions.push(SampleRegion {
            lokey: region.lokey,
            hikey: region.hikey,
            lovel: region.lovel,
            hivel: region.hivel,
            pitch_keycenter: region.pitch_keycenter,
            pcm,
            loop_region,
        });
    }

    Ok(Arc::new(SampleBank {
        name,
        regions: bank_regions,
        resident_bytes,
    }))
}

/// Read a WAV asset's source sample rate from its header (ADR-0103 §6).
/// Bank assembly needs it to scale a region's loop frame offsets by the
/// load-time resample ratio; `decode_wav_to_mono` consumes the same
/// header but only returns the resampled PCM. Parses the header chunk
/// only — the sample data is not read.
pub fn wav_source_rate(bytes: &[u8]) -> Result<u32, String> {
    let reader = hound::WavReader::new(Cursor::new(bytes)).map_err(|e| e.to_string())?;
    let rate = reader.spec().sample_rate;
    if rate == 0 {
        return Err("zero sample rate".to_owned());
    }
    Ok(rate)
}

/// Scale a region's source-frame loop bounds into device-rate fractional
/// positions (ADR-0103 §6): multiply by the resample ratio
/// `target_rate / source_rate` — the same ratio the PCM was resampled
/// by at load — and clamp `loop_end` to the resampled length. Returns
/// `None` when the resampled region is too short to loop or the bounds
/// collapse after clamping, degrading the region to unlooped.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
pub fn scale_loop(
    lp: SfzLoop,
    source_rate: u32,
    target_rate: u32,
    resampled_len: usize,
) -> Option<SampleLoop> {
    if resampled_len < 2 || source_rate == 0 {
        return None;
    }
    let ratio = f64::from(target_rate) / f64::from(source_rate);
    let start = f64::from(lp.start) * ratio;
    let end = (f64::from(lp.end) * ratio).min(resampled_len as f64);
    if start + 1.0 >= end {
        return None;
    }
    Some(SampleLoop {
        start: start as f32,
        end: end as f32,
    })
}

/// The directory portion of an fs path (everything before the last
/// `/`), or `""` when the path has no directory. A bank's samples are
/// addressed relative to the `.sfz`'s own directory (ADR-0103 §5).
pub fn sfz_dir(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "",
    }
}

/// Join a sample path onto the `.sfz`'s directory. An empty directory
/// leaves the sample as-is.
pub fn join_fs(dir: &str, rel: &str) -> String {
    if dir.is_empty() {
        rel.to_owned()
    } else {
        format!("{dir}/{rel}")
    }
}

/// Derive a bank name from the `.sfz` filename stem (the last path
/// segment without its extension). Falls back to `"instrument"` for a
/// pathological empty stem.
pub fn bank_name_from_path(path: &str) -> String {
    let file = path.rsplit('/').next().unwrap_or(path);
    let stem = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
    if stem.is_empty() {
        "instrument".to_owned()
    } else {
        stem.to_owned()
    }
}
