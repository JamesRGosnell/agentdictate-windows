//! Recorded-call replay for diagnostics: `AGENTDICTATE_COPILOT_REPLAY` names
//! a WAV file that is played into the session as the other side of a call,
//! in real time, instead of capturing live audio.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use agentdictate_core::Speaker;

use crate::copilot::{
    engine::{AudioControl, AudioMsg, AudioSink},
    vad::{Resampler, SAMPLE_RATE, SegmentEvent, Segmenter, SegmenterConfig},
};

pub(crate) const REPLAY_VARIABLE: &str = "AGENTDICTATE_COPILOT_REPLAY";

/// Reads a WAV file as 16 kHz mono samples.
pub(crate) fn read_speech(path: &Path) -> Result<Vec<f32>, String> {
    let mut reader = hound::WavReader::open(path).map_err(|error| error.to_string())?;
    let spec = reader.spec();
    let channels = usize::from(spec.channels.max(1));
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().filter_map(Result::ok).collect(),
        hound::SampleFormat::Int => {
            let scale = (1_i64 << (spec.bits_per_sample.saturating_sub(1))) as f32;
            reader
                .samples::<i32>()
                .filter_map(Result::ok)
                .map(|sample| sample as f32 / scale)
                .collect()
        }
    };
    let mono: Vec<f32> = samples
        .chunks_exact(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect();
    let mut resampler = Resampler::new(spec.sample_rate);
    let mut output = Vec::new();
    resampler.process(&mono, &mut output);
    Ok(output)
}

pub(crate) struct ReplayAudio {
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

/// One side of a replayed call: who is speaking and their audio, with
/// silence where the other side speaks.
pub(crate) struct Lane {
    pub speaker: Speaker,
    pub samples: Vec<f32>,
}

impl ReplayAudio {
    pub(crate) fn start(path: PathBuf, sink: AudioSink) -> Result<Self, String> {
        let samples = read_speech(&path)?;
        tracing::info!(path = %path.display(), seconds = samples.len() / SAMPLE_RATE as usize, "replaying a recorded call");
        Self::start_lanes(
            vec![Lane {
                speaker: Speaker::Them,
                samples,
            }],
            Arc::default(),
            sink,
        )
    }

    /// Plays every lane at once, in real time. `began` is set when the first
    /// sample plays.
    pub(crate) fn start_lanes(
        lanes: Vec<Lane>,
        began: Arc<OnceLock<Instant>>,
        sink: AudioSink,
    ) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let worker = {
            let stop = Arc::clone(&stop);
            let paused = Arc::clone(&paused);
            std::thread::Builder::new()
                .name("copilot-replay".into())
                .spawn(move || replay(&lanes, &began, &sink, &stop, &paused))
                .map_err(|error| error.to_string())?
        };
        Ok(Self {
            stop,
            paused,
            worker: Some(worker),
        })
    }
}

fn replay(
    lanes: &[Lane],
    began: &OnceLock<Instant>,
    sink: &AudioSink,
    stop: &AtomicBool,
    paused: &AtomicBool,
) {
    let mut segmenters: Vec<Segmenter> = lanes
        .iter()
        .map(|lane| {
            Segmenter::new(match lane.speaker {
                Speaker::Me => SegmenterConfig::microphone(),
                Speaker::Them => SegmenterConfig::system_audio(),
            })
        })
        .collect();
    let emit = |speaker: Speaker, events: Vec<SegmentEvent>| {
        for message in events
            .into_iter()
            .flat_map(|event| AudioMsg::from_segment(speaker, event))
        {
            sink(message);
        }
    };
    // Let the window open before the "call" begins.
    std::thread::sleep(Duration::from_millis(2_500));
    let block = SAMPLE_RATE as usize / 50;
    let blocks = lanes
        .iter()
        .map(|lane| lane.samples.len().div_ceil(block))
        .max()
        .unwrap_or(0);
    let started = Instant::now();
    let _ = began.set(started);
    for index in 0..blocks {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let due = started + Duration::from_millis(20 * index as u64);
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        if paused.load(Ordering::Relaxed) {
            continue;
        }
        for (lane, segmenter) in lanes.iter().zip(&mut segmenters) {
            let from = (index * block).min(lane.samples.len());
            let to = (from + block).min(lane.samples.len());
            emit(lane.speaker, segmenter.push(&lane.samples[from..to]));
            if index % 3 == 0 {
                sink(AudioMsg::Level {
                    speaker: lane.speaker,
                    level: segmenter.level(),
                });
            }
        }
    }
    for (lane, segmenter) in lanes.iter().zip(&mut segmenters) {
        emit(lane.speaker, segmenter.push_silence(SAMPLE_RATE as usize));
        sink(AudioMsg::Level {
            speaker: lane.speaker,
            level: 0.0,
        });
    }
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
    }
}

impl AudioControl for ReplayAudio {
    fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    fn stop(mut self: Box<Self>) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
