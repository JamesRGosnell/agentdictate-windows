//! WASAPI capture for the copilot: the default microphone ("Me") and a
//! loopback of the default playback device ("Them").

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use agentdictate_core::Speaker;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::copilot::{
    engine::{AudioControl, AudioMsg, AudioSink},
    vad::{Resampler, SAMPLE_RATE, SegmentEvent, Segmenter, SegmenterConfig},
};

struct Chunk {
    samples: Vec<f32>,
    rate: u32,
}

pub(crate) struct WasapiCapture {
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    controller: Option<JoinHandle<()>>,
}

impl WasapiCapture {
    pub(crate) fn start(
        microphone: bool,
        system_audio: bool,
        sink: AudioSink,
    ) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let (ready, started) = mpsc::sync_channel(1);
        let controller = {
            let stop = Arc::clone(&stop);
            let paused = Arc::clone(&paused);
            std::thread::Builder::new()
                .name("copilot-audio".into())
                .spawn(move || {
                    run_controller(microphone, system_audio, sink, stop, paused, ready);
                })
                .map_err(|error| error.to_string())?
        };
        match started.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => Ok(Self {
                stop,
                paused,
                controller: Some(controller),
            }),
            Ok(Err(error)) => {
                let _ = controller.join();
                Err(error)
            }
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                Err("Audio devices did not respond.".into())
            }
        }
    }
}

impl AudioControl for WasapiCapture {
    fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    fn stop(mut self: Box<Self>) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(controller) = self.controller.take() {
            let _ = controller.join();
        }
    }
}

impl Drop for WasapiCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

struct Source {
    speaker: Speaker,
    stream: Option<cpal::Stream>,
    device_name: String,
    failed: Arc<AtomicBool>,
    chunks: SyncSender<Chunk>,
    last_attempt: Instant,
}

fn device_for(speaker: Speaker) -> Result<cpal::Device, String> {
    let host = cpal::default_host();
    match speaker {
        Speaker::Me => host
            .default_input_device()
            .ok_or_else(|| "No microphone is available.".to_owned()),
        Speaker::Them => host
            .default_output_device()
            .ok_or_else(|| "No playback device is available for system audio.".to_owned()),
    }
}

fn build_stream(
    speaker: Speaker,
    chunks: SyncSender<Chunk>,
    failed: Arc<AtomicBool>,
) -> Result<(cpal::Stream, String), String> {
    let device = device_for(speaker)?;
    let name = device.name().unwrap_or_default();
    let supported = match speaker {
        Speaker::Me => device.default_input_config(),
        Speaker::Them => device.default_output_config(),
    }
    .map_err(|error| error.to_string())?;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let stream = match format {
        cpal::SampleFormat::F32 => typed_stream::<f32>(&device, &config, chunks, failed),
        cpal::SampleFormat::I16 => typed_stream::<i16>(&device, &config, chunks, failed),
        cpal::SampleFormat::U16 => typed_stream::<u16>(&device, &config, chunks, failed),
        cpal::SampleFormat::I32 => typed_stream::<i32>(&device, &config, chunks, failed),
        other => return Err(format!("Unsupported audio format: {other}")),
    }
    .map_err(|error| error.to_string())?;
    stream.play().map_err(|error| error.to_string())?;
    Ok((stream, name))
}

fn typed_stream<T: cpal::SizedSample + cpal::Sample + Send + 'static>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    chunks: SyncSender<Chunk>,
    failed: Arc<AtomicBool>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    f32: cpal::FromSample<T>,
{
    let channels = usize::from(config.channels.max(1));
    let rate = config.sample_rate.0;
    // On WASAPI an input stream on a render device is a loopback capture.
    device.build_input_stream(
        config,
        move |data: &[T], _| {
            let samples = data
                .chunks_exact(channels)
                .map(|frame| {
                    frame
                        .iter()
                        .map(|sample| sample.to_sample::<f32>())
                        .sum::<f32>()
                        / channels as f32
                })
                .collect();
            // Dropping audio under backpressure is preferable to blocking the
            // device callback.
            let _ = chunks.try_send(Chunk { samples, rate });
        },
        move |error| {
            tracing::warn!(%error, "copilot audio stream error");
            failed.store(true, Ordering::Relaxed);
        },
        None,
    )
}

fn run_controller(
    microphone: bool,
    system_audio: bool,
    sink: AudioSink,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    ready: SyncSender<Result<(), String>>,
) {
    let mut sources = Vec::new();
    let mut processors = Vec::new();
    let mut errors = Vec::new();
    for (enabled, speaker) in [(microphone, Speaker::Me), (system_audio, Speaker::Them)] {
        if !enabled {
            continue;
        }
        let (chunks, incoming) = mpsc::sync_channel::<Chunk>(256);
        let failed = Arc::new(AtomicBool::new(false));
        let (stream, device_name) = match build_stream(speaker, chunks.clone(), Arc::clone(&failed))
        {
            Ok((stream, name)) => {
                tracing::info!(?speaker, device = %name, "copilot audio source started");
                (Some(stream), name)
            }
            Err(error) => {
                tracing::warn!(?speaker, %error, "copilot audio source unavailable");
                errors.push(format!("{}: {error}", speaker.display()));
                (None, String::new())
            }
        };
        let processor = {
            let sink = Arc::clone(&sink);
            let stop = Arc::clone(&stop);
            let paused = Arc::clone(&paused);
            std::thread::Builder::new()
                .name(format!("copilot-audio-{}", speaker.label().to_lowercase()))
                .spawn(move || process(speaker, incoming, sink, stop, paused))
        };
        match processor {
            Ok(handle) => processors.push(handle),
            Err(error) => errors.push(error.to_string()),
        }
        sources.push(Source {
            speaker,
            stream,
            device_name,
            failed,
            chunks,
            last_attempt: Instant::now(),
        });
    }
    if sources.iter().all(|source| source.stream.is_none()) {
        let _ = ready.send(Err(if errors.is_empty() {
            "No audio source is enabled.".into()
        } else {
            errors.join("; ")
        }));
        stop.store(true, Ordering::Relaxed);
        drop(sources);
        for processor in processors {
            let _ = processor.join();
        }
        return;
    }
    if !errors.is_empty() {
        sink(AudioMsg::Problem(errors.join("; ")));
    }
    let _ = ready.send(Ok(()));
    let mut last_device_check = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(200));
        let check_devices = last_device_check.elapsed() >= Duration::from_secs(3);
        if check_devices {
            last_device_check = Instant::now();
        }
        for source in &mut sources {
            let default_changed = check_devices
                && device_for(source.speaker)
                    .ok()
                    .and_then(|device| device.name().ok())
                    .is_some_and(|name| !name.is_empty() && name != source.device_name);
            let broken = source.failed.load(Ordering::Relaxed) || source.stream.is_none();
            if !(default_changed || broken)
                || source.last_attempt.elapsed() < Duration::from_secs(2)
            {
                continue;
            }
            source.last_attempt = Instant::now();
            source.stream = None;
            source.failed.store(false, Ordering::Relaxed);
            match build_stream(
                source.speaker,
                source.chunks.clone(),
                Arc::clone(&source.failed),
            ) {
                Ok((stream, name)) => {
                    tracing::info!(speaker = ?source.speaker, device = %name, "copilot audio source restarted");
                    source.stream = Some(stream);
                    source.device_name = name;
                }
                Err(error) => {
                    tracing::debug!(speaker = ?source.speaker, %error, "copilot audio source still unavailable");
                }
            }
        }
    }
    // Dropping the streams closes the chunk channels once the processors
    // have drained them; each processor flushes its final utterance.
    for source in &mut sources {
        source.stream = None;
    }
    drop(sources);
    for processor in processors {
        let _ = processor.join();
    }
}

fn process(
    speaker: Speaker,
    incoming: Receiver<Chunk>,
    sink: AudioSink,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
) {
    let config = match speaker {
        Speaker::Me => SegmenterConfig::microphone(),
        Speaker::Them => SegmenterConfig::system_audio(),
    };
    let mut segmenter = Segmenter::new(config);
    let mut resampler: Option<(u32, Resampler)> = None;
    let mut resampled = Vec::new();
    let mut last_data = Instant::now();
    let mut last_level = Instant::now();
    let mut was_paused = false;
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
    loop {
        let message = incoming.recv_timeout(Duration::from_millis(120));
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let is_paused = paused.load(Ordering::Relaxed);
        if is_paused != was_paused {
            was_paused = is_paused;
            if is_paused {
                if let Some(utterance) = segmenter.flush() {
                    sink(AudioMsg::Speaking {
                        speaker,
                        active: false,
                    });
                    sink(AudioMsg::Utterance { speaker, utterance });
                }
                sink(AudioMsg::Level {
                    speaker,
                    level: 0.0,
                });
            }
        }
        match message {
            Ok(chunk) => {
                last_data = Instant::now();
                if is_paused {
                    continue;
                }
                if resampler
                    .as_ref()
                    .is_none_or(|(rate, _)| *rate != chunk.rate)
                {
                    resampler = Some((chunk.rate, Resampler::new(chunk.rate)));
                }
                resampled.clear();
                if let Some((_, resampler)) = resampler.as_mut() {
                    resampler.process(&chunk.samples, &mut resampled);
                }
                emit(segmenter.push(&resampled));
            }
            Err(RecvTimeoutError::Timeout) => {
                // Loopback goes quiet when nothing plays; treat the gap as
                // silence so an utterance in progress still closes.
                let gap = last_data.elapsed();
                if !is_paused && gap >= Duration::from_millis(100) {
                    let samples =
                        (gap.as_millis() as usize).min(1_000) * SAMPLE_RATE as usize / 1000;
                    emit(segmenter.push_silence(samples));
                    last_data = Instant::now();
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if last_level.elapsed() >= Duration::from_millis(60) {
            last_level = Instant::now();
            sink(AudioMsg::Level {
                speaker,
                level: if is_paused { 0.0 } else { segmenter.level() },
            });
        }
    }
    if let Some(utterance) = segmenter.flush() {
        sink(AudioMsg::Speaking {
            speaker,
            active: false,
        });
        sink(AudioMsg::Utterance { speaker, utterance });
    }
}
