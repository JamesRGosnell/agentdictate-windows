//! WASAPI capture for the copilot: the default microphone ("Me") and
//! loopbacks of the playback devices where calls are heard ("Them").

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
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

pub(crate) struct Chunk {
    pub samples: Vec<f32>,
    pub rate: u32,
}

/// A running capture: cpal for loopbacks and the fallback microphone, or the
/// communications-mode microphone with Windows echo cancellation.
#[expect(dead_code, reason = "each capture runs until its handle is dropped")]
enum Capture {
    Cpal(cpal::Stream),
    Comms(super::comms_mic::CommsMicrophone),
}

/// Set to capture the raw microphone without Windows voice processing.
const RAW_MICROPHONE_VARIABLE: &str = "AGENTDICTATE_COPILOT_RAW_MIC";

/// 20 ms of 16 kHz audio: the unit of echo analysis.
const BLOCK: usize = SAMPLE_RATE as usize / 50;

/// Recent speaker output energy by capture time, written by the loopback
/// processors and read by the microphone processor.
#[derive(Default)]
pub(crate) struct EchoReference {
    frames: Mutex<VecDeque<(Instant, f32)>>,
}

impl EchoReference {
    fn push(&self, at: Instant, energy: f32) {
        if let Ok(mut frames) = self.frames.lock() {
            frames.push_back((at, energy));
            while frames.front().is_some_and(|(time, _)| {
                at.saturating_duration_since(*time) > Duration::from_secs(2)
            }) {
                frames.pop_front();
            }
        }
    }

    /// Speaker output that could still be echoing at `at`: full weight while
    /// the playback-to-microphone delay is uncertain, then decaying like room
    /// reverberation.
    fn recent_peak(&self, at: Instant) -> f32 {
        let earliest = at.checked_sub(ECHO_WINDOW).unwrap_or(at);
        let latest = at + Duration::from_millis(20);
        self.frames.lock().map_or(0.0, |frames| {
            frames
                .iter()
                .filter(|(time, _)| *time >= earliest && *time <= latest)
                .map(|(time, energy)| {
                    let age = at.saturating_duration_since(*time).as_secs_f32();
                    let weight = if age <= 0.16 {
                        1.0
                    } else {
                        (-(age - 0.16) / 0.1).exp()
                    };
                    energy * weight
                })
                .fold(0.0, f32::max)
        })
    }
}

/// Speaker-to-microphone delay plus room reverberation covered by the gate.
const ECHO_WINDOW: Duration = Duration::from_millis(400);

/// Removes microphone audio that is only the speakers' sound coming back.
///
/// For each 20 ms block the gate compares microphone energy with the loudest
/// recent speaker output. It learns the speaker-to-microphone coupling as a
/// high percentile of that ratio while the other side talks, and treats a
/// block within a margin of it as echo. Speech louder than the echo, such as
/// the user talking over the call, passes. With Windows echo cancellation
/// active the learned coupling is tiny and the gate rarely acts.
pub(crate) struct EchoGate {
    log_coupling: f32,
    active_blocks: u32,
}

impl Default for EchoGate {
    fn default() -> Self {
        // Start by assuming very loud speakers and learn downward: talk-over
        // protection slows upward learning, so starting low could let echo
        // through until the estimate caught up.
        Self {
            log_coupling: 3.0,
            active_blocks: 0,
        }
    }
}

impl EchoGate {
    /// Reference energy below which the speakers count as silent (-55 dBFS).
    const ACTIVE_REFERENCE: f32 = 3.2e-6;
    /// Percentile of the echo ratio treated as the coupling.
    const PERCENTILE: f32 = 0.85;
    /// Ratios this far (log units, about 4 dB) above the coupling, the gate
    /// margin, are most likely the user speaking and barely move the estimate.
    const DOUBLE_TALK: f32 = 0.9;
    /// Blocks within this factor of the coupling (about 4 dB) are echo.
    const MARGIN: f32 = 2.5;

    /// Returns true when the block is speaker echo rather than the user.
    pub(crate) fn is_echo(&mut self, microphone: f32, reference: f32) -> bool {
        if reference < Self::ACTIVE_REFERENCE {
            return false;
        }
        let log_ratio = (microphone.max(1e-12) / reference).ln();
        let difference = log_ratio - self.log_coupling;
        self.active_blocks = self.active_blocks.saturating_add(1);
        // Learn quickly at first, then settle.
        let step = if self.active_blocks < 200 { 0.25 } else { 0.08 };
        let direction = if difference <= 0.0 {
            Self::PERCENTILE - 1.0
        } else if difference < Self::DOUBLE_TALK {
            Self::PERCENTILE
        } else {
            Self::PERCENTILE * 0.05
        };
        self.log_coupling += step * direction;
        self.log_coupling = self.log_coupling.clamp(-14.0, 4.0);
        microphone <= self.log_coupling.exp() * Self::MARGIN * reference
    }

    pub(crate) fn coupling(&self) -> f32 {
        self.log_coupling.exp()
    }
}

fn energy(block: &[f32]) -> f32 {
    block.iter().map(|sample| sample * sample).sum::<f32>() / block.len().max(1) as f32
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
    stream: Option<Capture>,
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

/// Friendly name of the default communications microphone.
fn communications_input_name() -> Option<String> {
    use windows::Win32::{
        Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
        Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator, eCapture, eCommunications},
        System::Com::{
            CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, STGM_READ,
        },
    };
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eCapture, eCommunications)
            .ok()?;
        let store = device.OpenPropertyStore(STGM_READ).ok()?;
        let name = store.GetValue(&PKEY_Device_FriendlyName).ok()?.to_string();
        (!name.is_empty()).then_some(name)
    }
}

fn raw_microphone() -> bool {
    std::env::var_os(RAW_MICROPHONE_VARIABLE).is_some()
}

/// The device each source would capture now.
fn current_device_name(kind: SourceKind) -> Option<String> {
    match kind {
        SourceKind::Microphone if !raw_microphone() => communications_input_name()
            .or_else(|| device_for(kind).ok().and_then(|device| device.name().ok())),
        SourceKind::CallPlayback => separate_call_device(),
        kind => device_for(kind).ok().and_then(|device| device.name().ok()),
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

/// Starts a capture and reports the device and, for the microphone, the
/// Windows voice processing applied to it.
fn build_stream(
    kind: SourceKind,
    chunks: SyncSender<Chunk>,
    failed: Arc<AtomicBool>,
) -> Result<(Capture, String, Option<String>), String> {
    if kind == SourceKind::Microphone && !raw_microphone() {
        match super::comms_mic::CommsMicrophone::start(
            chunks.clone(),
            Arc::clone(&failed),
            separate_call_device(),
        ) {
            Ok((microphone, processing)) => {
                let name = processing.device.clone();
                return Ok((
                    Capture::Comms(microphone),
                    name,
                    Some(processing.describe()),
                ));
            }
            Err(error) => {
                tracing::warn!(%error, "communications microphone unavailable; using the raw microphone");
            }
        }
    }
    let (stream, name) = build_cpal_stream(kind, chunks, failed)?;
    Ok((Capture::Cpal(stream), name, None))
}

fn build_cpal_stream(
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
    let echo = Arc::new(EchoReference::default());
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
                Ok((stream, name, processing)) => {
                    tracing::info!(source = kind.label(), device = %name, "copilot audio source started");
                    if let Some(processing) = processing {
                        sink(AudioMsg::Status(format!("Microphone {processing}")));
                    }
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
            let echo = Arc::clone(&echo);
            std::thread::Builder::new()
                .name(format!("copilot-audio-{}", kind.label().replace(' ', "-")))
                .spawn(move || process(kind.speaker(), incoming, sink, stop, paused, echo))
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
                    restart(source, &sink);
                }
            }
            continue;
        }
        last_device_check = Instant::now();
        for source in &mut sources {
            let current = current_device_name(source.kind);
            match (current, source.stream.is_some()) {
                // The call device now matches the default; one loopback is enough.
                (None, true) if source.kind == SourceKind::CallPlayback => {
                    tracing::info!("call audio now plays through the default device");
                    source.stream = None;
                    source.device_name.clear();
                }
                (Some(name), true) if name != source.device_name => restart(source, &sink),
                (Some(_), false) => restart(source, &sink),
                _ if source.failed.load(Ordering::Relaxed) => restart(source, &sink),
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

fn restart(source: &mut Source, sink: &AudioSink) {
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
        Ok((stream, name, processing)) => {
            tracing::info!(source = source.kind.label(), device = %name, "copilot audio source started");
            if let Some(processing) = processing {
                sink(AudioMsg::Status(format!("Microphone {processing}")));
            }
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
    echo: Arc<EchoReference>,
) {
    let config = match speaker {
        Speaker::Me => SegmenterConfig::microphone(),
        Speaker::Them => SegmenterConfig::system_audio(),
    };
    let mut segmenter = Segmenter::new(config);
    let mut gate = EchoGate::default();
    let mut resampler: Option<(u32, Resampler)> = None;
    let mut pending: Vec<f32> = Vec::with_capacity(BLOCK * 4);
    let mut last_data = Instant::now();
    let mut last_level = Instant::now();
    let mut last_report = Instant::now();
    let mut gated_blocks = 0_u32;
    let mut was_paused = false;
    let emit = |events: Vec<SegmentEvent>| {
        for message in events
            .into_iter()
            .flat_map(|event| AudioMsg::from_segment(speaker, event))
        {
            sink(message);
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
                    emit(vec![SegmentEvent::Utterance(utterance)]);
                }
                sink(AudioMsg::Level {
                    speaker,
                    level: 0.0,
                });
            }
        }
        match message {
            Ok(chunk) => {
                let arrived = Instant::now();
                last_data = arrived;
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
                if let Some((_, resampler)) = resampler.as_mut() {
                    resampler.process(&chunk.samples, &mut pending);
                }
                let blocks = pending.len() / BLOCK;
                for index in 0..blocks {
                    let block = &mut pending[index * BLOCK..(index + 1) * BLOCK];
                    // Time the block by how much buffered audio follows it.
                    let after = (blocks - index - 1) * BLOCK;
                    let at = arrived
                        .checked_sub(Duration::from_micros(
                            after as u64 * 1_000_000 / u64::from(SAMPLE_RATE),
                        ))
                        .unwrap_or(arrived);
                    let level = energy(block);
                    match speaker {
                        Speaker::Them => echo.push(at, level),
                        Speaker::Me => {
                            if gate.is_echo(level, echo.recent_peak(at)) {
                                for sample in block.iter_mut() {
                                    *sample *= 0.03;
                                }
                                gated_blocks += 1;
                            }
                        }
                    }
                }
                let used = blocks * BLOCK;
                emit(segmenter.push(&pending[..used]));
                pending.drain(..used);
                if speaker == Speaker::Me && last_report.elapsed() >= Duration::from_secs(30) {
                    if gated_blocks > 0 {
                        tracing::info!(
                            gated_ms = gated_blocks * 20,
                            coupling = gate.coupling(),
                            "removed speaker echo from the microphone"
                        );
                    }
                    gated_blocks = 0;
                    last_report = Instant::now();
                }
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
        emit(vec![SegmentEvent::Utterance(utterance)]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic speech-like bursts: noise shaped by a syllable envelope.
    fn speech(seconds: f32, amplitude: f32, seed: u32) -> Vec<f32> {
        let mut state = seed;
        (0..(seconds * SAMPLE_RATE as f32) as usize)
            .map(|index| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (state >> 8) as f32 / (1 << 24) as f32 * 2.0 - 1.0;
                let envelope = (0.5 + 0.5 * (index as f32 / 1_600.0).sin()).powi(2);
                noise * envelope * amplitude
            })
            .collect()
    }

    /// Microphone = delayed, attenuated, reverberant speaker output (+ near end).
    fn echo_of(reference: &[f32], delay: usize, gain: f32) -> Vec<f32> {
        let mut output = vec![0.0; reference.len()];
        for (index, sample) in reference.iter().enumerate() {
            for (tap, weight) in [(0, 1.0), (160, 0.5), (800, 0.25)] {
                let at = index + delay + tap;
                if at < output.len() {
                    output[at] += sample * gain * weight;
                }
            }
        }
        output
    }

    /// Runs the gate over aligned reference and microphone signals and
    /// returns, per block, whether it was treated as echo.
    fn gate_blocks(reference: &[f32], microphone: &[f32]) -> Vec<bool> {
        let echo = EchoReference::default();
        let mut gate = EchoGate::default();
        let start = Instant::now();
        reference
            .chunks(BLOCK)
            .zip(microphone.chunks(BLOCK))
            .enumerate()
            .map(|(index, (reference, microphone))| {
                let at = start + Duration::from_millis(20 * index as u64);
                echo.push(at, energy(reference));
                gate.is_echo(energy(microphone), echo.recent_peak(at))
            })
            .collect()
    }

    fn share(flags: &[bool]) -> f32 {
        flags.iter().filter(|flag| **flag).count() as f32 / flags.len().max(1) as f32
    }

    #[test]
    fn loud_speaker_echo_is_removed_once_the_coupling_is_learned() {
        let reference = speech(8.0, 0.3, 7);
        let microphone = echo_of(&reference, 960, 0.6);
        let flags = gate_blocks(&reference, &microphone);
        // After the first two seconds every echo block is gated.
        let late = &flags[100..];
        let active: Vec<bool> = late
            .iter()
            .zip(reference.chunks(BLOCK).skip(100))
            .filter(|(_, block)| energy(block) > EchoGate::ACTIVE_REFERENCE)
            .map(|(flag, _)| *flag)
            .collect();
        assert!(share(&active) > 0.97, "gated {:.2}", share(&active));
    }

    #[test]
    fn the_user_talking_over_the_call_is_kept() {
        let reference = speech(8.0, 0.2, 11);
        let mut microphone = echo_of(&reference, 960, 0.15);
        // The user speaks over the call for the last three seconds.
        let user = speech(3.0, 0.5, 23);
        let offset = microphone.len() - user.len();
        for (index, sample) in user.iter().enumerate() {
            microphone[offset + index] += sample;
        }
        let flags = gate_blocks(&reference, &microphone);
        // Blocks where the user is clearly louder than the echo must pass.
        let echo = echo_of(&reference, 960, 0.15);
        let dominant: Vec<bool> = flags
            .iter()
            .enumerate()
            .skip(flags.len() - 150)
            .filter(|(index, _)| {
                let range = index * BLOCK..(index + 1) * BLOCK;
                energy(&microphone[range.clone()]) > 10.0 * energy(&echo[range])
            })
            .map(|(_, flag)| *flag)
            .collect();
        assert!(dominant.len() > 50);
        assert!(
            share(&dominant) < 0.1,
            "gated {:.2} of the user",
            share(&dominant)
        );
    }

    #[test]
    fn the_user_alone_is_never_gated() {
        let reference = vec![0.0; 3 * SAMPLE_RATE as usize];
        let microphone = speech(3.0, 0.3, 5);
        assert!(
            gate_blocks(&reference, &microphone)
                .iter()
                .all(|flag| !flag)
        );
    }

    #[test]
    fn echo_tails_after_the_other_side_stops_are_removed() {
        let mut reference = speech(4.0, 0.3, 3);
        reference.extend(vec![0.0; SAMPLE_RATE as usize]);
        let microphone = echo_of(&reference, 960, 0.5);
        let flags = gate_blocks(&reference, &microphone);
        // Blocks within 300 ms after the reference stops still carry echo.
        let tail_start = 4 * 50;
        let tail: Vec<bool> = flags[tail_start..tail_start + 15]
            .iter()
            .zip(microphone.chunks(BLOCK).skip(tail_start))
            .filter(|(_, block)| energy(block) > 1e-5)
            .map(|(flag, _)| *flag)
            .collect();
        assert!(share(&tail) > 0.9, "gated {:.2} of the tail", share(&tail));
    }
}
