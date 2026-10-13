//! Microphone capture in the communications category, the way call apps
//! capture. Windows then applies its voice processing to the stream,
//! including acoustic echo cancellation (the device's own or Voice Clarity),
//! which removes the call audio that the microphone hears from the speakers.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use windows::{
    Win32::{
        Foundation::HANDLE,
        Media::Audio::{
            AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMOPTIONS_NONE, AUDIO_EFFECT,
            AUDIO_EFFECT_STATE_ON, AudioCategory_Communications, AudioClientProperties,
            IAcousticEchoCancellationControl, IAudioCaptureClient, IAudioClient2,
            IAudioEffectsManager, IAudioSessionControl, IAudioSessionControl2, IMMDevice,
            IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eCapture,
            eCommunications, eConsole, eRender,
        },
        System::Com::{
            CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
        },
    },
    core::{GUID, HSTRING, Interface},
};

use super::audio::Chunk;

const ECHO_CANCELLATION: GUID = GUID::from_u128(0x6f64adbe_8211_11e2_8c70_2c27d7f001fa);
const NOISE_SUPPRESSION: GUID = GUID::from_u128(0x6f64adbf_8211_11e2_8c70_2c27d7f001fa);
const GAIN_CONTROL: GUID = GUID::from_u128(0x6f64adc0_8211_11e2_8c70_2c27d7f001fa);
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// What Windows reports about the processed microphone stream.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MicProcessing {
    pub device: String,
    pub echo_cancellation: bool,
    pub noise_suppression: bool,
    pub gain_control: bool,
    /// The echo reference was pointed at the device the call plays through.
    pub reference: Option<String>,
}

impl MicProcessing {
    pub(crate) fn describe(&self) -> String {
        let mut effects = Vec::new();
        if self.echo_cancellation {
            effects.push("echo cancellation");
        }
        if self.noise_suppression {
            effects.push("noise suppression");
        }
        if self.gain_control {
            effects.push("gain control");
        }
        let effects = if effects.is_empty() {
            "no Windows voice processing reported".to_owned()
        } else {
            effects.join(", ")
        };
        match &self.reference {
            Some(reference) => format!(
                "\u{201c}{}\u{201d} with {effects}, echo reference \u{201c}{reference}\u{201d}",
                self.device
            ),
            None => format!("\u{201c}{}\u{201d} with {effects}", self.device),
        }
    }
}

pub(crate) struct CommsMicrophone {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for CommsMicrophone {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl CommsMicrophone {
    /// Starts capture; `reference` names the playback device whose audio
    /// should be cancelled (the call device), when known.
    pub(crate) fn start(
        chunks: SyncSender<Chunk>,
        failed: Arc<AtomicBool>,
        reference: Option<String>,
    ) -> Result<(Self, MicProcessing), String> {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready, started) = mpsc::sync_channel(1);
        let worker = {
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("copilot-comms-mic".into())
                .spawn(move || unsafe { run(chunks, failed, reference, stop, ready) })
                .map_err(|error| error.to_string())?
        };
        match started.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(processing)) => Ok((
                Self {
                    stop,
                    worker: Some(worker),
                },
                processing,
            )),
            Ok(Err(error)) => {
                let _ = worker.join();
                Err(error)
            }
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                Err("the communications microphone did not start".into())
            }
        }
    }
}

unsafe fn device_name(device: &IMMDevice) -> String {
    use windows::Win32::{
        Devices::FunctionDiscovery::PKEY_Device_FriendlyName, System::Com::STGM_READ,
    };
    unsafe {
        device
            .OpenPropertyStore(STGM_READ)
            .and_then(|store| store.GetValue(&PKEY_Device_FriendlyName))
            .map(|value| value.to_string())
            .unwrap_or_default()
    }
}

unsafe fn device_id(device: &IMMDevice) -> Option<String> {
    unsafe {
        let id = device.GetId().ok()?;
        let text = id.to_string().ok();
        CoTaskMemFree(Some(id.0.cast()));
        text
    }
}

/// Finds the playback device whose friendly name matches `name`, falling
/// back to the default playback device.
unsafe fn reference_device(
    enumerator: &IMMDeviceEnumerator,
    name: Option<&str>,
) -> Option<(String, String)> {
    use windows::Win32::Media::Audio::DEVICE_STATE_ACTIVE;
    unsafe {
        if let Some(name) = name
            && let Ok(devices) = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
        {
            for index in 0..devices.GetCount().unwrap_or(0) {
                if let Ok(device) = devices.Item(index)
                    && device_name(&device) == name
                {
                    return device_id(&device).map(|id| (id, name.to_owned()));
                }
            }
        }
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
        let name = device_name(&device);
        device_id(&device).map(|id| (id, name))
    }
}

unsafe fn run(
    chunks: SyncSender<Chunk>,
    failed: Arc<AtomicBool>,
    reference: Option<String>,
    stop: Arc<AtomicBool>,
    ready: SyncSender<Result<MicProcessing, String>>,
) {
    let mut ready = Some(ready);
    let result = unsafe { capture(&chunks, &reference, &stop, &mut ready) };
    if let Err(error) = result {
        tracing::warn!(%error, "communications microphone capture stopped");
        match ready.take() {
            Some(ready) => {
                let _ = ready.send(Err(error.to_string()));
            }
            None => failed.store(true, Ordering::Relaxed),
        }
    }
}

unsafe fn capture(
    chunks: &SyncSender<Chunk>,
    reference: &Option<String>,
    stop: &AtomicBool,
    ready: &mut Option<SyncSender<Result<MicProcessing, String>>>,
) -> windows::core::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        // Call apps record from the default communications microphone.
        let device = enumerator.GetDefaultAudioEndpoint(eCapture, eCommunications)?;
        let mut processing = MicProcessing {
            device: device_name(&device),
            ..MicProcessing::default()
        };
        let client: IAudioClient2 = device.Activate(CLSCTX_ALL, None)?;
        let properties = AudioClientProperties {
            cbSize: std::mem::size_of::<AudioClientProperties>() as u32,
            bIsOffload: false.into(),
            eCategory: AudioCategory_Communications,
            Options: AUDCLNT_STREAMOPTIONS_NONE,
        };
        client.SetClientProperties(&properties)?;
        let format = client.GetMixFormat()?;
        let result = (|| {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                2_000_000,
                0,
                format,
                None,
            )?;
            let event = windows_sys::Win32::System::Threading::CreateEventW(
                std::ptr::null(),
                0,
                0,
                std::ptr::null(),
            );
            if event.is_null() {
                return Err(windows::core::Error::from_win32());
            }
            client.SetEventHandle(HANDLE(event))?;
            let capture_client: IAudioCaptureClient = client.GetService()?;
            // Point echo cancellation at the device the call plays through.
            if let Ok(control) = client.GetService::<IAcousticEchoCancellationControl>()
                && let Some((id, name)) = reference_device(&enumerator, reference.as_deref())
            {
                match control.SetEchoCancellationRenderEndpoint(&HSTRING::from(id)) {
                    Ok(()) => processing.reference = Some(name),
                    Err(error) => {
                        tracing::info!(%error, "echo reference could not be changed");
                    }
                }
            }
            if let Ok(effects) = client.GetService::<IAudioEffectsManager>() {
                let mut list: *mut AUDIO_EFFECT = std::ptr::null_mut();
                let mut count = 0_u32;
                if effects.GetAudioEffects(&mut list, &mut count).is_ok() && !list.is_null() {
                    for effect in std::slice::from_raw_parts(list, count as usize) {
                        let on = effect.state == AUDIO_EFFECT_STATE_ON;
                        match effect.id {
                            id if id == ECHO_CANCELLATION => processing.echo_cancellation |= on,
                            id if id == NOISE_SUPPRESSION => processing.noise_suppression |= on,
                            id if id == GAIN_CONTROL => processing.gain_control |= on,
                            _ => {}
                        }
                    }
                    CoTaskMemFree(Some(list.cast()));
                }
            }
            // Recording this stream must not make Windows lower the call.
            if let Ok(session) = client.GetService::<IAudioSessionControl>()
                && let Ok(session) = session.cast::<IAudioSessionControl2>()
            {
                let _ = session.SetDuckingPreference(true);
            }
            let layout = SampleLayout::from_format(format);
            client.Start()?;
            tracing::info!(processing = %processing.describe(), "communications microphone started");
            if let Some(ready) = ready.take() {
                let _ = ready.send(Ok(processing));
            }
            let mut last_data = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                windows_sys::Win32::System::Threading::WaitForSingleObject(event, 200);
                loop {
                    let packet = capture_client.GetNextPacketSize()?;
                    if packet == 0 {
                        break;
                    }
                    let mut data = std::ptr::null_mut();
                    let mut frames = 0_u32;
                    let mut flags = 0_u32;
                    capture_client.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                    let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                    let samples = layout.mono(data, frames as usize, silent);
                    capture_client.ReleaseBuffer(frames)?;
                    last_data = Instant::now();
                    let _ = chunks.try_send(Chunk {
                        samples,
                        rate: layout.rate,
                    });
                }
                if last_data.elapsed() > Duration::from_secs(3) {
                    // A microphone stream never goes quiet; treat it as lost.
                    return Err(windows::core::Error::from_win32());
                }
            }
            let _ = client.Stop();
            windows_sys::Win32::Foundation::CloseHandle(event);
            Ok(())
        })();
        CoTaskMemFree(Some(format.cast()));
        result
    }
}

#[derive(Clone, Copy)]
struct SampleLayout {
    channels: usize,
    rate: u32,
    float: bool,
    bits: u16,
}

impl SampleLayout {
    unsafe fn from_format(format: *const WAVEFORMATEX) -> Self {
        unsafe {
            let base = *format;
            let float = match base.wFormatTag {
                WAVE_FORMAT_IEEE_FLOAT => true,
                WAVE_FORMAT_EXTENSIBLE => {
                    let extensible = *(format as *const WAVEFORMATEXTENSIBLE);
                    let sub_format = extensible.SubFormat;
                    sub_format.data1 == u32::from(WAVE_FORMAT_IEEE_FLOAT)
                }
                _ => false,
            };
            Self {
                channels: usize::from(base.nChannels.max(1)),
                rate: base.nSamplesPerSec,
                float,
                bits: base.wBitsPerSample,
            }
        }
    }

    /// Downmixes one capture packet to mono `f32`.
    unsafe fn mono(&self, data: *const u8, frames: usize, silent: bool) -> Vec<f32> {
        if silent || data.is_null() {
            return vec![0.0; frames];
        }
        let samples = frames * self.channels;
        let read = |index: usize| -> f32 {
            unsafe {
                match (self.float, self.bits) {
                    (true, 32) => *(data as *const f32).add(index),
                    (false, 16) => f32::from(*(data as *const i16).add(index)) / 32_768.0,
                    (false, 32) => *(data as *const i32).add(index) as f32 / 2_147_483_648.0,
                    (false, 24) => {
                        let bytes = data.add(index * 3);
                        let value = i32::from(*bytes)
                            | (i32::from(*bytes.add(1)) << 8)
                            | (i32::from(*bytes.add(2) as i8) << 16);
                        value as f32 / 8_388_608.0
                    }
                    _ => 0.0,
                }
            }
        };
        (0..samples)
            .step_by(self.channels)
            .map(|start| {
                (start..start + self.channels).map(read).sum::<f32>() / self.channels as f32
            })
            .collect()
    }
}
