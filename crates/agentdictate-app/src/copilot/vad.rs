//! Energy-based speech segmentation for continuous meeting audio.
//!
//! Each source (microphone, system audio) runs its own segmenter over 16 kHz
//! mono samples. Utterances end after a short pause so questions reach the
//! model quickly, and long monologues are split to bound transcription delay.

use std::collections::VecDeque;

pub(crate) const SAMPLE_RATE: u32 = 16_000;
const BLOCK: usize = 320; // 20 ms
const BLOCK_MS: usize = 20;
/// Silent blocks kept at the end of an utterance.
const TAIL_BLOCKS: usize = 8;

#[derive(Clone, Copy, Debug)]
pub(crate) struct SegmenterConfig {
    /// Absolute RMS below which audio is always silence.
    pub min_threshold: f32,
    /// Speech must exceed the adaptive noise floor by this factor.
    pub floor_ratio: f32,
    /// Consecutive speech blocks required to open an utterance.
    pub start_blocks: usize,
    /// Silence blocks that close an utterance.
    pub end_blocks: usize,
    /// Silence blocks after which the utterance is released provisionally,
    /// so transcription can start before the pause is confirmed; 0 never.
    pub provisional_blocks: usize,
    /// Shorter pause that closes an utterance once it is long.
    pub soft_end_blocks: usize,
    /// Utterance length (blocks) after which the shorter pause applies.
    pub soft_max_blocks: usize,
    /// Hard utterance limit (blocks).
    pub max_blocks: usize,
    /// Audio kept from before speech onset.
    pub pre_roll_blocks: usize,
    /// Utterances with less voiced audio than this are discarded.
    pub min_speech_blocks: usize,
}

impl SegmenterConfig {
    pub(crate) const fn microphone() -> Self {
        Self {
            min_threshold: 0.008,
            floor_ratio: 3.0,
            start_blocks: 3,
            end_blocks: 32,
            provisional_blocks: 0,
            soft_end_blocks: 10,
            soft_max_blocks: 450,
            max_blocks: 900,
            pre_roll_blocks: 15,
            min_speech_blocks: 12,
        }
    }

    /// System audio is clean and digitally silent between speakers, so it can
    /// use a lower threshold and react a little faster. The other side's
    /// questions are answered from it, so its speech is also released
    /// provisionally after 300 ms of silence.
    pub(crate) const fn system_audio() -> Self {
        Self {
            min_threshold: 0.004,
            floor_ratio: 3.0,
            start_blocks: 2,
            end_blocks: 28,
            provisional_blocks: 15,
            soft_end_blocks: 10,
            soft_max_blocks: 450,
            max_blocks: 900,
            pre_roll_blocks: 15,
            min_speech_blocks: 10,
        }
    }
}

/// A finished utterance in 16 kHz mono PCM.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Utterance {
    pub samples: Vec<i16>,
    /// Absolute sample index of the first sample, from the source start.
    pub start_sample: u64,
    pub voiced_blocks: usize,
    pub peak_rms: f32,
    /// Audio kept from before speech began.
    pub lead_ms: u64,
    /// Silence kept after speech ended.
    pub tail_ms: u64,
    /// Silence that passed after `samples` before the utterance was released.
    pub released_after_ms: u64,
}

impl Utterance {
    pub(crate) fn start_ms(&self) -> u64 {
        self.start_sample * 1000 / u64::from(SAMPLE_RATE)
    }

    pub(crate) fn duration_ms(&self) -> u64 {
        self.samples.len() as u64 * 1000 / u64::from(SAMPLE_RATE)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SegmentEvent {
    SpeechStarted,
    /// Speech ended without enough voiced audio to transcribe.
    SpeechDiscarded,
    /// Speech paused long enough that it may have ended: the utterance as it
    /// would end now. If the pause does end it, the [`SegmentEvent::Utterance`]
    /// that follows has the same start and samples.
    Provisional(Utterance),
    Utterance(Utterance),
}

struct Active {
    samples: Vec<f32>,
    start_sample: u64,
    lead_blocks: usize,
    voiced_blocks: usize,
    silent_run: usize,
    blocks: usize,
    peak_rms: f32,
}

pub(crate) struct Segmenter {
    config: SegmenterConfig,
    pending: Vec<f32>,
    pre_roll: VecDeque<Vec<f32>>,
    onset_run: usize,
    noise_floor: f32,
    active: Option<Active>,
    consumed_samples: u64,
    last_rms: f32,
}

impl Segmenter {
    pub(crate) fn new(config: SegmenterConfig) -> Self {
        Self {
            config,
            pending: Vec::with_capacity(BLOCK),
            pre_roll: VecDeque::with_capacity(config.pre_roll_blocks + 1),
            onset_run: 0,
            noise_floor: config.min_threshold / config.floor_ratio,
            active: None,
            consumed_samples: 0,
            last_rms: 0.0,
        }
    }

    /// Current input level in `0.0..=1.0` for meters.
    pub(crate) fn level(&self) -> f32 {
        if self.last_rms <= 0.0 {
            return 0.0;
        }
        ((20.0 * self.last_rms.log10() + 55.0) / 45.0).clamp(0.0, 1.0)
    }

    #[cfg(test)]
    pub(crate) fn is_speaking(&self) -> bool {
        self.active.is_some()
    }

    fn threshold(&self) -> f32 {
        (self.noise_floor * self.config.floor_ratio).max(self.config.min_threshold)
    }

    /// Feeds silence for a stalled source; WASAPI loopback delivers nothing
    /// while the system is quiet, which must still close an utterance.
    pub(crate) fn push_silence(&mut self, samples: usize) -> Vec<SegmentEvent> {
        let mut events = Vec::new();
        let mut remaining = samples;
        let zeros = [0.0_f32; BLOCK];
        while remaining > 0 {
            let take = remaining.min(BLOCK);
            events.extend(self.push(&zeros[..take]));
            remaining -= take;
        }
        events
    }

    pub(crate) fn push(&mut self, samples: &[f32]) -> Vec<SegmentEvent> {
        let mut events = Vec::new();
        for &sample in samples {
            self.pending.push(sample);
            if self.pending.len() == BLOCK {
                let block = std::mem::replace(&mut self.pending, Vec::with_capacity(BLOCK));
                if let Some(event) = self.block(block) {
                    events.push(event);
                }
            }
        }
        events
    }

    fn block(&mut self, block: Vec<f32>) -> Option<SegmentEvent> {
        let rms = (block.iter().map(|sample| sample * sample).sum::<f32>() / BLOCK as f32).sqrt();
        self.last_rms = rms;
        let block_start = self.consumed_samples;
        self.consumed_samples += BLOCK as u64;
        let threshold = self.threshold();
        let voiced = rms > threshold;
        if let Some(active) = self.active.as_mut() {
            active.samples.extend_from_slice(&block);
            active.blocks += 1;
            active.peak_rms = active.peak_rms.max(rms);
            // Hysteresis keeps soft word endings inside the utterance.
            if rms > threshold * 0.6 {
                active.voiced_blocks += usize::from(voiced);
                active.silent_run = 0;
            } else {
                active.silent_run += 1;
            }
            let end_after = if active.blocks >= self.config.soft_max_blocks {
                self.config.soft_end_blocks
            } else {
                self.config.end_blocks
            };
            if active.silent_run >= end_after || active.blocks >= self.config.max_blocks {
                return Some(self.close());
            }
            if self.config.provisional_blocks > 0
                && active.silent_run == self.config.provisional_blocks
                && active.voiced_blocks >= self.config.min_speech_blocks
            {
                return Some(SegmentEvent::Provisional(Self::utterance(active)));
            }
            return None;
        }
        if voiced {
            self.onset_run += 1;
        } else {
            self.onset_run = 0;
            // Track the floor only from quiet audio, adapting downward fast.
            let rate = if rms < self.noise_floor { 0.2 } else { 0.02 };
            self.noise_floor += (rms - self.noise_floor) * rate;
        }
        self.pre_roll.push_back(block);
        if self.pre_roll.len() > self.config.pre_roll_blocks + self.config.start_blocks {
            self.pre_roll.pop_front();
        }
        if self.onset_run >= self.config.start_blocks {
            let blocks: Vec<Vec<f32>> = self.pre_roll.drain(..).collect();
            let start_sample = block_start + BLOCK as u64 - (blocks.len() * BLOCK) as u64;
            let lead_blocks = blocks.len().saturating_sub(self.onset_run);
            let samples: Vec<f32> = blocks.into_iter().flatten().collect();
            self.active = Some(Active {
                blocks: samples.len() / BLOCK,
                samples,
                start_sample,
                lead_blocks,
                voiced_blocks: self.onset_run,
                silent_run: 0,
                peak_rms: rms,
            });
            self.onset_run = 0;
            return Some(SegmentEvent::SpeechStarted);
        }
        None
    }

    fn close(&mut self) -> SegmentEvent {
        let active = self.active.take().expect("an active utterance");
        if active.voiced_blocks < self.config.min_speech_blocks {
            return SegmentEvent::SpeechDiscarded;
        }
        SegmentEvent::Utterance(Self::utterance(&active))
    }

    /// The utterance as it would end now. Silence past the kept tail is
    /// dropped, so the samples stay the same however long the pause runs.
    fn utterance(active: &Active) -> Utterance {
        let dropped_blocks = active.silent_run.saturating_sub(TAIL_BLOCKS);
        let keep = active.samples.len().saturating_sub(dropped_blocks * BLOCK);
        Utterance {
            samples: active.samples[..keep]
                .iter()
                .map(|sample| (sample.clamp(-1.0, 1.0) * 32767.0).round() as i16)
                .collect(),
            start_sample: active.start_sample,
            voiced_blocks: active.voiced_blocks,
            peak_rms: active.peak_rms,
            lead_ms: (active.lead_blocks * BLOCK_MS) as u64,
            tail_ms: (active.silent_run.min(TAIL_BLOCKS) * BLOCK_MS) as u64,
            released_after_ms: (dropped_blocks * BLOCK_MS) as u64,
        }
    }

    /// Ends any utterance in progress (session end or pause).
    pub(crate) fn flush(&mut self) -> Option<Utterance> {
        self.onset_run = 0;
        self.pre_roll.clear();
        self.active.as_ref()?;
        match self.close() {
            SegmentEvent::Utterance(utterance) => Some(utterance),
            _ => None,
        }
    }
}

/// Linear resampler from an arbitrary device rate to 16 kHz mono.
pub(crate) struct Resampler {
    ratio: f64,
    position: f64,
    previous: f32,
}

impl Resampler {
    pub(crate) fn new(input_rate: u32) -> Self {
        Self {
            ratio: f64::from(input_rate) / f64::from(SAMPLE_RATE),
            position: 0.0,
            previous: 0.0,
        }
    }

    pub(crate) fn process(&mut self, input: &[f32], output: &mut Vec<f32>) {
        for &sample in input {
            while self.position <= 1.0 {
                let fraction = self.position as f32;
                output.push(self.previous + (sample - self.previous) * fraction);
                self.position += self.ratio;
            }
            self.position -= 1.0;
            self.previous = sample;
        }
    }
}

/// Encodes 16 kHz mono PCM as a WAV file in memory.
pub(crate) fn wav_bytes(samples: &[i16]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut bytes = Vec::with_capacity(44 + samples.len() * 2);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    bytes.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(milliseconds: usize, amplitude: f32) -> Vec<f32> {
        let count = milliseconds * SAMPLE_RATE as usize / 1000;
        (0..count)
            .map(|index| amplitude * (index as f32 * 0.07).sin())
            .collect()
    }

    fn silence(milliseconds: usize) -> Vec<f32> {
        vec![0.0; milliseconds * SAMPLE_RATE as usize / 1000]
    }

    fn utterances(events: Vec<SegmentEvent>) -> Vec<Utterance> {
        events
            .into_iter()
            .filter_map(|event| match event {
                SegmentEvent::Utterance(utterance) => Some(utterance),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn speech_between_pauses_becomes_one_timed_utterance() {
        let mut segmenter = Segmenter::new(SegmenterConfig::microphone());
        let mut events = segmenter.push(&silence(500));
        events.extend(segmenter.push(&tone(1_200, 0.3)));
        assert!(events.contains(&SegmentEvent::SpeechStarted));
        events.extend(segmenter.push(&silence(1_000)));
        let found = utterances(events);
        assert_eq!(found.len(), 1);
        let utterance = &found[0];
        // Onset includes pre-roll from before the tone started at 500 ms.
        assert!(
            (150..=500).contains(&utterance.start_ms()),
            "{}",
            utterance.start_ms()
        );
        assert!(
            (1_300..=2_000).contains(&utterance.duration_ms()),
            "{}",
            utterance.duration_ms()
        );
        // The speech itself, from 500 to 1,700 ms, can be recovered, and the
        // release came 32 silent blocks (640 ms) after it ended.
        let speech_start = utterance.start_ms() + utterance.lead_ms;
        let samples_end = utterance.start_ms() + utterance.duration_ms();
        let speech_end = samples_end - utterance.tail_ms;
        assert!(speech_start.abs_diff(500) <= 20, "{speech_start}");
        assert!(speech_end.abs_diff(1_700) <= 20, "{speech_end}");
        let released = samples_end + utterance.released_after_ms;
        assert!(released.abs_diff(2_340) <= 20, "{released}");
        assert!(!segmenter.is_speaking());
    }

    fn provisionals(events: &[SegmentEvent]) -> Vec<&Utterance> {
        events
            .iter()
            .filter_map(|event| match event {
                SegmentEvent::Provisional(utterance) => Some(utterance),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_pause_releases_the_utterance_provisionally_before_it_ends() {
        let mut segmenter = Segmenter::new(SegmenterConfig::system_audio());
        let mut events = segmenter.push(&tone(1_200, 0.3));
        events.extend(segmenter.push(&silence(400)));
        let early = provisionals(&events);
        assert_eq!(early.len(), 1);
        assert!(utterances(events.clone()).is_empty(), "not ended yet");
        let early = early[0].clone();
        events.extend(segmenter.push(&silence(600)));
        let found = utterances(events);
        assert_eq!(found.len(), 1);
        // The pause ended it: the provisional copy is the utterance.
        assert_eq!(found[0].start_sample, early.start_sample);
        assert_eq!(found[0].samples, early.samples);
        assert!(found[0].released_after_ms > early.released_after_ms);
    }

    #[test]
    fn speech_resuming_after_a_provisional_release_makes_a_longer_utterance() {
        let mut segmenter = Segmenter::new(SegmenterConfig::system_audio());
        let mut events = segmenter.push(&tone(1_200, 0.3));
        events.extend(segmenter.push(&silence(400)));
        events.extend(segmenter.push(&tone(800, 0.3)));
        events.extend(segmenter.push(&silence(1_000)));
        let early = provisionals(&events);
        assert_eq!(early.len(), 2, "one per pause");
        let early = early[0].clone();
        let found = utterances(events);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].start_sample, early.start_sample);
        assert!(found[0].samples.len() > early.samples.len());
    }

    #[test]
    fn microphone_speech_is_never_released_provisionally() {
        let mut segmenter = Segmenter::new(SegmenterConfig::microphone());
        let mut events = segmenter.push(&tone(1_200, 0.3));
        events.extend(segmenter.push(&silence(1_000)));
        assert!(provisionals(&events).is_empty());
        assert_eq!(utterances(events).len(), 1);
    }

    #[test]
    fn short_clicks_are_discarded() {
        let mut segmenter = Segmenter::new(SegmenterConfig::microphone());
        let mut events = segmenter.push(&tone(100, 0.4));
        events.extend(segmenter.push(&silence(1_000)));
        assert!(events.contains(&SegmentEvent::SpeechDiscarded));
        assert!(utterances(events).is_empty());
    }

    #[test]
    fn long_monologues_split_at_short_pauses() {
        let mut segmenter = Segmenter::new(SegmenterConfig::system_audio());
        let mut events = Vec::new();
        for _ in 0..6 {
            events.extend(segmenter.push(&tone(2_000, 0.3)));
            events.extend(segmenter.push(&silence(250)));
        }
        events.extend(segmenter.push(&silence(1_000)));
        let found = utterances(events);
        assert!(found.len() >= 2, "expected a split, got {}", found.len());
        assert!(
            found
                .iter()
                .all(|utterance| utterance.duration_ms() <= 18_500)
        );
    }

    #[test]
    fn stalled_loopback_silence_closes_speech() {
        let mut segmenter = Segmenter::new(SegmenterConfig::system_audio());
        segmenter.push(&tone(800, 0.3));
        assert!(segmenter.is_speaking());
        let events = segmenter.push_silence(SAMPLE_RATE as usize);
        assert_eq!(utterances(events).len(), 1);
    }

    #[test]
    fn steady_background_noise_raises_the_floor() {
        let mut segmenter = Segmenter::new(SegmenterConfig::microphone());
        let events = segmenter.push(&tone(3_000, 0.006));
        assert!(utterances(events).is_empty());
        assert!(!segmenter.is_speaking());
    }

    #[test]
    fn resampler_converts_48k_to_16k() {
        let mut resampler = Resampler::new(48_000);
        let mut output = Vec::new();
        resampler.process(&vec![0.5; 48_000], &mut output);
        assert!(
            (15_990..=16_010).contains(&output.len()),
            "{}",
            output.len()
        );
    }

    #[test]
    fn wav_header_describes_mono_16k_pcm() {
        let wav = wav_bytes(&[0, 1, -1]);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000);
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 6);
        assert_eq!(wav.len(), 50);
    }
}
