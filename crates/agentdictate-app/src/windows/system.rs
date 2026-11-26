//! Native Windows recording and one-shot clipboard delivery.
use crate::{CapturedRecording, RecordingController};
use agentdictate_core::{ClientCommand, JobId, Settings};
use agentdictate_runtime::{
    Deliverer, DeliveryDisposition, ExternalError, FocusedWindow, IpcClient, Recorder, RecordingJob,
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
    Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_MANDATORY_LABEL,
        TOKEN_QUERY, TokenIntegrityLevel,
    },
    System::Threading::*,
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

type Reply<T> = mpsc::SyncSender<Result<T, ExternalError>>;
enum AudioCommand {
    Start(JobId, PathBuf, Reply<()>),
    Finish(JobId, Reply<CapturedRecording>),
}
#[path = "clipboard.rs"]
mod clipboard;
#[path = "ducking.rs"]
mod ducking;
use clipboard::Clipboard as _;
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
        let ducker = ducking::PlaybackDucker::new(runtime.join("ducking.json"));
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
            ducker,
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
impl SystemDeliverer {
    pub fn for_environment(shortcut: &str) -> Self {
        Self {
            shortcut: shortcut.into(),
        }
    }
    pub fn update_shortcut(&mut self, shortcut: &str) {
        self.shortcut = shortcut.into();
    }
    /// Leaves `text` on the clipboard for the user to paste.
    pub fn copy_text(&mut self, text: &str) -> Result<(), ExternalError> {
        clipboard::Win32Clipboard.write_text(text, false)
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
/// Mandatory integrity level of a process, as far as we are allowed to see it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessIntegrity {
    Level(u32),
    /// The token could not be opened for query, which from a normal process
    /// means the target runs elevated.
    AccessDenied,
    Unknown,
}
const ELEVATED_PASTE_REASON: &str = "Can't paste into an administrator window; press Ctrl+V";
/// UIPI silently drops input sent to a higher-integrity window while
/// SendInput still reports success, so such a paste must not be attempted.
fn paste_blocked_by_integrity(target: ProcessIntegrity, ours: ProcessIntegrity) -> bool {
    match (target, ours) {
        (ProcessIntegrity::AccessDenied, _) => true,
        (ProcessIntegrity::Level(target), ProcessIntegrity::Level(ours)) => target > ours,
        _ => false,
    }
}
fn process_integrity(pid: Option<u32>) -> ProcessIntegrity {
    // SAFETY: every handle opened here is closed before returning, and the
    // token buffer is u32-aligned and sized by the first GetTokenInformation.
    unsafe {
        let process = match pid {
            None => GetCurrentProcess(),
            Some(pid) => OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid),
        };
        if process.is_null() {
            return if GetLastError() == ERROR_ACCESS_DENIED {
                ProcessIntegrity::AccessDenied
            } else {
                ProcessIntegrity::Unknown
            };
        }
        let mut token = std::ptr::null_mut();
        let opened = OpenProcessToken(process, TOKEN_QUERY, &mut token) != 0;
        let open_error = GetLastError();
        if pid.is_some() {
            CloseHandle(process);
        }
        if !opened {
            return if open_error == ERROR_ACCESS_DENIED {
                ProcessIntegrity::AccessDenied
            } else {
                ProcessIntegrity::Unknown
            };
        }
        let mut size = 0;
        GetTokenInformation(
            token,
            TokenIntegrityLevel,
            std::ptr::null_mut(),
            0,
            &mut size,
        );
        let mut buffer = vec![0u32; (size as usize).div_ceil(4).max(1)];
        let read = GetTokenInformation(
            token,
            TokenIntegrityLevel,
            buffer.as_mut_ptr().cast(),
            (buffer.len() * 4) as u32,
            &mut size,
        ) != 0;
        let integrity = if read {
            let label = &*buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>();
            let count = *GetSidSubAuthorityCount(label.Label.Sid);
            if count == 0 {
                ProcessIntegrity::Unknown
            } else {
                ProcessIntegrity::Level(*GetSidSubAuthority(label.Label.Sid, u32::from(count) - 1))
            }
        } else {
            ProcessIntegrity::Unknown
        };
        CloseHandle(token);
        integrity
    }
}
/// Whether a finished dictation may be pasted, or only copied.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PasteDecision {
    Paste,
    CopyOnly,
}
/// A dictation is pasted only into the window it was stopped in. A delivery
/// without a stop window (a Recovery retry, or a focus that could not be
/// read at stop) pastes into the current window, as before.
fn paste_decision(stopped_in: Option<FocusedWindow>, now: Option<FocusedWindow>) -> PasteDecision {
    match stopped_in {
        None => PasteDecision::Paste,
        Some(stopped_in) if now == Some(stopped_in) => PasteDecision::Paste,
        Some(_) => PasteDecision::CopyOnly,
    }
}
fn foreground_window() -> Option<FocusedWindow> {
    // SAFETY: only queries the foreground HWND and its owning process id.
    let window = unsafe { GetForegroundWindow() };
    if window.is_null() {
        return None;
    }
    let mut process_id = 0;
    unsafe { GetWindowThreadProcessId(window, &mut process_id) };
    Some(FocusedWindow {
        window: window as usize as u64,
        process_id,
    })
}
/// The text stays on the clipboard for the user to paste; the job counts
/// as delivered.
const COPIED_ONLY: DeliveryDisposition = DeliveryDisposition::Submitted {
    copied_to_clipboard: true,
    paste_triggered: false,
};
fn not_sent(copied_to_clipboard: bool, reason: impl Into<String>) -> DeliveryDisposition {
    DeliveryDisposition::NotSent {
        copied_to_clipboard,
        reason: reason.into(),
    }
}
impl SystemDeliverer {
    /// Copies the transcript without pasting because the focus left the
    /// window the dictation was stopped in.
    fn copy_after_focus_moved(&mut self, text: &str) -> DeliveryDisposition {
        tracing::info!("the focus moved since the stop; copied without pasting");
        match self.copy_text(text) {
            Ok(()) => COPIED_ONLY,
            Err(error) => not_sent(
                false,
                format!("Could not copy the text, so nothing was pasted: {error}"),
            ),
        }
    }
}
impl Deliverer for SystemDeliverer {
    fn deliver(&mut self, job: &RecordingJob) -> Result<DeliveryDisposition, ExternalError> {
        self.deliver_to(job, None)
    }
    fn focused_window(&mut self) -> Option<FocusedWindow> {
        foreground_window()
    }
    /// Pastes with one injected shortcut. Every failure before any key is
    /// injected is `NotSent`, so the text can be pasted again safely.
    fn deliver_to(
        &mut self,
        job: &RecordingJob,
        stopped_in: Option<FocusedWindow>,
    ) -> Result<DeliveryDisposition, ExternalError> {
        // Copying needs no released modifiers, so a moved focus is
        // honored before waiting for them.
        if paste_decision(stopped_in, foreground_window()) == PasteDecision::CopyOnly {
            return Ok(self.copy_after_focus_moved(&job.final_text));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        // Wait for physical modifiers to be released; never release the user's held keys.
        while [VK_CONTROL, VK_SHIFT, VK_MENU, VK_LWIN, VK_RWIN]
            .iter()
            .any(|vk| unsafe { GetAsyncKeyState(*vk as i32) } < 0)
        {
            if Instant::now() >= deadline {
                return Ok(not_sent(
                    false,
                    "Release shortcut modifiers before pasting; the transcript remains in Recovery",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let Some(focused) = foreground_window() else {
            return Ok(if stopped_in.is_some() {
                self.copy_after_focus_moved(&job.final_text)
            } else {
                not_sent(false, "No focused destination window")
            });
        };
        if paste_decision(stopped_in, Some(focused)) == PasteDecision::CopyOnly {
            return Ok(self.copy_after_focus_moved(&job.final_text));
        }
        let target = focused.window as usize as HWND;
        let pid = focused.process_id;
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
            return Ok(not_sent(
                false,
                "Focus the destination application before stopping dictation",
            ));
        }
        let elevated_target =
            paste_blocked_by_integrity(process_integrity(Some(pid)), process_integrity(None));
        if elevated_target {
            if let Err(error) = self.copy_text(&job.final_text) {
                return Ok(not_sent(
                    false,
                    format!("Could not copy the text, so nothing was pasted: {error}"),
                ));
            }
            tracing::info!(pid, "focused window is elevated; copied without pasting");
            return Ok(not_sent(true, ELEVATED_PASTE_REASON));
        }
        let shortcut = self.shortcut.clone();
        let staged =
            clipboard::paste_staged(&mut clipboard::Win32Clipboard, &job.final_text, || {
                send_paste_keys(target, &shortcut)
            });
        match staged {
            Ok((disposition, restore)) => {
                if let Some(staged) = restore {
                    clipboard::restore_later(staged);
                }
                Ok(disposition)
            }
            Err(error) => Ok(not_sent(
                false,
                format!("Could not copy the text, so nothing was pasted: {error}"),
            )),
        }
    }
}
/// Sends the paste shortcut to `target`, whose transcript is already on the
/// clipboard, unless the focus or held keys changed since it was chosen.
fn send_paste_keys(target: HWND, shortcut: &str) -> DeliveryDisposition {
    if unsafe { GetForegroundWindow() } != target
        || [VK_CONTROL, VK_SHIFT, VK_MENU, VK_LWIN, VK_RWIN]
            .iter()
            .any(|vk| unsafe { GetAsyncKeyState(*vk as i32) } < 0)
    {
        return not_sent(
            true,
            "Focus or held keys changed before pasting; the text is on the clipboard",
        );
    }
    let mut class = [0u16; 256];
    let size = unsafe { GetClassNameW(target, class.as_mut_ptr(), class.len() as i32) };
    let class = String::from_utf16_lossy(&class[..size.max(0) as usize]);
    let keys = paste_keys(shortcut, &class);
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
    if count == 0 {
        return not_sent(
            true,
            format!(
                "Windows refused the paste keys; the text is on the clipboard: {}",
                std::io::Error::last_os_error()
            ),
        );
    }
    if count != inputs.len() as u32 {
        // Balance our own partial injection, without retrying the paste itself.
        let release: Vec<INPUT> = keys.iter().rev().map(|vk| key(*vk, true)).collect();
        unsafe {
            SendInput(
                release.len() as u32,
                release.as_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            );
        }
        return DeliveryDisposition::Ambiguous {
            copied_to_clipboard: true,
        };
    }
    DeliveryDisposition::Submitted {
        copied_to_clipboard: true,
        paste_triggered: true,
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
    fn a_dictation_is_pasted_only_into_the_window_it_was_stopped_in() {
        use PasteDecision::{CopyOnly, Paste};
        let editor = FocusedWindow {
            window: 0x1234,
            process_id: 40,
        };
        let browser = FocusedWindow {
            window: 0x5678,
            process_id: 41,
        };
        // The handle was freed and reused by another process's window.
        let reused = FocusedWindow {
            window: 0x1234,
            process_id: 42,
        };
        for (stopped_in, now, expected) in [
            (Some(editor), Some(editor), Paste),
            (Some(editor), Some(browser), CopyOnly),
            (Some(editor), Some(reused), CopyOnly),
            // Nothing has the focus, so it surely left the stop window.
            (Some(editor), None, CopyOnly),
            // Recovery retries have no stop window and paste where focused.
            (None, Some(browser), Paste),
            (None, None, Paste),
        ] {
            assert_eq!(
                paste_decision(stopped_in, now),
                expected,
                "{stopped_in:?} -> {now:?}"
            );
        }
    }
    #[test]
    fn paste_is_skipped_only_for_a_higher_integrity_or_unreadable_target() {
        use ProcessIntegrity::{AccessDenied, Level, Unknown};
        const MEDIUM: u32 = 0x2000;
        const HIGH: u32 = 0x3000;
        assert!(paste_blocked_by_integrity(Level(HIGH), Level(MEDIUM)));
        assert!(paste_blocked_by_integrity(AccessDenied, Level(MEDIUM)));
        assert!(paste_blocked_by_integrity(AccessDenied, Unknown));
        assert!(!paste_blocked_by_integrity(Level(MEDIUM), Level(MEDIUM)));
        assert!(!paste_blocked_by_integrity(Level(MEDIUM), Level(HIGH)));
        assert!(!paste_blocked_by_integrity(Unknown, Level(MEDIUM)));
        assert!(!paste_blocked_by_integrity(Level(HIGH), Unknown));
    }
    #[test]
    fn own_process_integrity_is_readable_and_never_blocks_itself() {
        let ours = process_integrity(None);
        assert!(matches!(ours, ProcessIntegrity::Level(_)), "{ours:?}");
        assert_eq!(process_integrity(Some(std::process::id())), ours);
        assert!(!paste_blocked_by_integrity(ours, ours));
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
