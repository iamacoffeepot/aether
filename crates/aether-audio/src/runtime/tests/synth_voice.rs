use super::*;

// Tripwire: a built-in's wire instrument_id is its positional index into BUILTINS, so reordering the table is a wire-breaking change. This pins the full ordered name list to catch a silent reorder.
#[test]
fn builtin_registry_lists_eleven_patches() {
    assert_eq!(builtin_count(), 11);
    assert_eq!(
        builtin_names(),
        vec![
            "sine_lead",
            "square_bass",
            "triangle",
            "saw_lead",
            "pluck",
            "piano",
            "electric_piano",
            "pad",
            "kick",
            "hat",
            "snare",
        ],
    );
}

/// Pull a `PartialBankDef` out of the registry by name for the
/// kernel tests. Panics if the named patch is not a partial bank.
fn partial_bank_def(name: &str) -> PartialBankDef {
    let def = BUILTINS.iter().find(|d| d.name == name).expect("named builtin exists");
    match def.voice {
        VoiceDef::PartialBank(bank) => bank,
        VoiceDef::Oscillator { .. } => panic!("{name} is not a partial-bank patch"),
    }
}

/// Drive a kernel until it frees itself, returning the sample
/// count. Caps iterations so a stuck voice fails the test instead
/// of hanging.
fn samples_until_done(voice: &mut PartialBankVoice, sample_rate: f32) -> usize {
    let dt = 1.0 / sample_rate;
    // 30 s cap at the test rate — exact for usize.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let cap = (sample_rate * 30.0) as usize;
    let mut n = 0;
    while !voice.done() && n < cap {
        voice.next_sample(dt);
        n += 1;
    }
    assert!(voice.done(), "voice did not free itself within the cap");
    n
}

#[test]
fn partial_bank_envelope_decreases_after_attack() {
    let def = partial_bank_def("piano");
    let mut voice = PartialBankVoice::new(60, 100, &def, 0.3, 48_000.0);
    let dt = 1.0 / 48_000.0;
    // Run past the (near-zero) attack into sustain.
    while !voice.in_sustain() {
        voice.next_sample(dt);
    }
    let mut last = voice.envelope_level();
    for _ in 0..4_000 {
        voice.next_sample(dt);
        let level = voice.envelope_level();
        assert!(level <= last + f32::EPSILON, "partial envelope must not rise in sustain: {level} > {last}");
        last = level;
    }
}

#[test]
fn higher_pitch_decays_in_fewer_samples() {
    let def = partial_bank_def("piano");
    let mut low = PartialBankVoice::new(40, 100, &def, 0.3, 48_000.0);
    let mut high = PartialBankVoice::new(84, 100, &def, 0.3, 48_000.0);
    let low_samples = samples_until_done(&mut low, 48_000.0);
    let high_samples = samples_until_done(&mut high, 48_000.0);
    assert!(high_samples < low_samples, "high pitch ({high_samples}) should ring shorter than low ({low_samples})");
}

#[test]
fn upper_partial_energy_rises_with_velocity() {
    let def = partial_bank_def("piano");
    let soft = PartialBankVoice::new(60, 20, &def, 0.3, 48_000.0);
    let hard = PartialBankVoice::new(60, 120, &def, 0.3, 48_000.0);
    let upper_share = |v: &PartialBankVoice| -> f32 {
        let amps = v.partial_amps();
        let upper: f32 = amps[PARTIAL_COUNT / 2..].iter().map(|a| a.abs()).sum();
        let total: f32 = amps.iter().map(|a| a.abs()).sum();
        upper / total
    };
    assert!(upper_share(&hard) > upper_share(&soft), "harder strike must shift energy toward upper partials");
}

#[test]
fn note_off_silences_faster_than_natural_decay() {
    let def = partial_bank_def("piano");
    let mut undamped = PartialBankVoice::new(60, 100, &def, 0.3, 48_000.0);
    let mut damped = PartialBankVoice::new(60, 100, &def, 0.3, 48_000.0);
    let dt = 1.0 / 48_000.0;
    // Let both ring briefly, then release only the damped one.
    for _ in 0..480 {
        undamped.next_sample(dt);
        damped.next_sample(dt);
    }
    damped.note_off();
    let damped_samples = 480 + samples_until_done(&mut damped, 48_000.0);
    let undamped_samples = 480 + samples_until_done(&mut undamped, 48_000.0);
    assert!(
        damped_samples < undamped_samples,
        "note_off damper ({damped_samples}) should beat natural decay ({undamped_samples})",
    );
}

#[test]
fn partial_bank_voice_frees_itself_when_silent() {
    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, 48_000.0);
    // id 5 is piano; high pitch rings out quickly.
    sender
        .push(AudioEvent::NoteOn { sender_mailbox: MailboxId(1), pitch: 96, velocity: 100, instrument_id: 5, pan: 0 })
        .unwrap();
    let mut buf = vec![0.0f32; 4_800];
    for _ in 0..200 {
        synth.fill(&mut buf, 1);
        if synth.voice_count() == 0 {
            break;
        }
    }
    assert_eq!(synth.voice_count(), 0, "held piano voice never freed");
}

#[test]
fn pad_holds_level_through_sustain() {
    let def = partial_bank_def("pad");
    let mut voice = PartialBankVoice::new(60, 100, &def, 0.18, 48_000.0);
    let dt = 1.0 / 48_000.0;
    // Drive through the long attack into sustain.
    while !voice.in_sustain() {
        voice.next_sample(dt);
    }
    let level = voice.envelope_level();
    for _ in 0..48_000 {
        voice.next_sample(dt);
    }
    let after = voice.envelope_level();
    assert!((after - level).abs() < 1.0e-3, "pad must sustain its level while held: {level} -> {after}");
}

/// A sustain-holding ADSR (instant attack, no decay, full
/// sustain) so a kernel test reads the raw waveform without the
/// envelope shaping the level.
const HOLD_ADSR: Adsr = Adsr { attack_secs: 0.0, decay_secs: 0.0, sustain: 1.0, release_secs: 0.1 };

/// Build an oscillator voice and collect `n` samples at 48 kHz.
fn collect_osc(wave: Wave, base_amp: f32, seed: u32, n: usize) -> Vec<f32> {
    let mut voice = OscVoice::new(60, 100, wave, HOLD_ADSR, base_amp, 48_000.0, seed);
    let dt = 1.0 / 48_000.0;
    (0..n).map(|_| voice.next_sample(dt)).collect()
}

/// Count sign changes across a sample window — a proxy for
/// instantaneous frequency.
fn zero_crossings(samples: &[f32]) -> usize {
    samples.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count()
}

#[test]
fn noise_is_bounded_and_nonzero() {
    let samples = collect_osc(Wave::Noise { lowpass: 1.0, tone_mix: 0.0 }, 1.0, voice_seed(MailboxId(1), 9, 60), 4_000);
    assert!(samples.iter().all(|s| s.abs() <= 1.0 + f32::EPSILON), "noise sample escaped [-1, 1]");
    assert!(samples.iter().any(|s| s.abs() > 0.0), "noise produced silence");
}

#[test]
fn noise_is_deterministic_for_a_fixed_voice_key() {
    let seed = voice_seed(MailboxId(7), 9, 64);
    let wave = Wave::Noise { lowpass: 0.8, tone_mix: 0.0 };
    let first = collect_osc(wave, 1.0, seed, 2_000);
    let second = collect_osc(wave, 1.0, seed, 2_000);
    assert_eq!(first, second, "fixed-key noise must be reproducible");
}

#[test]
fn lowpass_reduces_sample_to_sample_delta() {
    let seed = voice_seed(MailboxId(1), 9, 60);
    let unfiltered = collect_osc(Wave::Noise { lowpass: 1.0, tone_mix: 0.0 }, 1.0, seed, 8_000);
    let filtered = collect_osc(Wave::Noise { lowpass: 0.15, tone_mix: 0.0 }, 1.0, seed, 8_000);
    let mean_delta = |s: &[f32]| -> f32 {
        let sum: f32 = s.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
        // window count is bounded and small — exact in f32.
        #[allow(clippy::cast_precision_loss)]
        let count = (s.len() - 1) as f32;
        sum / count
    };
    assert!(mean_delta(&filtered) < mean_delta(&unfiltered), "lowpassed noise should be smoother sample-to-sample");
}

#[test]
fn pitch_sweep_zero_crossing_rate_falls_toward_base() {
    let mut voice = OscVoice::new(60, 100, Wave::Sine, HOLD_ADSR, 1.0, 48_000.0, 1)
        .with_pitch_sweep(PitchSweep { start_ratio: 8.0, time_constant_secs: 0.05 }, 48_000.0);
    let dt = 1.0 / 48_000.0;
    let samples: Vec<f32> = (0..19_200).map(|_| voice.next_sample(dt)).collect();
    let onset = zero_crossings(&samples[0..2_400]);
    let settled = zero_crossings(&samples[16_800..19_200]);
    assert!(settled < onset, "swept pitch should slow toward the base frequency: onset {onset}, settled {settled}");
}

#[test]
fn note_on_off_lifecycle() {
    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, 48_000.0);
    sender
        .push(AudioEvent::NoteOn { sender_mailbox: MailboxId(1), pitch: 60, velocity: 100, instrument_id: 0, pan: 0 })
        .unwrap();
    let mut buf = vec![0.0f32; 480];
    synth.fill(&mut buf, 1);
    assert_eq!(synth.voice_count(), 1);
    assert!(buf.iter().any(|s| s.abs() > 0.0));

    sender.push(AudioEvent::NoteOff { sender_mailbox: MailboxId(1), pitch: 60, instrument_id: 0 }).unwrap();
    // Compile-time constant; trivially exact for usize.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let release_samples = (0.5 * 48_000.0) as usize;
    let mut tail = vec![0.0f32; release_samples];
    synth.fill(&mut tail, 1);
    assert_eq!(synth.voice_count(), 0);
}

// ADR-0126 master reverb send. `reverb_send` defaults to fully dry
// and only a `SetReverbSend` event changes it; these tests pin the
// inert-by-default invariant and the reverb tail's existence.

// Tripwire: with `reverb_send` at its default 0.0, `fill`'s output
// for a fixed note must exactly match an independent, reverb-free
// computation of the same voice — a regression that feeds the mix
// into the reverb regardless of `reverb_send`, or defaults the send
// to nonzero, would otherwise silently color every mix.
#[test]
fn reverb_send_zero_matches_pre_reverb_dry_mix_bit_for_bit() {
    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, TEST_RATE);
    sender
        .push(AudioEvent::NoteOn { sender_mailbox: MailboxId(1), pitch: 60, velocity: 100, instrument_id: 0, pan: 0 })
        .unwrap();
    assert_eq!(synth.reverb_send(), 0.0, "reverb send must default to fully dry");

    let mut buf = vec![0.0f32; 480];
    synth.fill(&mut buf, 1);

    // Independent parallel computation of the dry mix: the exact
    // same built-in kernel `trigger_note_on` would have built,
    // stepped and mixed the same way `fill` does (master_gain at
    // its 1.0 default), never touching the reverb.
    let def = BUILTINS.iter().find(|d| d.name == "sine_lead").expect("sine_lead is builtin id 0");
    let mut kernel = build_builtin_kernel(MailboxId(1), 0, 60, 100, def, TEST_RATE);
    let dt = 1.0 / TEST_RATE;
    let expected: Vec<f32> = (0..480)
        .map(|_| {
            let dry = match &mut kernel {
                VoiceKernel::Oscillator(v) => v.next_sample(dt),
                VoiceKernel::PartialBank(v) => v.next_sample(dt),
                VoiceKernel::Sample(v) => v.next_sample(dt),
            };
            dry.tanh()
        })
        .collect();
    assert_eq!(buf, expected, "reverb_send == 0.0 must not alter the dry mix");
}

#[test]
fn reverb_tail_persists_after_the_note_ends() {
    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, TEST_RATE);
    sender.push(AudioEvent::SetReverbSend { send: 1.0 }).unwrap();
    sender
        .push(AudioEvent::NoteOn { sender_mailbox: MailboxId(1), pitch: 60, velocity: 100, instrument_id: 0, pan: 0 })
        .unwrap();
    let mut buf = vec![0.0f32; 480];
    synth.fill(&mut buf, 1);
    assert_eq!(synth.reverb_send(), 1.0);

    sender.push(AudioEvent::NoteOff { sender_mailbox: MailboxId(1), pitch: 60, instrument_id: 0 }).unwrap();
    // Drive well past the release so the voice fully frees.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let release_samples = (0.5 * TEST_RATE) as usize;
    let mut tail = vec![0.0f32; release_samples];
    synth.fill(&mut tail, 1);
    assert_eq!(synth.voice_count(), 0, "voice should have released by now");

    // Render a further block with no voices or tracks live — any
    // energy here can only be the reverb's tail from the note that
    // already ended.
    let mut after = vec![0.0f32; 4_000];
    synth.fill(&mut after, 1);
    assert!(after.iter().any(|s| s.abs() > 1.0e-6), "expected a reverb tail after the note's voice had already ended");
}

// ADR-0104 scheduled note events. These drive `fill` with known
// block sizes against the synth's frame clock, so the frame a
// scheduled event fires on is deterministic.

#[test]
fn scheduled_note_fires_at_its_exact_frame() {
    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, TEST_RATE);
    // 1 ms at 48 kHz is exactly 48 frames.
    sender
        .push(AudioEvent::Schedule {
            sender_mailbox: MailboxId(1),
            events: vec![ScheduledEvent {
                at_millis: 1,
                event: ScheduledNote::On { pitch: 60, velocity: 100, instrument_id: 0, pan: 0 },
            }],
        })
        .unwrap();
    let mut buf = vec![0.0f32; 1];
    // The first drain converts the offset to due frame 48 and parks
    // it; rendering frames 0..47 must not fire it early.
    for _ in 0..48 {
        synth.fill(&mut buf, 1);
    }
    assert_eq!(synth.voice_count(), 0, "scheduled note fired before its frame");
    assert_eq!(synth.scheduled_count(), 1, "event left the heap too early");
    // The 49th fill renders absolute frame 48 — the exact due frame.
    synth.fill(&mut buf, 1);
    assert_eq!(synth.voice_count(), 1, "scheduled note missed its frame");
    assert_eq!(synth.scheduled_count(), 0, "fired event not drained from the heap");
    assert!(synth.has_voice_with_pitch(60));
}

#[test]
fn simultaneous_scheduled_events_stay_a_chord() {
    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, TEST_RATE);
    // Two notes at the same offset share one receipt timebase, so
    // they fire on the same frame — a chord stays a chord.
    sender
        .push(AudioEvent::Schedule {
            sender_mailbox: MailboxId(1),
            events: vec![
                ScheduledEvent {
                    at_millis: 0,
                    event: ScheduledNote::On { pitch: 60, velocity: 100, instrument_id: 0, pan: 0 },
                },
                ScheduledEvent {
                    at_millis: 0,
                    event: ScheduledNote::On { pitch: 64, velocity: 100, instrument_id: 0, pan: 0 },
                },
            ],
        })
        .unwrap();
    let mut buf = vec![0.0f32; 64];
    synth.fill(&mut buf, 1);
    assert_eq!(synth.voice_count(), 2, "simultaneous notes did not both fire");
    assert!(synth.has_voice_with_pitch(60));
    assert!(synth.has_voice_with_pitch(64));
}

#[test]
fn scheduled_note_off_releases_after_its_note_on() {
    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, TEST_RATE);
    // One note held for 10 ms, then released — both events in one
    // batch. The off keys the same voice as the on (same sender +
    // instrument + pitch).
    sender
        .push(AudioEvent::Schedule {
            sender_mailbox: MailboxId(1),
            events: vec![
                ScheduledEvent {
                    at_millis: 0,
                    event: ScheduledNote::On { pitch: 60, velocity: 100, instrument_id: 0, pan: 0 },
                },
                ScheduledEvent { at_millis: 10, event: ScheduledNote::Off { pitch: 60, instrument_id: 0 } },
            ],
        })
        .unwrap();
    let mut buf = vec![0.0f32; 64];
    // The note-on fires on the first block; the off's due frame
    // (480 at 48 kHz) is still in the future, so the voice sounds.
    synth.fill(&mut buf, 1);
    assert_eq!(synth.voice_count(), 1, "scheduled note-on never sounded");
    assert!(synth.has_voice_with_pitch(60));
    // Play past the off's due frame plus the 0.5 s release: the off
    // fires after the on and the voice frees.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let tail_samples = (0.6 * TEST_RATE) as usize;
    let mut tail = vec![0.0f32; tail_samples];
    synth.fill(&mut tail, 1);
    assert_eq!(synth.voice_count(), 0, "scheduled note-off never released the voice");
}

#[test]
fn schedule_offset_spans_block_boundaries() {
    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, TEST_RATE);
    // 2 ms == 96 frames; with 64-frame blocks the note lands in the
    // second block, never the first.
    sender
        .push(AudioEvent::Schedule {
            sender_mailbox: MailboxId(1),
            events: vec![ScheduledEvent {
                at_millis: 2,
                event: ScheduledNote::On { pitch: 72, velocity: 100, instrument_id: 0, pan: 0 },
            }],
        })
        .unwrap();
    let mut buf = vec![0.0f32; 64];
    synth.fill(&mut buf, 1);
    assert_eq!(synth.voice_count(), 0, "fired in the wrong block");
    synth.fill(&mut buf, 1);
    assert_eq!(synth.voice_count(), 1, "note never fired in its block");
}

/// Tripwire: two concurrent note-ons sharing a `(sender_mailbox,
/// instrument_id, pitch)` key must each allocate their own voice —
/// the second must not steal the first's slot (issue 2524).
#[test]
fn same_key_note_ons_stack_voices() {
    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, 48_000.0);
    for _ in 0..2 {
        sender
            .push(AudioEvent::NoteOn {
                sender_mailbox: MailboxId(1),
                pitch: 60,
                velocity: 100,
                instrument_id: 0,
                pan: 0,
            })
            .unwrap();
    }
    let mut buf = vec![0.0f32; 128];
    synth.fill(&mut buf, 1);
    assert_eq!(synth.voice_count(), 2, "two same-key note-ons must both sound as independent voices");
}

/// Tripwire: with two voices stacked on one key, `NoteOff` must
/// release the oldest still-sounding voice and leave its sibling
/// alone — pairing oldest-note-on with oldest-note-off. A second
/// `NoteOff` on the same key then releases the survivor (issue 2524).
#[test]
fn note_off_releases_oldest_unreleased_voice_on_shared_key() {
    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, 48_000.0);
    for _ in 0..2 {
        sender
            .push(AudioEvent::NoteOn {
                sender_mailbox: MailboxId(1),
                pitch: 60,
                velocity: 100,
                instrument_id: 0,
                pan: 0,
            })
            .unwrap();
    }
    let mut buf = vec![0.0f32; 64];
    synth.fill(&mut buf, 1);
    assert_eq!(synth.voice_count(), 2, "setup: both note-ons must sound");

    sender.push(AudioEvent::NoteOff { sender_mailbox: MailboxId(1), pitch: 60, instrument_id: 0 }).unwrap();
    // instrument 0 (sine_lead) releases in 0.18s; run well past that
    // so the released voice finishes its release ramp and is pruned,
    // while its never-released sibling (held in Sustain) stays
    // resident regardless of how long we run.
    let mut tail = vec![0.0f32; 12_000];
    synth.fill(&mut tail, 1);
    assert_eq!(synth.voice_count(), 1, "note-off must release exactly the oldest voice, leaving its sibling sounding");
    assert!(synth.has_voice_with_pitch(60), "the un-released sibling must still be sounding");

    sender.push(AudioEvent::NoteOff { sender_mailbox: MailboxId(1), pitch: 60, instrument_id: 0 }).unwrap();
    synth.fill(&mut tail, 1);
    assert_eq!(synth.voice_count(), 0, "second note-off must release the surviving voice");
}

#[test]
fn different_senders_get_independent_voices() {
    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, 48_000.0);
    for mailbox in 1..=3 {
        sender
            .push(AudioEvent::NoteOn {
                sender_mailbox: MailboxId(mailbox),
                pitch: 60,
                velocity: 100,
                instrument_id: 0,
                pan: 0,
            })
            .unwrap();
    }
    let mut buf = vec![0.0f32; 128];
    synth.fill(&mut buf, 1);
    assert_eq!(synth.voice_count(), 3);
}

#[test]
fn set_master_gain_clamps_above_unity() {
    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, 48_000.0);
    sender.push(AudioEvent::SetMasterGain { gain: 1.5 }).unwrap();
    let mut buf = vec![0.0f32; 64];
    synth.fill(&mut buf, 1);
    assert!((synth.master_gain() - 1.0).abs() < f32::EPSILON);

    sender.push(AudioEvent::SetMasterGain { gain: -0.2 }).unwrap();
    synth.fill(&mut buf, 1);
    assert!(synth.master_gain().abs() < f32::EPSILON);
}

// ADR-0127 per-note pan + per-sender gain. These drive `Synth::fill`
// with a known stereo channel count and read the two channels' energy
// apart, exercising this crate's own mix logic (the pan law wiring,
// the sender-gain table, and its block-granular live resolution).

/// Sum the absolute energy in each channel of an interleaved buffer.
fn channel_energy(buffer: &[f32], channels: usize) -> Vec<f32> {
    let mut energy = vec![0.0f32; channels];
    for frame in buffer.chunks_exact(channels) {
        for (ch, s) in frame.iter().enumerate() {
            energy[ch] += s.abs();
        }
    }
    energy
}

// Tripwire: a hard-left note must land its energy in the left channel
// and near-none in the right — the pan law wiring from the i8 field
// through the L/R accumulator split.
#[test]
fn hard_left_note_puts_energy_in_the_left_channel() {
    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, TEST_RATE);
    sender
        .push(AudioEvent::NoteOn {
            sender_mailbox: MailboxId(1),
            pitch: 60,
            velocity: 127,
            instrument_id: 0,
            pan: -128,
        })
        .unwrap();
    let mut buf = vec![0.0f32; 480 * 2];
    synth.fill(&mut buf, 2);
    let energy = channel_energy(&buf, 2);
    assert!(energy[0] > 0.0, "hard-left note produced no left energy");
    assert!(
        energy[1] < energy[0] * 1.0e-3,
        "hard-left note leaked into the right channel: L={}, R={}",
        energy[0],
        energy[1],
    );
}

// Tripwire: a set_sender_gain of 0.5 must halve that sender's voice
// contribution versus unity — the sender-gain table applied at the mix.
#[test]
fn set_sender_gain_scales_voice_contribution() {
    let render_energy = |gain: Option<f32>| -> f32 {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, TEST_RATE);
        if let Some(g) = gain {
            sender.push(AudioEvent::SetSenderGain { sender_mailbox: MailboxId(1), gain: g }).unwrap();
        }
        sender
            .push(AudioEvent::NoteOn {
                sender_mailbox: MailboxId(1),
                pitch: 60,
                // A modest velocity keeps the summed level well inside
                // the near-linear region of the tanh soft clip, so the
                // energy ratio reflects the gain, not the clip.
                velocity: 80,
                instrument_id: 0,
                pan: 0,
            })
            .unwrap();
        let mut buf = vec![0.0f32; 480 * 2];
        synth.fill(&mut buf, 2);
        buf.iter().map(|s| s.abs()).sum()
    };
    let unity = render_energy(None);
    let halved = render_energy(Some(0.5));
    let ratio = halved / unity;
    assert!((ratio - 0.5).abs() < 0.05, "gain 0.5 should halve a sender's energy, got ratio {ratio}");
}

// Tripwire: a set_sender_gain arriving after a note is already sounding
// must duck it on the next block — the trim is resolved per block, not
// captured at note_on.
#[test]
fn set_sender_gain_ducks_a_sounding_voice() {
    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, TEST_RATE);
    sender
        .push(AudioEvent::NoteOn { sender_mailbox: MailboxId(1), pitch: 60, velocity: 100, instrument_id: 0, pan: 0 })
        .unwrap();
    let mut buf = vec![0.0f32; 256 * 2];
    // Warm past the attack, then measure a steady block at unity.
    synth.fill(&mut buf, 2);
    synth.fill(&mut buf, 2);
    let unity_energy: f32 = buf.iter().map(|s| s.abs()).sum();
    // Duck the already-sounding voice; the next block must drop.
    sender.push(AudioEvent::SetSenderGain { sender_mailbox: MailboxId(1), gain: 0.25 }).unwrap();
    synth.fill(&mut buf, 2);
    let ducked_energy: f32 = buf.iter().map(|s| s.abs()).sum();
    assert!(
        ducked_energy < unity_energy * 0.5,
        "set_sender_gain must duck a sounding voice: unity={unity_energy}, ducked={ducked_energy}",
    );
}

#[test]
fn unknown_instrument_id_drops_note() {
    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, 48_000.0);
    sender
        .push(AudioEvent::NoteOn { sender_mailbox: MailboxId(1), pitch: 60, velocity: 100, instrument_id: 99, pan: 0 })
        .unwrap();
    let mut buf = vec![0.0f32; 64];
    synth.fill(&mut buf, 1);
    assert_eq!(synth.voice_count(), 0);
}

#[test]
fn voice_steal_caps_at_max_voices() {
    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, 48_000.0);
    for i in 0..(MAX_VOICES as u64 + 10) {
        sender
            .push(AudioEvent::NoteOn {
                sender_mailbox: MailboxId(i + 1),
                pitch: 60,
                velocity: 100,
                instrument_id: 0,
                pan: 0,
            })
            .unwrap();
    }
    let mut buf = vec![0.0f32; 64];
    synth.fill(&mut buf, 1);
    assert_eq!(synth.voice_count(), MAX_VOICES);
}

/// Voice-steal must evict the *quietest* sounding voice, not the
/// oldest — an old voice under sustain treatment is frequently still
/// loud, so an age-based steal would cut an audible note.
///
/// Setup: saturate the pool with loud (velocity 127) voices except
/// one deliberately quiet voice (velocity 1) that is *newer* than the
/// oldest, and make the oldest voice (pitch 0, allocated first) loud.
/// Trigger one more note: an oldest-seq steal would evict the loud
/// pitch 0; the quietest steal must evict the quiet voice and leave
/// pitch 0 sounding.
#[test]
fn voice_steal_evicts_quietest_note() {
    // Pitch 0 is the oldest voice and loud; `QUIET_PITCH` is newer but
    // very quiet. All share instrument 0 (an oscillator patch), so
    // after a common fill their envelope levels match and velocity
    // alone sets `current_level`.
    const QUIET_PITCH: u8 = 5;

    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, 48_000.0);

    for pitch in 0..MAX_VOICES {
        let pitch = u8::try_from(pitch).unwrap();
        let velocity = if pitch == QUIET_PITCH {
            1
        } else {
            127
        };
        sender
            .push(AudioEvent::NoteOn { sender_mailbox: MailboxId(1), pitch, velocity, instrument_id: 0, pan: 0 })
            .unwrap();
    }
    let mut buf = vec![0.0f32; 64];
    synth.fill(&mut buf, 1);
    assert_eq!(synth.voice_count(), MAX_VOICES);
    assert!(synth.has_voice_with_pitch(QUIET_PITCH));

    // One more note saturates the pool — steal fires.
    sender
        .push(AudioEvent::NoteOn { sender_mailbox: MailboxId(1), pitch: 200, velocity: 100, instrument_id: 0, pan: 0 })
        .unwrap();
    synth.fill(&mut buf, 1);

    // The active pool stays exactly at the cap — the stolen voice
    // moved to the (separate) dying pool, not into `voices`.
    assert_eq!(synth.voice_count(), MAX_VOICES, "active pool must stay at the cap");
    assert!(!synth.has_voice_with_pitch(QUIET_PITCH), "voice steal must evict the quietest note (pitch=5, velocity 1)");
    assert!(synth.has_voice_with_pitch(0), "the loud oldest voice (pitch=0) must survive a quietest steal");
}

/// A stolen voice must fade out over `STEAL_RELEASE_SECS`, not drop to
/// zero in a single sample — the graceful-eviction contract at the
/// kernel level.
#[test]
fn steal_release_fades_rather_than_cutting() {
    let adsr = Adsr { attack_secs: 0.0, decay_secs: 0.0, sustain: 1.0, release_secs: 1.0 };
    let mut voice = OscVoice::new(69, 127, Wave::Sine, adsr, 0.5, 48_000.0, 1);
    let dt = 1.0 / 48_000.0;
    // Advance into sustain (attack/decay are zero-length).
    for _ in 0..10 {
        voice.next_sample(dt);
    }
    assert!(voice.current_level() > 0.0, "voice should be sounding");

    voice.steal_release();
    // Still audible on the very next sample — not an instant cut.
    assert!(voice.current_level() > 0.0, "stolen voice must not drop to zero in one sample");

    // Within STEAL_RELEASE_SECS the fast release completes.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let steps = (STEAL_RELEASE_SECS * 48_000.0).ceil() as usize + 2;
    for _ in 0..steps {
        voice.next_sample(dt);
    }
    assert!(voice.done(), "stolen voice never finished its fast release");
}
