//! Notification tones, synthesized on-device from the fixed specs below.
//!
//! Nothing here is user-editable: the specs are compiled into the binary and
//! rendered to 16-bit mono PCM on demand. Each note is a small additive voice
//! (a handful of partials with their own exponential decays), the notes are
//! summed, run through a few early-reflection taps for some air, normalized to
//! the tone's level and faded out at the end so nothing clicks.
//!
//! Playback is best effort: without a sound device (a headless service, say)
//! nothing plays and nothing fails. On Windows the tone goes to `PlaySoundW`
//! from memory; elsewhere it is handed to the first available command-line
//! player (`pw-play`, `paplay`, `aplay`, `afplay`), so no audio libraries are
//! linked in.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tracing::debug;

use crate::config::ToneSettings;

pub const SAMPLE_RATE: u32 = 48_000;

// Off until the agent applies the configuration, so other subcommands stay silent.
static ENABLED: AtomicBool = AtomicBool::new(false);
static JOBS: AtomicBool = AtomicBool::new(false);
// One tone at a time: later tones wait instead of cutting off or overlapping earlier ones.
static PLAYING: Mutex<()> = Mutex::new(());

/// Applies the tone settings for all later [`play`] calls.
pub fn configure(settings: &ToneSettings) {
    ENABLED.store(settings.enabled, Ordering::Relaxed);
    JOBS.store(settings.jobs, Ordering::Relaxed);
}

fn allowed(tone: Tone) -> bool {
    ENABLED.load(Ordering::Relaxed)
        && (!matches!(tone, Tone::JobReceived | Tone::JobFinished) || JOBS.load(Ordering::Relaxed))
}

/// Plays a tone in the background if the settings allow it.
pub fn play(tone: Tone) {
    if allowed(tone) {
        std::thread::spawn(move || output(tone));
    }
}

/// Plays a tone and waits for it to finish, for tones right before the process exits.
pub fn play_blocking(tone: Tone) {
    if allowed(tone) {
        output(tone);
    }
}

#[cfg(windows)]
fn output(tone: Tone) {
    let _turn = PLAYING.lock().unwrap_or_else(|e| e.into_inner());
    use windows_sys::Win32::Media::Audio::{PlaySoundW, SND_MEMORY, SND_NODEFAULT, SND_SYNC};

    let bytes = wav(&render(tone));
    // SAFETY: with SND_MEMORY the "name" points to a complete WAV image, which
    // outlives the synchronous call.
    let ok = unsafe {
        PlaySoundW(
            bytes.as_ptr().cast(),
            std::ptr::null_mut(),
            SND_MEMORY | SND_SYNC | SND_NODEFAULT,
        )
    };
    if ok == 0 {
        debug!(tone = tone.name(), "playing the tone failed");
    }
}

#[cfg(not(windows))]
fn output(tone: Tone) {
    let _turn = PLAYING.lock().unwrap_or_else(|e| e.into_inner());
    use std::process::{Command, Stdio};

    const PLAYERS: &[(&str, &[&str])] = &[("pw-play", &[]), ("paplay", &[]), ("aplay", &["-q"]), ("afplay", &[])];

    let path = std::env::temp_dir().join(format!("verbarelay-{}-{}.wav", std::process::id(), tone.name()));
    if let Err(e) = std::fs::write(&path, wav(&render(tone))) {
        debug!(tone = tone.name(), "writing the tone failed: {e}");
        return;
    }
    let played = PLAYERS.iter().any(|(player, args)| {
        Command::new(player)
            .args(*args)
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    });
    if !played {
        debug!(tone = tone.name(), "no sound player could play the tone");
    }
    let _ = std::fs::remove_file(&path);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Startup,
    UpdateFound,
    UpdateDownloaded,
    Restart,
    Shutdown,
    JobReceived,
    JobFinished,
}

impl Tone {
    #[cfg(test)]
    pub const ALL: [Tone; 7] = [
        Tone::Startup,
        Tone::UpdateFound,
        Tone::UpdateDownloaded,
        Tone::Restart,
        Tone::Shutdown,
        Tone::JobReceived,
        Tone::JobFinished,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Tone::Startup => "startup",
            Tone::UpdateFound => "update-found",
            Tone::UpdateDownloaded => "update-downloaded",
            Tone::Restart => "restart",
            Tone::Shutdown => "shutdown",
            Tone::JobReceived => "job-received",
            Tone::JobFinished => "job-finished",
        }
    }

    fn spec(self) -> &'static Spec {
        match self {
            Tone::Startup => &STARTUP,
            Tone::UpdateFound => &UPDATE_FOUND,
            Tone::UpdateDownloaded => &UPDATE_DOWNLOADED,
            Tone::Restart => &RESTART,
            Tone::Shutdown => &SHUTDOWN,
            Tone::JobReceived => &JOB_RECEIVED,
            Tone::JobFinished => &JOB_FINISHED,
        }
    }
}

/// One sine component of a voice: frequency ratio to the note, relative
/// amplitude, and decay time as a fraction of the note's decay.
struct Partial {
    ratio: f32,
    amp: f32,
    decay: f32,
}

struct Note {
    /// Onset in seconds from the start of the tone.
    at: f32,
    /// MIDI note number (69 = A4 = 440 Hz).
    midi: f32,
    /// Time constant of the fundamental's exponential decay, in seconds.
    decay: f32,
    gain: f32,
}

struct Spec {
    timbre: &'static [Partial],
    notes: &'static [Note],
    /// Total length in seconds, including the tail.
    length: f32,
    /// Peak amplitude after normalization (1.0 = full scale).
    level: f32,
}

/// Short synthetic blip: the marimba-like 1:4:10 overtones die quickly.
const BLIP: &[Partial] = &[
    Partial {
        ratio: 1.0,
        amp: 1.0,
        decay: 1.0,
    },
    Partial {
        ratio: 3.93,
        amp: 0.22,
        decay: 0.3,
    },
    Partial {
        ratio: 9.8,
        amp: 0.05,
        decay: 0.12,
    },
];

const fn note(at: f32, midi: f32, decay: f32, gain: f32) -> Note {
    Note { at, midi, decay, gain }
}

// Rising C major arpeggio up to the octave.
const STARTUP: Spec = Spec {
    timbre: BLIP,
    notes: &[
        note(0.00, 72.0, 0.08, 0.85),
        note(0.07, 76.0, 0.08, 0.85),
        note(0.14, 79.0, 0.08, 0.90),
        note(0.21, 84.0, 0.14, 1.00),
    ],
    length: 0.7,
    level: 0.30,
};

// A rising fourth: "something's available".
const UPDATE_FOUND: Spec = Spec {
    timbre: BLIP,
    notes: &[note(0.00, 81.0, 0.08, 0.85), note(0.09, 86.0, 0.12, 1.00)],
    length: 0.5,
    level: 0.28,
};

// G major triad going up: "ready".
const UPDATE_DOWNLOADED: Spec = Spec {
    timbre: BLIP,
    notes: &[
        note(0.00, 79.0, 0.07, 0.85),
        note(0.07, 83.0, 0.07, 0.85),
        note(0.14, 86.0, 0.14, 1.00),
    ],
    length: 0.6,
    level: 0.28,
};

// Down a fourth and back up: going away and coming back.
const RESTART: Spec = Spec {
    timbre: BLIP,
    notes: &[
        note(0.00, 79.0, 0.07, 0.90),
        note(0.08, 74.0, 0.07, 0.85),
        note(0.22, 74.0, 0.07, 0.85),
        note(0.30, 79.0, 0.12, 1.00),
    ],
    length: 0.7,
    level: 0.28,
};

// The startup arpeggio in reverse, settling low.
const SHUTDOWN: Spec = Spec {
    timbre: BLIP,
    notes: &[
        note(0.00, 84.0, 0.08, 0.90),
        note(0.07, 79.0, 0.08, 0.85),
        note(0.14, 76.0, 0.08, 0.85),
        note(0.21, 72.0, 0.16, 1.00),
    ],
    length: 0.75,
    level: 0.30,
};

// Single quiet tick.
const JOB_RECEIVED: Spec = Spec {
    timbre: BLIP,
    notes: &[note(0.00, 88.0, 0.08, 1.00)],
    length: 0.30,
    level: 0.22,
};

// Two quick ticks up a fifth: "done".
const JOB_FINISHED: Spec = Spec {
    timbre: BLIP,
    notes: &[note(0.00, 84.0, 0.07, 0.85), note(0.07, 91.0, 0.10, 1.00)],
    length: 0.40,
    level: 0.22,
};

/// Early reflections: (delay in seconds, gain).
const REFLECTIONS: &[(f32, f32)] = &[(0.023, 0.16), (0.037, 0.12), (0.053, 0.09), (0.079, 0.06)];
const ATTACK: f32 = 0.004;
const FADE_OUT: f32 = 0.04;

/// Renders a tone to samples in [-1, 1] at [`SAMPLE_RATE`].
pub fn render(tone: Tone) -> Vec<f32> {
    let spec = tone.spec();
    let rate = SAMPLE_RATE as f32;
    let len = (spec.length * rate) as usize;
    let mut dry = vec![0.0f32; len];

    for note in spec.notes {
        let freq = 440.0 * 2f32.powf((note.midi - 69.0) / 12.0);
        let start = (note.at * rate) as usize;
        for (i, sample) in dry.iter_mut().enumerate().skip(start) {
            let t = (i - start) as f32 / rate;
            let attack = if t < ATTACK {
                0.5 - 0.5 * (std::f32::consts::PI * t / ATTACK).cos()
            } else {
                1.0
            };
            let mut v = 0.0;
            for p in spec.timbre {
                let f = freq * p.ratio;
                if f >= rate * 0.45 {
                    continue;
                }
                let env = (-t / (note.decay * p.decay)).exp();
                v += p.amp * env * (std::f32::consts::TAU * f * t).sin();
            }
            *sample += note.gain * attack * v;
        }
    }

    let mut out = dry.clone();
    for &(delay, gain) in REFLECTIONS {
        let offset = (delay * rate) as usize;
        for i in offset..len {
            out[i] += gain * dry[i - offset];
        }
    }

    let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    let scale = if peak > 0.0 { spec.level / peak } else { 0.0 };
    let fade = (FADE_OUT * rate) as usize;
    for (i, s) in out.iter_mut().enumerate() {
        let remaining = len - i;
        let tail = if remaining < fade {
            remaining as f32 / fade as f32
        } else {
            1.0
        };
        *s *= scale * tail;
    }
    out
}

/// Encodes samples as a 16-bit mono PCM WAV file.
pub fn wav(samples: &[f32]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tones_are_bounded_and_click_free() {
        for tone in Tone::ALL {
            let spec = tone.spec();
            let samples = render(tone);
            let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            assert!((peak - spec.level).abs() < 1e-3, "{}: peak {peak}", tone.name());
            assert!(samples[0].abs() < 1e-3, "{} starts with a click", tone.name());
            assert!(
                samples.last().unwrap().abs() < 1e-3,
                "{} ends with a click",
                tone.name()
            );
        }
    }

    #[test]
    fn wav_header_matches_data() {
        let bytes = wav(&render(Tone::JobReceived));
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(
            u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize,
            bytes.len() - 44
        );
    }

    #[test]
    fn job_tones_need_both_switches() {
        configure(&ToneSettings {
            enabled: true,
            jobs: false,
        });
        assert!(allowed(Tone::Startup));
        assert!(!allowed(Tone::JobFinished));
        configure(&ToneSettings {
            enabled: false,
            jobs: true,
        });
        assert!(!allowed(Tone::Startup));
        assert!(!allowed(Tone::JobReceived));
        configure(&ToneSettings {
            enabled: true,
            jobs: true,
        });
        assert!(allowed(Tone::JobReceived));
    }

    /// Plays every tone on this machine's speakers.
    #[test]
    #[ignore]
    fn play_all() {
        for tone in Tone::ALL {
            output(tone);
        }
    }

    /// Writes every tone to `$VR_TONE_PREVIEW_DIR` for listening.
    #[test]
    #[ignore]
    fn write_previews() {
        let dir = std::path::PathBuf::from(std::env::var("VR_TONE_PREVIEW_DIR").unwrap());
        std::fs::create_dir_all(&dir).unwrap();
        for (i, tone) in Tone::ALL.into_iter().enumerate() {
            let path = dir.join(format!("{}-{}.wav", i + 1, tone.name()));
            std::fs::write(path, wav(&render(tone))).unwrap();
        }
    }
}
