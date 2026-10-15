//! Audio decoding, resampling and splitting.
//!
//! Uploads in any common format (WAV, FLAC, MP3, AAC/M4A, Ogg Vorbis, ...) are
//! decoded with Symphonia, mixed to mono and resampled to 16 kHz, the format
//! Whisper expects. Decoding happens in memory only; audio never touches disk.

use std::io::Cursor;

use anyhow::{Context, bail};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use verbarelay_protocol::SAMPLE_RATE;

/// Decoded audio: 16 kHz mono 16-bit PCM.
#[derive(Debug, Clone)]
pub struct Pcm {
    pub samples: Vec<i16>,
}

impl Pcm {
    pub fn duration_ms(&self) -> u64 {
        self.samples.len() as u64 * 1000 / u64::from(SAMPLE_RATE)
    }
}

/// Decodes an uploaded file. `file_name` only serves as a format hint.
pub fn decode(data: Vec<u8>, file_name: Option<&str>) -> anyhow::Result<Pcm> {
    let mut hint = Hint::new();
    if let Some(ext) = file_name.and_then(|n| n.rsplit_once('.')).map(|(_, e)| e) {
        hint.with_extension(ext);
    }
    let source = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());
    let probed = symphonia::default::get_probe()
        .format(&hint, source, &FormatOptions::default(), &MetadataOptions::default())
        .context("unsupported or corrupt audio file")?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .context("the file contains no audio track")?;
    let track_id = track.id;
    let source_rate = track.codec_params.sample_rate.context("unknown sample rate")?;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .context("unsupported audio codec")?;

    let mut mono: Vec<f32> = Vec::new();
    let mut buffer: Option<SampleBuffer<f32>> = None;
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymphoniaError::ResetRequired) => break,
            Err(e) => return Err(e).context("reading audio packets"),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            // Skip damaged packets instead of failing the whole file.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(e).context("decoding audio"),
        };
        let spec = *decoded.spec();
        let channels = spec.channels.count().max(1);
        let buf = buffer.get_or_insert_with(|| SampleBuffer::new(decoded.capacity() as u64, spec));
        if buf.capacity() < decoded.capacity() * channels {
            *buf = SampleBuffer::new(decoded.capacity() as u64, spec);
        }
        buf.copy_interleaved_ref(decoded);
        for frame in buf.samples().chunks_exact(channels) {
            mono.push(frame.iter().sum::<f32>() / channels as f32);
        }
    }

    if mono.is_empty() {
        bail!("the file contains no audio samples");
    }
    let resampled = resample(&mono, source_rate, SAMPLE_RATE);
    let samples = resampled
        .iter()
        .map(|s| (s.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16)
        .collect();
    Ok(Pcm { samples })
}

/// Resamples with a windowed-sinc low-pass filter followed by linear interpolation.
/// Good enough for speech recognition and free of extra dependencies.
pub fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }
    // Only the input positions used by the interpolation are filtered, which keeps
    // long recordings fast.
    let kernel = (to < from).then(|| low_pass_kernel(0.45 * to as f32 / from as f32));
    let sample_at = |idx: usize| -> f32 {
        let idx = idx.min(input.len() - 1);
        match &kernel {
            Some(k) => filter_at(input, k, idx),
            None => input[idx],
        }
    };
    let ratio = f64::from(from) / f64::from(to);
    let out_len = ((input.len() as f64) / ratio).floor() as usize;
    (0..out_len)
        .map(|i| {
            let pos = i as f64 * ratio;
            let idx = pos.floor() as usize;
            let frac = (pos - idx as f64) as f32;
            let a = sample_at(idx);
            let b = sample_at(idx + 1);
            a + (b - a) * frac
        })
        .collect()
}

fn filter_at(input: &[f32], kernel: &[f32], i: usize) -> f32 {
    let half = kernel.len() / 2;
    kernel
        .iter()
        .enumerate()
        .filter_map(|(k, w)| {
            (i + k)
                .checked_sub(half)
                .filter(|&j| j < input.len())
                .map(|j| input[j] * w)
        })
        .sum()
}

/// FIR low-pass kernel with a Blackman-windowed sinc; `cutoff` is relative to the sample rate.
fn low_pass_kernel(cutoff: f32) -> Vec<f32> {
    const TAPS: usize = 63;
    let mid = (TAPS / 2) as f32;
    let mut kernel: Vec<f32> = (0..TAPS)
        .map(|i| {
            let x = i as f32 - mid;
            let sinc = if x == 0.0 {
                2.0 * cutoff
            } else {
                (2.0 * std::f32::consts::PI * cutoff * x).sin() / (std::f32::consts::PI * x)
            };
            let n = i as f32 / (TAPS - 1) as f32;
            let window =
                0.42 - 0.5 * (2.0 * std::f32::consts::PI * n).cos() + 0.08 * (4.0 * std::f32::consts::PI * n).cos();
            sinc * window
        })
        .collect();
    let sum: f32 = kernel.iter().sum();
    kernel.iter_mut().for_each(|k| *k /= sum);
    kernel
}

/// Chooses split points (sample indices) so the recording falls into `parts`
/// pieces of roughly equal length, each cut placed in the quietest 300 ms window
/// within 15 seconds of the ideal position, so words are not cut in half.
pub fn split_points(samples: &[i16], parts: usize) -> Vec<usize> {
    if parts < 2 || samples.is_empty() {
        return Vec::new();
    }
    let rate = SAMPLE_RATE as usize;
    let window = rate * 3 / 10;
    let search = rate * 15;
    let step = rate / 20;

    let mut points = Vec::with_capacity(parts - 1);
    for p in 1..parts {
        let target = samples.len() * p / parts;
        let start = target.saturating_sub(search);
        let end = (target + search).min(samples.len().saturating_sub(window));
        // Quietest window wins; among equally quiet ones the one closest to the target.
        let mut best = (u64::MAX, usize::MAX, target);
        let mut pos = start;
        while pos <= end {
            let energy: u64 = samples[pos..pos + window]
                .iter()
                .map(|s| u64::from(s.unsigned_abs()))
                .sum();
            // Treat nearly equal energies as equal so the distance decides.
            let energy = energy / (window as u64 * 16).max(1);
            let center = pos + window / 2;
            let distance = center.abs_diff(target);
            if (energy, distance) < (best.0, best.1) {
                best = (energy, distance, center);
            }
            pos += step;
        }
        let point = best.2.clamp(points.last().copied().unwrap_or(0) + 1, samples.len() - 1);
        points.push(point);
    }
    points
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav_bytes(rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let data_len = (samples.len() * 2) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
        out.extend_from_slice(&(channels * 2).to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for s in samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    #[test]
    fn decodes_and_resamples_stereo_wav() {
        // One second of a 440 Hz tone at 48 kHz stereo.
        let samples: Vec<i16> = (0..48_000)
            .flat_map(|i| {
                let v = ((i as f32 * 440.0 * 2.0 * std::f32::consts::PI / 48_000.0).sin() * 10_000.0) as i16;
                [v, v]
            })
            .collect();
        let pcm = decode(wav_bytes(48_000, 2, &samples), Some("tone.wav")).unwrap();
        assert!((15_900..=16_000).contains(&pcm.samples.len()), "{}", pcm.samples.len());
        assert!((990..=1000).contains(&pcm.duration_ms()));
        let peak = pcm.samples.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(peak > 8_000, "tone should survive resampling, peak {peak}");
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode(vec![1, 2, 3, 4, 5], Some("x.wav")).is_err());
    }

    #[test]
    fn prefers_the_pause_nearest_the_middle() {
        // Pauses at 11 s and 23 s in a 48 s recording: the cut belongs at 23 s.
        let rate = SAMPLE_RATE as usize;
        let mut samples = vec![5_000i16; rate * 48];
        for start in [11, 23, 35] {
            for s in &mut samples[rate * start..rate * (start + 1)] {
                *s = 0;
            }
        }
        let points = split_points(&samples, 2);
        assert!(points[0] > rate * 23 && points[0] < rate * 24, "{}", points[0] / rate);
    }

    #[test]
    fn splits_at_silence() {
        // 60 s of "speech" with a silent second around 31 s.
        let rate = SAMPLE_RATE as usize;
        let mut samples = vec![5_000i16; rate * 60];
        for s in &mut samples[rate * 31..rate * 32] {
            *s = 0;
        }
        let points = split_points(&samples, 2);
        assert_eq!(points.len(), 1);
        assert!(points[0] > rate * 31 && points[0] < rate * 32, "{}", points[0]);
    }
}
