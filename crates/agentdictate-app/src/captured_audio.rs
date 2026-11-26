use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

/// Locate PCM data without assuming a 44-byte header. `pw-record` can write extra chunks.
pub(crate) fn data_start(file: &mut File) -> anyhow::Result<u64> {
    let mut header = [0; 12];
    file.read_exact(&mut header)?;
    anyhow::ensure!(
        &header[..4] == b"RIFF" && &header[8..] == b"WAVE",
        "not WAV"
    );
    let mut format_seen = false;
    loop {
        anyhow::ensure!(file.stream_position()? < 65536, "oversized WAV header");
        let mut chunk = [0; 8];
        file.read_exact(&mut chunk)?;
        let size = u32::from_le_bytes(chunk[4..].try_into()?) as u64;
        if &chunk[..4] == b"data" {
            anyhow::ensure!(format_seen, "PCM format missing");
            return Ok(file.stream_position()?);
        }
        let start = file.stream_position()?;
        if &chunk[..4] == b"fmt " {
            anyhow::ensure!(size >= 16, "short format");
            let mut fmt = [0; 16];
            file.read_exact(&mut fmt)?;
            let encoding = u16::from_le_bytes(fmt[0..2].try_into()?);
            anyhow::ensure!(encoding == 1 || encoding == 65534, "not PCM");
            anyhow::ensure!(
                u16::from_le_bytes(fmt[2..4].try_into()?) == 1
                    && u32::from_le_bytes(fmt[4..8].try_into()?) == 16000
                    && u16::from_le_bytes(fmt[14..16].try_into()?) == 16,
                "expected mono 16kHz PCM16"
            );
            if encoding == 65534 {
                anyhow::ensure!(size >= 40, "short extensible format");
                let mut extension = [0; 24];
                file.read_exact(&mut extension)?;
                anyhow::ensure!(
                    extension[8..24] == [1, 0, 0, 0, 0, 0, 16, 0, 128, 0, 0, 170, 0, 56, 155, 113],
                    "not extensible PCM"
                );
            }
            format_seen = true;
        }
        file.seek(SeekFrom::Start(
            start
                .checked_add(size + size % 2)
                .ok_or_else(|| anyhow::anyhow!("chunk overflow"))?,
        ))?;
    }
}

/// Only a valid, finalized, near-silent PCM capture may turn an empty ASR result
/// into a harmless stop. Unknown formats and audible clips remain recoverable.
pub(crate) fn is_near_silent(path: &Path) -> bool {
    inspect_quiet_pcm(path).unwrap_or(false)
}

/// A quick shortcut release can finalize only one 10 ms input buffer. Skip
/// these accidental taps before ASR; keep clips of 50 ms or more, including
/// short words. Inspect the finalized PCM rather than trusting job duration.
pub(crate) fn is_accidental_tap(path: &Path) -> bool {
    let inspect = || -> anyhow::Result<bool> {
        let mut file = File::open(path)?;
        let start = data_start(&mut file)?;
        file.seek(SeekFrom::Start(start - 4))?;
        let mut length = [0; 4];
        file.read_exact(&mut length)?;
        let bytes = u64::from(u32::from_le_bytes(length));
        anyhow::ensure!(
            bytes % 2 == 0 && start + bytes <= file.metadata()?.len(),
            "invalid PCM length"
        );
        Ok(bytes < 1600) // 50 ms of mono 16 kHz PCM16.
    };
    inspect().unwrap_or(false)
}

/// Window over which loudness is measured: 30 ms of 16 kHz audio, short enough
/// to resolve one syllable and long enough to average out single samples.
const ENERGY_WINDOW_SAMPLES: usize = 480;
/// RMS level, about -45 dBFS, above which a window holds more than room tone.
/// Recorded speech measures -27 to -35 dBFS; the near-silent clips that were
/// misreported as failed dictations measure -57 to -63 dBFS.
const AUDIBLE_WINDOW_RMS: f64 = 184.0;
/// Consecutive audible windows (90 ms) that make a clip audible. A spoken
/// syllable sustains energy this long, even a quiet "Hello?", while a key or
/// mouse click lasts a few milliseconds and lifts at most two windows.
const AUDIBLE_RUN_WINDOWS: usize = 3;

/// Near-silent means no 90 ms stretch rises above -45 dBFS. Clicks and other
/// brief transients do not count as sound; anything sustained, including a
/// steady hum, does, so the clip stays recoverable.
fn inspect_quiet_pcm(path: &Path) -> anyhow::Result<bool> {
    let mut file = File::open(path)?;
    let start = data_start(&mut file)?;
    file.seek(SeekFrom::Start(start - 4))?;
    let mut length = [0; 4];
    file.read_exact(&mut length)?;
    let mut remaining = u64::from(u32::from_le_bytes(length));
    anyhow::ensure!(
        remaining > 0 && remaining % 2 == 0 && start + remaining <= file.metadata()?.len(),
        "invalid PCM length"
    );
    let mut loudness = WindowedLoudness::default();
    let mut buffer = [0; 8192];
    while remaining > 0 {
        let count = remaining.min(buffer.len() as u64) as usize;
        file.read_exact(&mut buffer[..count])?;
        for pair in buffer[..count].chunks_exact(2) {
            if loudness.push(i16::from_le_bytes([pair[0], pair[1]])) {
                return Ok(false);
            }
        }
        remaining -= count as u64;
    }
    Ok(!loudness.finish())
}

/// Tracks runs of consecutive audible energy windows.
#[derive(Default)]
struct WindowedLoudness {
    energy: f64,
    samples: usize,
    audible_run: usize,
}

impl WindowedLoudness {
    /// Adds one sample; true once a sustained audible run has been seen.
    fn push(&mut self, sample: i16) -> bool {
        let sample = f64::from(sample);
        self.energy += sample * sample;
        self.samples += 1;
        self.samples == ENERGY_WINDOW_SAMPLES && self.close_window()
    }

    /// Closes a trailing partial window; true if the clip is audible.
    fn finish(&mut self) -> bool {
        self.samples > 0 && self.close_window()
    }

    fn close_window(&mut self) -> bool {
        let audible = self.energy / self.samples as f64 > AUDIBLE_WINDOW_RMS * AUDIBLE_WINDOW_RMS;
        self.energy = 0.0;
        self.samples = 0;
        self.audible_run = if audible { self.audible_run + 1 } else { 0 };
        self.audible_run >= AUDIBLE_RUN_WINDOWS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accidental_taps_use_validated_pcm_length_and_preserve_short_words() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capture.wav");
        for (samples, skipped) in [
            (0, true),
            (160, true),
            (799, true),
            (800, false),
            (1600, false),
        ] {
            let mut wav = b"RIFF\0\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x80\x3e\0\0\0\x7d\0\0\x02\0\x10\0data\0\0\0\0".to_vec();
            wav[4..8].copy_from_slice(&(36u32 + samples * 2).to_le_bytes());
            wav[40..44].copy_from_slice(&(samples * 2).to_le_bytes());
            for _ in 0..samples {
                wav.extend_from_slice(&2000i16.to_le_bytes());
            }
            std::fs::write(&path, wav).unwrap();
            assert_eq!(is_accidental_tap(&path), skipped);
        }
        let mut wav = std::fs::read(&path).unwrap();
        wav.truncate(100); // A truncated longer recording must remain recoverable.
        std::fs::write(&path, wav).unwrap();
        assert!(!is_accidental_tap(&path));
        std::fs::write(&path, b"broken recording").unwrap();
        assert!(!is_accidental_tap(&path));
        assert!(!is_accidental_tap(&dir.path().join("missing.wav")));
    }
    #[test]
    fn quiet_pcm_ignores_metadata_but_never_accepts_audible_or_invalid_audio() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capture.wav");
        for (sample, quiet) in [(0i16, true), (12, true), (2000, false)] {
            let mut wav = b"RIFF\0\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x80\x3e\0\0\0\x7d\0\0\x02\0\x10\0data\x80\x0c\0\0".to_vec();
            for _ in 0..1600 {
                wav.extend_from_slice(&sample.to_le_bytes());
            }
            wav.extend_from_slice(b"JUNK\x04\0\0\0loud");
            std::fs::write(&path, wav).unwrap();
            assert_eq!(is_near_silent(&path), quiet);
        }
        std::fs::write(&path, b"broken recording").unwrap();
        assert!(!is_near_silent(&path));
        assert!(!is_near_silent(&dir.path().join("missing.wav")));
    }

    /// A canonical 16 kHz mono PCM16 WAV holding `samples`.
    fn wav(samples: &[i16]) -> Vec<u8> {
        let data_len = (samples.len() * 2) as u32;
        let mut wav =
            b"RIFF\0\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x80\x3e\0\0\0\x7d\0\0\x02\0\x10\0data"
                .to_vec();
        wav[4..8].copy_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(&data_len.to_le_bytes());
        for sample in samples {
            wav.extend_from_slice(&sample.to_le_bytes());
        }
        wav
    }

    /// Two seconds of deterministic room tone within +/-20 (about -67 dBFS RMS).
    fn room_tone() -> Vec<i16> {
        let mut state = 0x2545_f491_u32;
        (0..32_000)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 16) as i16 % 21
            })
            .collect()
    }

    #[test]
    fn a_click_in_room_tone_is_near_silent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capture.wav");
        let mut samples = room_tone();
        // One 3 ms keyboard click at about -12 dBFS.
        for (index, sample) in samples[16_000..16_048].iter_mut().enumerate() {
            *sample = if index % 2 == 0 { 8000 } else { -8000 };
        }
        std::fs::write(&path, wav(&samples)).unwrap();

        assert!(is_near_silent(&path));
    }

    #[test]
    fn a_brief_word_at_speech_level_is_audible() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capture.wav");
        let mut samples = room_tone();
        // 300 ms of a 200 Hz tone at -30 dBFS RMS (peak 1465).
        for (index, sample) in samples[8_000..12_800].iter_mut().enumerate() {
            let phase = index as f64 * 200.0 * std::f64::consts::TAU / 16_000.0;
            *sample = (1465.0 * phase.sin()) as i16;
        }
        std::fs::write(&path, wav(&samples)).unwrap();

        assert!(!is_near_silent(&path));
    }
}
