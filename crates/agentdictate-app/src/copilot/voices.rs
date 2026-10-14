//! Telling the other side's speakers apart by voice.
//!
//! System audio mixes everyone on the call into one stream. Each finished
//! utterance gets a voice print, its average timbre (mel-frequency cepstral
//! coefficients) and typical pitch, and a cautious online clustering assigns
//! it to a voice. A new voice is only started from a long, clearly different
//! utterance, so one person is rarely split in two. Utterances where people
//! talk over each other or alternate without a pause get one voice: the
//! segmenter does not split them.

use std::f32::consts::PI;

use super::vad::SAMPLE_RATE;

const FRAME: usize = 512;
const HOP: usize = 256;
const MEL_BANDS: usize = 26;
const COEFFICIENTS: usize = 12;
const MIN_HZ: f32 = 80.0;
const MAX_HZ: f32 = 7_600.0;
/// Voiced audio needed for a print at all.
const MIN_VOICED_SECONDS: f32 = 0.6;
/// Voiced audio needed before a print may start a new voice (an utterance
/// of about two seconds).
const NEW_VOICE_SECONDS: f32 = 1.2;
const MAX_VOICES: usize = 6;
/// Speech frames analyzed per utterance (about eight seconds), evenly spread,
/// which bounds the cost of long monologues on the engine thread.
const MAX_FRAMES: usize = 500;

/// A voice's average timbre and pitch over one utterance.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct VoicePrint {
    pub(crate) timbre: [f32; COEFFICIENTS],
    /// Median log fundamental frequency of the voiced frames.
    pub(crate) log_pitch: Option<f32>,
    pub(crate) voiced_seconds: f32,
}

/// Computes the voice print of 16 kHz mono speech, or `None` when too little
/// of it is voiced.
pub(crate) fn voice_print(samples: &[i16]) -> Option<VoicePrint> {
    if samples.len() < FRAME {
        return None;
    }
    let signal: Vec<f32> = samples
        .iter()
        .map(|&sample| f32::from(sample) / 32_768.0)
        .collect();
    let window: Vec<f32> = (0..FRAME)
        .map(|index| 0.5 - 0.5 * (2.0 * PI * index as f32 / (FRAME - 1) as f32).cos())
        .collect();
    let filters = mel_filters();
    let frames: Vec<&[f32]> = (0..=(signal.len() - FRAME) / HOP)
        .map(|index| &signal[index * HOP..index * HOP + FRAME])
        .collect();
    let energies: Vec<f32> = frames
        .iter()
        .map(|frame| frame.iter().map(|sample| sample * sample).sum::<f32>() / FRAME as f32)
        .collect();
    // Speech frames: the louder part of the utterance, above silence.
    let mut sorted = energies.clone();
    sorted.sort_by(f32::total_cmp);
    let threshold = sorted[sorted.len() * 2 / 5].max(1e-6);
    let mut coefficients: Vec<[f32; COEFFICIENTS]> = Vec::new();
    let mut pitches: Vec<f32> = Vec::new();
    let speech: Vec<&[f32]> = frames
        .iter()
        .zip(&energies)
        .filter(|(_, energy)| **energy >= threshold)
        .map(|(frame, _)| *frame)
        .collect();
    let stride = speech.len().div_ceil(MAX_FRAMES).max(1);
    let mut real = vec![0.0_f32; FRAME];
    let mut imaginary = vec![0.0_f32; FRAME];
    for frame in speech.iter().step_by(stride) {
        for (index, sample) in frame.iter().enumerate() {
            real[index] = sample * window[index];
            imaginary[index] = 0.0;
        }
        fft(&mut real, &mut imaginary);
        let power: Vec<f32> = (0..=FRAME / 2)
            .map(|bin| real[bin] * real[bin] + imaginary[bin] * imaginary[bin])
            .collect();
        let log_mel: Vec<f32> = filters
            .iter()
            .map(|filter| {
                let band: f32 = filter
                    .iter()
                    .map(|(bin, weight)| power[*bin] * weight)
                    .sum();
                (band + 1e-10).ln()
            })
            .collect();
        coefficients.push(cepstrum(&log_mel));
        if let Some(pitch) = pitch(frame) {
            pitches.push(pitch.ln());
        }
    }
    let voiced_seconds = speech.len() as f32 * HOP as f32 / SAMPLE_RATE as f32;
    if voiced_seconds < MIN_VOICED_SECONDS {
        return None;
    }
    let mut timbre = [0.0; COEFFICIENTS];
    for frame in &coefficients {
        for (total, value) in timbre.iter_mut().zip(frame) {
            *total += value;
        }
    }
    for total in &mut timbre {
        *total /= coefficients.len() as f32;
    }
    // Pitch only counts when most speech frames had a clear one.
    let log_pitch = (pitches.len() * 3 >= coefficients.len()).then(|| {
        pitches.sort_by(f32::total_cmp);
        pitches[pitches.len() / 2]
    });
    Some(VoicePrint {
        timbre,
        log_pitch,
        voiced_seconds,
    })
}

/// How different two voices sound. Measured on recorded speech, one voice's
/// prints sit 0.8 to 1.7 apart and different people's 2.8 or more.
pub(crate) fn distance(left: &VoicePrint, right: &VoicePrint) -> f32 {
    // Higher coefficients vary less; scale them up so each counts.
    let timbre = left
        .timbre
        .iter()
        .zip(&right.timbre)
        .enumerate()
        .map(|(index, (left, right))| {
            let weight = TIMBRE_SCALE * (1.0 + index as f32 * 0.5);
            ((left - right) * weight).powi(2)
        })
        .sum::<f32>()
        .sqrt();
    let pitch = match (left.log_pitch, right.log_pitch) {
        // A semitone is about 0.058 in log frequency.
        (Some(left), Some(right)) => ((left - right).abs() / PITCH_SCALE).min(3.0),
        _ => 0.0,
    };
    (timbre * timbre + pitch * pitch).sqrt()
}

const TIMBRE_SCALE: f32 = 0.25;
const PITCH_SCALE: f32 = 0.25;

struct Voice {
    print: VoicePrint,
    weight: f32,
}

/// Assigns utterances to voices as they arrive.
#[derive(Default)]
pub(crate) struct VoiceTracker {
    voices: Vec<Voice>,
}

impl VoiceTracker {
    /// The voice an utterance belongs to, from 0; `None` without a print.
    pub(crate) fn assign(&mut self, print: Option<VoicePrint>) -> Option<u8> {
        let print = print?;
        let nearest = self
            .voices
            .iter()
            .enumerate()
            .map(|(index, voice)| (index, distance(&voice.print, &print)))
            .min_by(|left, right| left.1.total_cmp(&right.1));
        let index = match nearest {
            Some((index, gap)) if gap < SAME_VOICE => index,
            Some((index, _))
                if print.voiced_seconds < NEW_VOICE_SECONDS || self.voices.len() >= MAX_VOICES =>
            {
                index
            }
            None if print.voiced_seconds < MIN_VOICED_SECONDS => return None,
            _ => {
                self.voices.push(Voice {
                    weight: print.voiced_seconds,
                    print,
                });
                return Some((self.voices.len() - 1) as u8);
            }
        };
        let voice = &mut self.voices[index];
        let total = voice.weight + print.voiced_seconds;
        let mix = |old: f32, new: f32| (old * voice.weight + new * print.voiced_seconds) / total;
        for (old, new) in voice.print.timbre.iter_mut().zip(&print.timbre) {
            *old = mix(*old, *new);
        }
        voice.print.log_pitch = match (voice.print.log_pitch, print.log_pitch) {
            (Some(old), Some(new)) => Some(mix(old, new)),
            (old, new) => old.or(new),
        };
        voice.weight = total.min(60.0);
        Some(index as u8)
    }
}

/// Prints closer than this to a voice belong to it: between the spread of
/// one voice and the gap to the next.
const SAME_VOICE: f32 = 2.2;

/// Triangular mel filters as (bin, weight) pairs.
fn mel_filters() -> Vec<Vec<(usize, f32)>> {
    let mel = |hz: f32| 2_595.0 * (1.0 + hz / 700.0).log10();
    let hz = |mel: f32| 700.0 * (10_f32.powf(mel / 2_595.0) - 1.0);
    let bin_hz = SAMPLE_RATE as f32 / FRAME as f32;
    let points: Vec<f32> = (0..MEL_BANDS + 2)
        .map(|index| {
            hz(mel(MIN_HZ) + (mel(MAX_HZ) - mel(MIN_HZ)) * index as f32 / (MEL_BANDS + 1) as f32)
        })
        .collect();
    (0..MEL_BANDS)
        .map(|band| {
            let (low, centre, high) = (points[band], points[band + 1], points[band + 2]);
            (0..=FRAME / 2)
                .filter_map(|bin| {
                    let frequency = bin as f32 * bin_hz;
                    let weight = if frequency > low && frequency <= centre {
                        (frequency - low) / (centre - low)
                    } else if frequency > centre && frequency < high {
                        (high - frequency) / (high - centre)
                    } else {
                        0.0
                    };
                    (weight > 0.0).then_some((bin, weight))
                })
                .collect()
        })
        .collect()
}

/// Cepstral coefficients 1 to 12 (the DCT of log mel energies, without the
/// loudness term).
fn cepstrum(log_mel: &[f32]) -> [f32; COEFFICIENTS] {
    let mut coefficients = [0.0; COEFFICIENTS];
    let bands = log_mel.len() as f32;
    for (index, coefficient) in coefficients.iter_mut().enumerate() {
        let order = (index + 1) as f32;
        *coefficient = log_mel
            .iter()
            .enumerate()
            .map(|(band, value)| value * (PI * order * (band as f32 + 0.5) / bands).cos())
            .sum::<f32>()
            * (2.0 / bands).sqrt();
    }
    coefficients
}

/// The frame's fundamental frequency by autocorrelation, if clearly voiced.
fn pitch(frame: &[f32]) -> Option<f32> {
    let lags = (SAMPLE_RATE as usize / 400)..=(SAMPLE_RATE as usize / 70);
    let energy: f32 = frame.iter().map(|sample| sample * sample).sum();
    if energy <= 1e-8 {
        return None;
    }
    let (lag, correlation) = lags
        .map(|lag| {
            let correlation: f32 = frame[..FRAME - lag]
                .iter()
                .zip(&frame[lag..])
                .map(|(left, right)| left * right)
                .sum();
            (lag, correlation / energy)
        })
        .max_by(|left, right| left.1.total_cmp(&right.1))?;
    (correlation > 0.45).then(|| SAMPLE_RATE as f32 / lag as f32)
}

/// In-place iterative radix-2 FFT; `real.len()` must be a power of two.
fn fft(real: &mut [f32], imaginary: &mut [f32]) {
    let size = real.len();
    let mut target = 0;
    for index in 1..size {
        let mut bit = size >> 1;
        while target & bit != 0 {
            target ^= bit;
            bit >>= 1;
        }
        target |= bit;
        if index < target {
            real.swap(index, target);
            imaginary.swap(index, target);
        }
    }
    let mut length = 2;
    while length <= size {
        let angle = -2.0 * PI / length as f32;
        for start in (0..size).step_by(length) {
            for offset in 0..length / 2 {
                let (sin, cos) = (angle * offset as f32).sin_cos();
                let even = start + offset;
                let odd = even + length / 2;
                let odd_real = real[odd] * cos - imaginary[odd] * sin;
                let odd_imaginary = real[odd] * sin + imaginary[odd] * cos;
                real[odd] = real[even] - odd_real;
                imaginary[odd] = imaginary[even] - odd_imaginary;
                real[even] += odd_real;
                imaginary[even] += odd_imaginary;
            }
        }
        length <<= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A voiced sound: harmonics of `pitch` shaped by a spectral tilt, with
    /// a slow syllable envelope.
    fn voice(pitch: f32, tilt: f32, seconds: f32, seed: u32) -> Vec<i16> {
        let count = (seconds * SAMPLE_RATE as f32) as usize;
        let mut state = seed;
        (0..count)
            .map(|index| {
                let time = index as f32 / SAMPLE_RATE as f32;
                let envelope = 0.55 + 0.45 * (2.0 * PI * 3.0 * time).sin().abs();
                let mut value = 0.0;
                for harmonic in 1..30 {
                    let frequency = pitch * harmonic as f32;
                    if frequency > 7_000.0 {
                        break;
                    }
                    value += (2.0 * PI * frequency * time).sin() / (harmonic as f32).powf(tilt);
                }
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (state >> 9) as f32 / (1 << 23) as f32 - 1.0;
                ((value * 0.08 + noise * 0.002) * envelope * 32_767.0) as i16
            })
            .collect()
    }

    #[test]
    fn the_fft_finds_a_tone() {
        let mut real: Vec<f32> = (0..FRAME)
            .map(|index| (2.0 * PI * 16.0 * index as f32 / FRAME as f32).cos())
            .collect();
        let mut imaginary = vec![0.0; FRAME];
        fft(&mut real, &mut imaginary);
        let peak = (0..FRAME / 2)
            .max_by(|left, right| real[*left].abs().total_cmp(&real[*right].abs()))
            .unwrap();
        assert_eq!(peak, 16);
    }

    #[test]
    fn pitch_follows_the_fundamental() {
        let print = voice_print(&voice(120.0, 1.2, 2.0, 1)).unwrap();
        let hz = print.log_pitch.unwrap().exp();
        assert!((hz - 120.0).abs() < 6.0, "{hz}");
        assert!(voice_print(&vec![0; 4_000]).is_none());
    }

    #[test]
    fn the_same_voice_stays_together_and_a_different_one_is_told_apart() {
        let mut tracker = VoiceTracker::default();
        let low = |seed| voice_print(&voice(110.0, 1.4, 3.0, seed));
        let high = |seed| voice_print(&voice(210.0, 0.9, 3.0, seed));
        assert_eq!(tracker.assign(low(1)), Some(0));
        assert_eq!(tracker.assign(low(2)), Some(0));
        assert_eq!(tracker.assign(high(3)), Some(1));
        assert_eq!(tracker.assign(low(4)), Some(0));
        assert_eq!(tracker.assign(high(5)), Some(1));
        // A short clip never starts a voice; it joins the nearest one.
        let short = voice_print(&voice(160.0, 0.5, 1.6, 6));
        assert!(tracker.assign(short).is_some_and(|voice| voice < 2));
        assert_eq!(tracker.assign(None), None);
    }
}
