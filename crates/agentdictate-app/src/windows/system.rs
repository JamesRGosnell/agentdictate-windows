//! Native Windows recording and one-shot clipboard delivery.
use crate::{CapturedRecording, RecordingController};
use agentdictate_core::{ClientCommand, JobId, Settings};
use agentdictate_runtime::{
    Deliverer, DeliveryDisposition, ExternalError, IpcClient, Recorder, RecordingJob,
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::{
    fs::File,
    io::BufWriter,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    System::{DataExchange::*, Memory::*, Threading::*},
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

type Reply<T> = mpsc::SyncSender<Result<T, ExternalError>>;
enum AudioCommand {
    Start(JobId, PathBuf, Reply<()>),
    Finish(JobId, Reply<CapturedRecording>),
}
#[path = "ducking.rs"]
mod ducking;
pub struct SystemRecordingController {
    commands: mpsc::Sender<AudioCommand>,
    settings: Settings,
    ducker: ducking::PlaybackDucker,
}
struct Capture {
    stream: cpal::Stream,
    state: Arc<Mutex<AudioWriter>>,
    id: JobId,
}
struct AudioWriter {
    writer: Option<hound::WavWriter<BufWriter<File>>>,
    samples: u64,
    rate: u32,
    source_index: u64,
    next_output: f64,
    previous: f32,
    ready: Option<Reply<()>>,
    failure: Option<String>,
}
impl AudioWriter {
    fn frame(&mut self, sample: f32) -> Result<(), hound::Error> {
        let index = self.source_index as f64;
        while self.next_output <= index {
            let fraction = (self.next_output - (index - 1.)).clamp(0., 1.) as f32;
            let sample = self.previous + (sample - self.previous) * fraction;
            self.writer
                .as_mut()
                .expect("active WAV writer")
                .write_sample((sample.clamp(-1., 1.) * 32767.).round() as i16)?;
            self.samples += 1;
            self.next_output += self.rate as f64 / 16000.;
        }
        self.previous = sample;
        self.source_index += 1;
        Ok(())
    }
    fn flush(&mut self) -> Result<(), hound::Error> {
        self.writer.as_mut().expect("active WAV writer").flush()?;
        if self.samples > 0
            && let Some(ready) = self.ready.take()
        {
            let _ = ready.send(Ok(()));
        }
        Ok(())
    }
}
fn audio_error(runtime: &Path, id: JobId, message: &str) {
    tracing::error!(%id,%message,"Windows audio capture failed");
    if let Ok((mut client, _)) = IpcClient::connect(runtime) {
        let _ = client.send(ClientCommand::recorder_exited(1_000_001, id));
    }
}
fn build_stream<T: cpal::SizedSample + cpal::Sample + Send + 'static>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    state: Arc<Mutex<AudioWriter>>,
    runtime: PathBuf,
    id: JobId,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    f32: cpal::FromSample<T>,
{
    let channels = config.channels as usize;
    let error_state = state.clone();
    let data_runtime = runtime.clone();
    device.build_input_stream(
        config,
        move |data: &[T], _| {
            if let Ok(mut writer) = state.lock() {
                if writer.failure.is_some() {
                    return;
                }
                let result = (|| {
                    for frame in data.chunks_exact(channels) {
                        let mono = frame
                            .iter()
                            .map(|sample| sample.to_sample::<f32>())
                            .sum::<f32>()
                            / channels as f32;
                        writer.frame(mono)?;
                    }
                    writer.flush()
                })();
                if let Err(error) = result {
                    let message = error.to_string();
                    writer.failure = Some(message.clone());
                    if let Some(ready) = writer.ready.take() {
                        let _ = ready.send(Err(ExternalError::new(message.clone())));
                    }
                    let runtime = data_runtime.clone();
                    std::thread::spawn(move || audio_error(&runtime, id, &message));
                }
            }
        },
        move |error| {
            if let Ok(mut writer) = error_state.lock() {
                if writer.failure.is_some() {
                    return;
                }
                writer.failure = Some(error.to_string());
                if let Some(ready) = writer.ready.take() {
                    let _ = ready.send(Err(ExternalError::new(error.to_string())));
                }
            }
            let runtime = runtime.clone();
            let message = error.to_string();
            std::thread::spawn(move || audio_error(&runtime, id, &message));
        },
        None,
    )
}
fn start_capture(
    id: JobId,
    path: PathBuf,
    ready: Reply<()>,
    runtime: PathBuf,
) -> Result<Capture, ExternalError> {
    let device = cpal::default_host().default_input_device().ok_or_else(|| {
        ExternalError::new("No default microphone is available. Check Windows microphone settings.")
    })?;
    let supported = device
        .default_input_config()
        .map_err(|e| ExternalError::new(e.to_string()))?;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ExternalError::new(e.to_string()))?;
    }
    let writer = hound::WavWriter::create(
        &path,
        hound::WavSpec {
            channels: 1,
            sample_rate: 16000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        },
    )
    .map_err(|e| ExternalError::new(e.to_string()))?;
    let state = Arc::new(Mutex::new(AudioWriter {
        writer: Some(writer),
        samples: 0,
        rate: config.sample_rate.0,
        source_index: 0,
        next_output: 0.,
        previous: 0.,
        ready: Some(ready),
        failure: None,
    }));
    let stream = match format {
        cpal::SampleFormat::F32 => {
            build_stream::<f32>(&device, &config, state.clone(), runtime, id)
        }
        cpal::SampleFormat::I16 => {
            build_stream::<i16>(&device, &config, state.clone(), runtime, id)
        }
        cpal::SampleFormat::U16 => {
            build_stream::<u16>(&device, &config, state.clone(), runtime, id)
        }
        cpal::SampleFormat::I32 => {
            build_stream::<i32>(&device, &config, state.clone(), runtime, id)
        }
        _ => {
            return Err(ExternalError::new(format!(
                "Unsupported microphone sample format: {format}"
            )));
        }
    }
    .map_err(|e| ExternalError::new(e.to_string()))?;
    stream
        .play()
        .map_err(|e| ExternalError::new(e.to_string()))?;
    Ok(Capture { stream, state, id })
}
fn finish_capture(capture: Capture) -> Result<CapturedRecording, ExternalError> {
    drop(capture.stream);
    let mut state = capture
        .state
        .lock()
        .map_err(|_| ExternalError::new("audio writer lock poisoned"))?;
    if let Some(writer) = state.writer.take() {
        writer
            .finalize()
            .map_err(|e| ExternalError::new(e.to_string()))?;
    }
    if let Some(failure) = &state.failure {
        return Err(ExternalError::new(failure.clone()));
    }
    if state.samples == 0 {
        return Err(ExternalError::new(
            "The microphone did not capture any audio.",
        ));
    }
    Ok(CapturedRecording {
        duration_seconds: state.samples as f64 / 16000.,
    })
}
impl SystemRecordingController {
    pub fn for_system(settings: &Settings, runtime: &Path) -> Self {
        let (commands, incoming) = mpsc::channel();
        let runtime = runtime.to_owned();
        std::thread::Builder::new()
            .name("agentdictate-wasapi".into())
            .spawn(move || {
                let mut active: Option<Capture> = None;
                while let Ok(command) = incoming.recv() {
                    match command {
                        AudioCommand::Start(id, path, reply) => {
                            if active.is_some() {
                                let _ =
                                    reply.send(Err(ExternalError::new("recording already active")));
                                continue;
                            }
                            match start_capture(id, path, reply.clone(), runtime.clone()) {
                                Ok(capture) => active = Some(capture),
                                Err(error) => {
                                    let _ = reply.send(Err(error));
                                }
                            }
                        }
                        AudioCommand::Finish(id, reply) => {
                            let result = if active.as_ref().is_some_and(|capture| capture.id == id)
                            {
                                finish_capture(active.take().unwrap())
                            } else {
                                Err(ExternalError::new("recording is not active"))
                            };
                            let _ = reply.send(result);
                        }
                    }
                }
                if let Some(capture) = active {
                    let _ = finish_capture(capture);
                }
            })
            .expect("Windows audio owner thread should start");
        Self {
            commands,
            settings: settings.clone(),
            ducker: ducking::PlaybackDucker::new(),
        }
    }
    pub fn update_settings(&mut self, settings: &Settings) {
        self.settings = settings.clone();
        if !settings.audio_ducking_enabled {
            self.ducker.restore();
        }
    }
}
impl Recorder for SystemRecordingController {
    fn start(&mut self, job: &RecordingJob) -> Result<(), ExternalError> {
        self.ducker.duck(&self.settings);
        let (reply, result) = mpsc::sync_channel(1);
        self.commands
            .send(AudioCommand::Start(job.id, job.audio_path.clone(), reply))
            .map_err(|e| ExternalError::new(e.to_string()))?;
        match result.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => Ok(()),
            result => {
                let error = match result {
                    Ok(Err(e)) => e,
                    Err(e) => ExternalError::new(e.to_string()),
                    _ => unreachable!(),
                };
                let _ = self.finish(job);
                Err(error)
            }
        }
    }
    fn abort_start(&mut self, job: &RecordingJob) -> Result<(), ExternalError> {
        self.finish(job).map(|_| ())
    }
}
impl RecordingController for SystemRecordingController {
    fn finish(&mut self, job: &RecordingJob) -> Result<CapturedRecording, ExternalError> {
        self.ducker.restore();
        let (reply, result) = mpsc::sync_channel(1);
        self.commands
            .send(AudioCommand::Finish(job.id, reply))
            .map_err(|e| ExternalError::new(e.to_string()))?;
        result
            .recv_timeout(Duration::from_secs(10))
            .map_err(|e| ExternalError::new(e.to_string()))?
    }
}

pub struct SystemDeliverer {
    shortcut: String,
}

/// Runs in the Settings action worker before a recovery request reaches the daemon.
pub(crate) fn prepare_recovery_delivery() -> Result<(), ExternalError> {
    // SAFETY: only query the foreground HWND and its owning process. Minimize only
    // this UI process's window, never another application's destination window.
    let window = unsafe { GetForegroundWindow() };
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(window, &mut pid) };
    if window.is_null() || pid != std::process::id() {
        return Ok(());
    }
    if unsafe { ShowWindowAsync(window, SW_MINIMIZE) } == 0 {
        return Err(ExternalError::new(
            "Could not minimize AgentDictate. Use Copy and paste manually.",
        ));
    }
    wait_for_recovery_focus(
        || {
            let foreground = unsafe { GetForegroundWindow() };
            !foreground.is_null() && foreground != window
        },
        || std::thread::sleep(Duration::from_millis(10)),
    )
}

fn wait_for_recovery_focus(
    mut destination_ready: impl FnMut() -> bool,
    mut wait: impl FnMut(),
) -> Result<(), ExternalError> {
    // ShowWindowAsync posts to the UI thread; do not race it with the IPC request.
    for _ in 0..200 {
        if destination_ready() {
            return Ok(());
        }
        wait();
    }
    Err(ExternalError::new(
        "Focus the destination application, or use Copy and paste manually.",
    ))
}
struct ClipboardGuard(HWND);
impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            CloseClipboard();
            DestroyWindow(self.0);
        }
    }
}
fn open_clipboard() -> Result<ClipboardGuard, ExternalError> {
    let class: Vec<u16> = "STATIC".encode_utf16().chain(Some(0)).collect();
    let owner = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            std::ptr::null(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
        )
    };
    if owner.is_null() {
        return Err(ExternalError::new(
            "Could not create clipboard owner window",
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if unsafe { OpenClipboard(owner) } != 0 {
            return Ok(ClipboardGuard(owner));
        }
        if Instant::now() >= deadline {
            unsafe {
                DestroyWindow(owner);
            }
            return Err(ExternalError::new("Windows clipboard is busy"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
impl SystemDeliverer {
    pub fn for_environment(shortcut: &str) -> Self {
        Self {
            shortcut: shortcut.into(),
        }
    }
    pub fn update_shortcut(&mut self, shortcut: &str) {
        self.shortcut = shortcut.into();
    }
    pub fn copy_text(&mut self, text: &str) -> Result<(), ExternalError> {
        let text: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        let _guard = open_clipboard()?;
        // SAFETY: Windows receives ownership only after successful SetClipboardData.
        unsafe {
            let allocation = GlobalAlloc(GMEM_MOVEABLE, text.len() * 2);
            if allocation.is_null() {
                return Err(ExternalError::new("could not allocate clipboard memory"));
            }
            let destination = GlobalLock(allocation);
            if destination.is_null() {
                GlobalFree(allocation);
                return Err(ExternalError::new("could not lock clipboard memory"));
            }
            std::ptr::copy_nonoverlapping(text.as_ptr(), destination.cast(), text.len());
            GlobalUnlock(allocation);
            if EmptyClipboard() == 0 || SetClipboardData(13, allocation).is_null() {
                GlobalFree(allocation);
                return Err(ExternalError::new(format!(
                    "clipboard publication failed: {}",
                    std::io::Error::last_os_error()
                )));
            }
            let handle = GetClipboardData(13);
            if handle.is_null() {
                return Err(ExternalError::new("clipboard readback failed"));
            }
            let contents = GlobalLock(handle);
            if contents.is_null() {
                return Err(ExternalError::new("clipboard readback lock failed"));
            }
            let matches = GlobalSize(handle) >= text.len() * 2
                && std::slice::from_raw_parts(contents.cast::<u16>(), text.len())
                    == text.as_slice();
            GlobalUnlock(handle);
            if !matches {
                return Err(ExternalError::new(
                    "clipboard readback differs from transcript",
                ));
            }
        }
        Ok(())
    }
}
fn paste_keys(shortcut: &str, class: &str) -> Vec<u16> {
    let shortcut = shortcut.to_ascii_lowercase();
    if shortcut.starts_with("terminal") || shortcut == "ctrl+shift+v" {
        return vec![VK_CONTROL, VK_SHIFT, 0x56];
    }
    if shortcut == "shift+insert"
        || (shortcut == "automatic" && class.eq_ignore_ascii_case("putty"))
    {
        return vec![VK_SHIFT, VK_INSERT];
    }
    if shortcut == "automatic" && class.eq_ignore_ascii_case("CASCADIA_HOSTING_WINDOW_CLASS") {
        return vec![VK_CONTROL, VK_SHIFT, 0x56];
    }
    vec![VK_CONTROL, 0x56]
}
fn key(vk: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
                time: 0,
                dwExtraInfo: agentdictate_linux::native_hotkey::SELF_INJECTION,
            },
        },
    }
}
impl Deliverer for SystemDeliverer {
    fn deliver(&mut self, job: &RecordingJob) -> Result<DeliveryDisposition, ExternalError> {
        let deadline = Instant::now() + Duration::from_secs(5);
        // Wait for physical modifiers to be released; never release the user's held keys.
        while [VK_CONTROL, VK_SHIFT, VK_MENU, VK_LWIN, VK_RWIN]
            .iter()
            .any(|vk| unsafe { GetAsyncKeyState(*vk as i32) } < 0)
        {
            if Instant::now() >= deadline {
                return Err(ExternalError::new(
                    "Release shortcut modifiers before pasting; the transcript remains in Recovery",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let target = unsafe { GetForegroundWindow() };
        if target.is_null() {
            return Err(ExternalError::new("No focused destination window"));
        }
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(target, &mut pid);
        }
        let mut destination_is_ours = pid == std::process::id();
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if !process.is_null() {
                let mut name = vec![0u16; 32768];
                let mut len = name.len() as u32;
                if QueryFullProcessImageNameW(process, 0, name.as_mut_ptr(), &mut len) != 0 {
                    let path = PathBuf::from(String::from_utf16_lossy(&name[..len as usize]));
                    destination_is_ours |= path.file_name().is_some_and(|name| {
                        name.eq_ignore_ascii_case("agentdictate.exe")
                            || name.eq_ignore_ascii_case("agentdictated.exe")
                    });
                }
                CloseHandle(process);
            }
        }
        if destination_is_ours {
            return Err(ExternalError::new(
                "Focus the destination application before stopping dictation",
            ));
        }
        self.copy_text(&job.final_text)?;
        if unsafe { GetForegroundWindow() } != target
            || [VK_CONTROL, VK_SHIFT, VK_MENU, VK_LWIN, VK_RWIN]
                .iter()
                .any(|vk| unsafe { GetAsyncKeyState(*vk as i32) } < 0)
        {
            return Ok(DeliveryDisposition::Ambiguous {
                copied_to_clipboard: true,
            });
        }
        let mut class = [0u16; 256];
        let size = unsafe { GetClassNameW(target, class.as_mut_ptr(), class.len() as i32) };
        let class = String::from_utf16_lossy(&class[..size.max(0) as usize]);
        let keys = paste_keys(&self.shortcut, &class);
        let inputs: Vec<INPUT> = keys
            .iter()
            .map(|vk| key(*vk, false))
            .chain(keys.iter().rev().map(|vk| key(*vk, true)))
            .collect();
        let count = unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            )
        };
        if count != inputs.len() as u32 {
            // Balance our own partial injection, without retrying the paste itself.
            if count > 0 {
                let release: Vec<INPUT> = keys.iter().rev().map(|vk| key(*vk, true)).collect();
                unsafe {
                    SendInput(
                        release.len() as u32,
                        release.as_ptr(),
                        std::mem::size_of::<INPUT>() as i32,
                    );
                }
            }
            return Ok(DeliveryDisposition::Ambiguous {
                copied_to_clipboard: true,
            });
        }
        Ok(DeliveryDisposition::Submitted {
            copied_to_clipboard: true,
            paste_triggered: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recovery_waits_for_focus_handoff_and_bounds_a_failed_minimize() {
        let mut observations = 0;
        let mut waits = 0;
        wait_for_recovery_focus(
            || {
                observations += 1;
                observations == 4
            },
            || waits += 1,
        )
        .unwrap();
        assert_eq!(waits, 3);
        waits = 0;
        assert!(wait_for_recovery_focus(|| false, || waits += 1).is_err());
        assert_eq!(waits, 200);
        wait_for_recovery_focus(
            || true,
            || panic!("already focused destination must not wait"),
        )
        .unwrap();
    }
    #[test]
    fn terminal_paste_uses_windows_application_conventions() {
        assert_eq!(
            paste_keys("Terminal (Ctrl+Shift+V)", "EDIT"),
            vec![VK_CONTROL, VK_SHIFT, 0x56]
        );
        assert_eq!(paste_keys("Automatic", "PuTTY"), vec![VK_SHIFT, VK_INSERT]);
        assert_eq!(
            paste_keys("Automatic", "CASCADIA_HOSTING_WINDOW_CLASS"),
            vec![VK_CONTROL, VK_SHIFT, 0x56]
        );
        assert_eq!(paste_keys("Standard", "PuTTY"), vec![VK_CONTROL, 0x56]);
    }
    #[test]
    fn resampling_emits_exactly_one_second_of_pcm_and_a_readable_wav() {
        for rate in [16000, 44100, 48000] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("resampled.wav");
            let writer = hound::WavWriter::create(
                &path,
                hound::WavSpec {
                    channels: 1,
                    sample_rate: 16000,
                    bits_per_sample: 16,
                    sample_format: hound::SampleFormat::Int,
                },
            )
            .unwrap();
            let mut state = AudioWriter {
                writer: Some(writer),
                samples: 0,
                rate,
                source_index: 0,
                next_output: 0.,
                previous: 0.,
                ready: None,
                failure: None,
            };
            for i in 0..rate {
                state
                    .frame((i as f32 * 440. * std::f32::consts::TAU / rate as f32).sin() * 0.5)
                    .unwrap();
            }
            state.flush().unwrap();
            assert_eq!(state.samples, 16000);
            state.writer.take().unwrap().finalize().unwrap();
            let reader = hound::WavReader::open(path).unwrap();
            assert_eq!(reader.duration(), 16000);
            assert_eq!(reader.spec().sample_rate, 16000);
        }
    }
}
