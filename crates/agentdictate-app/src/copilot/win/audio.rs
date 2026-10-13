//! WASAPI capture for the copilot: the default microphone ("Me") and
//! loopbacks of the playback devices where calls are heard ("Them").

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

/// Where a capture stream listens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceKind {
    Microphone,
    /// The default playback device.
    Playback,
    /// The default communications playback device, which call apps such as
    /// Teams use. Captured only while it differs from the default device.
    CallPlayback,
}

impl SourceKind {
    const fn speaker(self) -> Speaker {
        match self {
            Self::Microphone => Speaker::Me,
            Self::Playback | Self::CallPlayback => Speaker::Them,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Microphone => "microphone",
            Self::Playback => "playback",
            Self::CallPlayback => "call playback",
        }
    }
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
    kind: SourceKind,
    stream: Option<cpal::Stream>,
    device_name: String,
    failed: Arc<AtomicBool>,
    chunks: SyncSender<Chunk>,
    last_attempt: Instant,
}

/// Friendly name of the default communications playback device.
fn communications_output_name() -> Option<String> {
    use windows::Win32::{
        Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
        Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator, eCommunications, eRender},
        System::Com::{
            CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, STGM_READ,
        },
    };
    unsafe {
        // Already-initialized apartments report a benign status.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eCommunications)
            .ok()?;
        let store = device.OpenPropertyStore(STGM_READ).ok()?;
        let name = store.GetValue(&PKEY_Device_FriendlyName).ok()?.to_string();
        (!name.is_empty()).then_some(name)
    }
}

/// The call device, when it is not already the default playback device.
fn separate_call_device() -> Option<String> {
    let call = communications_output_name()?;
    let default = cpal::default_host()
        .default_output_device()
        .and_then(|device| device.name().ok());
    (default.as_deref() != Some(call.as_str())).then_some(call)
}

/// The devices a session would capture, for diagnostics.
pub(crate) fn describe_devices() -> String {
    let name = |kind| {
        device_for(kind)
            .ok()
            .and_then(|device| device.name().ok())
            .unwrap_or_else(|| "unavailable".to_owned())
    };
    format!(
        "microphone \u{201c}{}\u{201d}, playback \u{201c}{}\u{201d}, calls {}",
        name(SourceKind::Microphone),
        name(SourceKind::Playback),
        separate_call_device().map_or_else(
            || "use the playback device".to_owned(),
            |call| format!("\u{201c}{call}\u{201d} (also captured)")
        )
    )
}

fn device_for(kind: SourceKind) -> Result<cpal::Device, String> {
    let host = cpal::default_host();
    match kind {
        SourceKind::Microphone => host
            .default_input_device()
            .ok_or_else(|| "No microphone is available.".to_owned()),
        SourceKind::Playback => host
            .default_output_device()
            .ok_or_else(|| "No playback device is available for system audio.".to_owned()),
        SourceKind::CallPlayback => {
            let name = separate_call_device()
                .ok_or_else(|| "Calls play through the default device.".to_owned())?;
            host.output_devices()
                .map_err(|error| error.to_string())?
                .find(|device| device.name().ok().as_deref() == Some(name.as_str()))
                .ok_or_else(|| format!("The call device \u{201c}{name}\u{201d} is unavailable."))
        }
    }
}

fn build_stream(
    kind: SourceKind,
    chunks: SyncSender<Chunk>,
    failed: Arc<AtomicBool>,
) -> Result<(cpal::Stream, String), String> {
    let device = device_for(kind)?;
    let name = device.name().unwrap_or_default();
    let supported = match kind {
        SourceKind::Microphone => device.default_input_config(),
        SourceKind::Playback | SourceKind::CallPlayback => device.default_output_config(),
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
    for (enabled, kind) in [
        (microphone, SourceKind::Microphone),
        (system_audio, SourceKind::Playback),
        (system_audio, SourceKind::CallPlayback),
    ] {
        if !enabled {
            continue;
        }
        let (chunks, incoming) = mpsc::sync_channel::<Chunk>(256);
        let failed = Arc::new(AtomicBool::new(false));
        let wanted = kind != SourceKind::CallPlayback || separate_call_device().is_some();
        let (stream, device_name) = if wanted {
            match build_stream(kind, chunks.clone(), Arc::clone(&failed)) {
                Ok((stream, name)) => {
                    tracing::info!(source = kind.label(), device = %name, "copilot audio source started");
                    (Some(stream), name)
                }
                Err(error) => {
                    tracing::warn!(source = kind.label(), %error, "copilot audio source unavailable");
                    if kind != SourceKind::CallPlayback {
                        errors.push(format!("{}: {error}", kind.speaker().display()));
                    }
                    (None, String::new())
                }
            }
        } else {
            (None, String::new())
        };
        let processor = {
            let sink = Arc::clone(&sink);
            let stop = Arc::clone(&stop);
            let paused = Arc::clone(&paused);
            std::thread::Builder::new()
                .name(format!("copilot-audio-{}", kind.label().replace(' ', "-")))
                .spawn(move || process(kind.speaker(), incoming, sink, stop, paused))
        };
        match processor {
            Ok(handle) => processors.push(handle),
            Err(error) => errors.push(error.to_string()),
        }
        sources.push(Source {
            kind,
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
        if last_device_check.elapsed() < Duration::from_secs(3) {
            // Between device checks, only rebuild streams that failed.
            for source in &mut sources {
                if source.failed.load(Ordering::Relaxed) {
                    restart(source);
                }
            }
            continue;
        }
        last_device_check = Instant::now();
        let call_device = separate_call_device();
        for source in &mut sources {
            let current = match source.kind {
                SourceKind::CallPlayback => call_device.clone(),
                kind => device_for(kind).ok().and_then(|device| device.name().ok()),
            };
            match (current, source.stream.is_some()) {
                // The call device now matches the default; one loopback is enough.
                (None, true) if source.kind == SourceKind::CallPlayback => {
                    tracing::info!("call audio now plays through the default device");
                    source.stream = None;
                    source.device_name.clear();
                }
                (Some(name), true) if name != source.device_name => restart(source),
                (Some(_), false) => restart(source),
                _ if source.failed.load(Ordering::Relaxed) => restart(source),
                _ => {}
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

fn restart(source: &mut Source) {
    if source.last_attempt.elapsed() < Duration::from_secs(2) {
        return;
    }
    source.last_attempt = Instant::now();
    source.stream = None;
    source.failed.store(false, Ordering::Relaxed);
    match build_stream(
        source.kind,
        source.chunks.clone(),
        Arc::clone(&source.failed),
    ) {
        Ok((stream, name)) => {
            tracing::info!(source = source.kind.label(), device = %name, "copilot audio source started");
            source.stream = Some(stream);
            source.device_name = name;
        }
        Err(error) => {
            tracing::debug!(source = source.kind.label(), %error, "copilot audio source still unavailable");
        }
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
    let mut received_audio = false;
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
                received_audio = true;
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
                if !is_paused && received_audio && gap >= Duration::from_millis(100) {
                    let samples =
                        (gap.as_millis() as usize).min(1_000) * SAMPLE_RATE as usize / 1000;
                    emit(segmenter.push_silence(samples));
                    last_data = Instant::now();
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
        // An idle call-device processor stays silent so it never hides the
        // default device's meter.
        if received_audio && last_level.elapsed() >= Duration::from_millis(60) {
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
