//! Recorded-call replay for diagnostics: `AGENTDICTATE_COPILOT_REPLAY` names
//! a WAV file that is played into the session as the other side of a call,
//! in real time, instead of capturing live audio.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
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

impl ReplayAudio {
    pub(crate) fn start(path: PathBuf, sink: AudioSink) -> Result<Self, String> {
        let samples = read_speech(&path)?;
        tracing::info!(path = %path.display(), seconds = samples.len() / SAMPLE_RATE as usize, "replaying a recorded call");
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let worker = {
            let stop = Arc::clone(&stop);
            let paused = Arc::clone(&paused);
            std::thread::Builder::new()
                .name("copilot-replay".into())
                .spawn(move || replay(&samples, &sink, &stop, &paused))
                .map_err(|error| error.to_string())?
        };
        Ok(Self {
            stop,
            paused,
            worker: Some(worker),
        })
    }
}

fn replay(samples: &[f32], sink: &AudioSink, stop: &AtomicBool, paused: &AtomicBool) {
    let speaker = Speaker::Them;
    let mut segmenter = Segmenter::new(SegmenterConfig::system_audio());
    let emit = |events: Vec<SegmentEvent>| {
        for event in events {
            match event {
                SegmentEvent::SpeechStarted => sink(AudioMsg::Speaking {
                    speaker,
                    active: true,
                }),
                SegmentEvent::SpeechDiscarded => sink(AudioMsg::Speaking {
                    speaker,
                    active: false,
                }),
                SegmentEvent::Utterance(utterance) => {
                    sink(AudioMsg::Speaking {
                        speaker,
                        active: false,
                    });
                    sink(AudioMsg::Utterance { speaker, utterance });
                }
            }
        }
    };
    // Let the window open before the "call" begins.
    std::thread::sleep(Duration::from_millis(2_500));
    let block = SAMPLE_RATE as usize / 50;
    let started = Instant::now();
    for (index, chunk) in samples.chunks(block).enumerate() {
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
        emit(segmenter.push(chunk));
        if index % 3 == 0 {
            sink(AudioMsg::Level {
                speaker,
                level: segmenter.level(),
            });
        }
    }
    emit(segmenter.push_silence(SAMPLE_RATE as usize));
    sink(AudioMsg::Level {
        speaker,
        level: 0.0,
    });
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
