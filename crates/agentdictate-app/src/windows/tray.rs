use agentdictate_core::{ClientCommand, ServerMessageKind, WorkflowPhase};
use agentdictate_runtime::IpcClient;
use std::{
    cell::{Cell, RefCell},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
};
use windows_sys::Win32::{
    Foundation::*,
    System::LibraryLoader::*,
    UI::{Shell::*, WindowsAndMessaging::*},
};
static TRAY_REQUEST_ID: AtomicU64 = AtomicU64::new(10_000);
/// User intent emitted by the desktop tray. Menu callbacks only enqueue these
/// values; IPC and process work happens away from the status-notifier thread.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrayAction {
    OpenSettings,
    ToggleDictation,
    StartLiteral,
    OpenCopilot,
    OpenMeetingNotes,
    Quit,
}

/// Converts the state-sensitive tray toggle into a single daemon command.
/// Busy processing states intentionally produce no command rather than
/// replaying an action after the current dictation completes. A dictation
/// waiting in Recovery does not block a new one, as with the hotkey.
#[must_use]
pub const fn tray_command_for_phase(
    action: TrayAction,
    phase: WorkflowPhase,
    request_id: u64,
) -> Option<ClientCommand> {
    if matches!(action, TrayAction::StartLiteral) {
        return match phase {
            WorkflowPhase::Ready | WorkflowPhase::NeedsAttention { .. } => {
                Some(ClientCommand::start_recording_in_mode(
                    request_id,
                    agentdictate_core::DictationMode::Literal,
                ))
            }
            _ => None,
        };
    }
    if !matches!(action, TrayAction::ToggleDictation) {
        return None;
    }
    match phase {
        WorkflowPhase::Ready | WorkflowPhase::NeedsAttention { .. } => {
            Some(ClientCommand::start_recording(request_id))
        }
        WorkflowPhase::Starting { .. } | WorkflowPhase::Recording { .. } => {
            Some(ClientCommand::stop_recording(request_id))
        }
        WorkflowPhase::Stopping { .. } | WorkflowPhase::Processing { .. } => None,
    }
}

thread_local! {static ACTIONS:RefCell<Option<mpsc::Sender<TrayAction>>>=const{RefCell::new(None)}; static ICON:RefCell<Option<NOTIFYICONDATAW>>=const{RefCell::new(None)}; static ADD_ATTEMPTS:Cell<u32>=const{Cell::new(0)};}
/// The shell can refuse NIM_ADD while Explorer is still starting (sign-in,
/// autostart) or restarting; the icon is retried on this timer instead of
/// being lost for the session.
const ICON_RETRY_TIMER: usize = 1;
const ICON_RETRY_INTERVAL_MS: u32 = 5_000;
/// Five-second retries for three minutes; after that only TaskbarCreated,
/// sent when Explorer (re)starts, adds the icon.
const ICON_RETRY_LIMIT: u32 = 36;
/// Shows the icon, returning whether the shell has it. NIM_MODIFY first: a
/// NIM_ADD that timed out may still have added it, and adding twice fails.
unsafe fn show_icon(icon: &NOTIFYICONDATAW) -> bool {
    // SAFETY: `icon` is a fully initialized NOTIFYICONDATAW for this window.
    unsafe { Shell_NotifyIconW(NIM_MODIFY, icon) != 0 || Shell_NotifyIconW(NIM_ADD, icon) != 0 }
}
/// Shows the icon now or schedules retries; runs on the tray thread.
unsafe fn add_icon_or_retry(hwnd: HWND) -> bool {
    let shown = ICON.with(|slot| {
        slot.borrow()
            .as_ref()
            // SAFETY: the stored icon belongs to this thread's tray window.
            .is_some_and(|icon| unsafe { show_icon(icon) })
    });
    // SAFETY: the timer belongs to `hwnd`, a live window of this thread.
    unsafe {
        if shown {
            KillTimer(hwnd, ICON_RETRY_TIMER);
            ADD_ATTEMPTS.set(0);
            return true;
        }
        let attempts = ADD_ATTEMPTS.get() + 1;
        ADD_ATTEMPTS.set(attempts);
        if attempts == 1 {
            tracing::warn!(
                error = %std::io::Error::last_os_error(),
                "could not add the tray icon; retrying"
            );
            SetTimer(hwnd, ICON_RETRY_TIMER, ICON_RETRY_INTERVAL_MS, None);
        } else if attempts > ICON_RETRY_LIMIT {
            KillTimer(hwnd, ICON_RETRY_TIMER);
            ADD_ATTEMPTS.set(0);
            tracing::warn!("tray icon is still unavailable; it will appear when Explorer restarts");
        }
    }
    false
}
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if message == RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) {
            KillTimer(hwnd, ICON_RETRY_TIMER);
            ADD_ATTEMPTS.set(0);
            add_icon_or_retry(hwnd);
            return 0;
        }
        if message == WM_TIMER && wparam == ICON_RETRY_TIMER {
            if add_icon_or_retry(hwnd) {
                tracing::info!("tray icon added after retrying");
            }
            return 0;
        }
        if message == WM_APP + 1 {
            let mut action = None;
            if lparam as u32 == WM_LBUTTONDBLCLK {
                action = Some(TrayAction::OpenSettings);
            }
            if lparam as u32 == WM_RBUTTONUP || lparam as u32 == WM_CONTEXTMENU {
                let menu = CreatePopupMenu();
                for (id, label) in [
                    (1, "Open AgentDictate"),
                    (2, "Toggle dictation"),
                    (3, "Start literal dictation"),
                    (0, ""),
                    (5, "Meeting Copilot"),
                    (6, "Meeting notes"),
                    (0, ""),
                    (4, "Quit AgentDictate"),
                ] {
                    if id == 0 {
                        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
                    } else {
                        AppendMenuW(menu, MF_STRING, id, wide(label).as_ptr());
                    }
                }
                let mut point = std::mem::zeroed();
                GetCursorPos(&mut point);
                SetForegroundWindow(hwnd);
                let selected = TrackPopupMenu(
                    menu,
                    TPM_RETURNCMD | TPM_NONOTIFY,
                    point.x,
                    point.y,
                    0,
                    hwnd,
                    std::ptr::null(),
                );
                DestroyMenu(menu);
                PostMessageW(hwnd, WM_NULL, 0, 0);
                action = match selected {
                    1 => Some(TrayAction::OpenSettings),
                    2 => Some(TrayAction::ToggleDictation),
                    3 => Some(TrayAction::StartLiteral),
                    4 => Some(TrayAction::Quit),
                    5 => Some(TrayAction::OpenCopilot),
                    6 => Some(TrayAction::OpenMeetingNotes),
                    _ => None,
                };
            }
            if let Some(action) = action {
                ACTIONS.with(|slot| {
                    if let Some(sender) = slot.borrow().as_ref() {
                        let _ = sender.send(action);
                    }
                });
            }
            return 0;
        }
        if message == WM_DESTROY {
            PostQuitMessage(0);
            return 0;
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}
pub struct SystemTrayHandle {
    window: usize,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Drop for SystemTrayHandle {
    fn drop(&mut self) {
        unsafe {
            PostMessageW(self.window as HWND, WM_CLOSE, 0, 0);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
pub fn start_system_tray(runtime: PathBuf, settings: PathBuf) -> std::io::Result<SystemTrayHandle> {
    let (actions, incoming) = mpsc::channel();
    let (ready, initialized) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("agentdictate-tray-actions".into())
        .spawn(move || {
            while let Ok(action) = incoming.recv() {
                if let Err(error) = execute_tray_action(action, &runtime, &settings) {
                    tracing::error!(%error,"tray action failed");
                }
            }
        })?;
    let thread = std::thread::Builder::new()
        .name("agentdictate-windows-tray".into())
        .spawn(move || unsafe {
            ACTIONS.with(|slot| *slot.borrow_mut() = Some(actions));
            let class = wide("AgentDictate.Tray");
            let instance = GetModuleHandleW(std::ptr::null());
            let mut wc: WNDCLASSW = std::mem::zeroed();
            wc.lpfnWndProc = Some(window_proc);
            wc.hInstance = instance;
            wc.lpszClassName = class.as_ptr();
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                wide("AgentDictate").as_ptr(),
                0,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                let _ = ready.send(Err(std::io::Error::last_os_error()));
                return;
            }
            let mut icon: NOTIFYICONDATAW = std::mem::zeroed();
            icon.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            icon.hWnd = hwnd;
            icon.uID = 1;
            icon.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
            icon.uCallbackMessage = WM_APP + 1;
            // MAKEINTRESOURCEW(1): the low word is a resource identifier, not a dereferenceable pointer.
            icon.hIcon = LoadIconW(instance, std::ptr::without_provenance::<u16>(1));
            if icon.hIcon.is_null() {
                let _ = ready.send(Err(std::io::Error::last_os_error()));
                DestroyWindow(hwnd);
                return;
            }
            let tip = wide("AgentDictate");
            icon.szTip[..tip.len()].copy_from_slice(&tip);
            ICON.with(|slot| *slot.borrow_mut() = Some(icon));
            // A refused first add is retried; the window and its loop stay
            // alive so TaskbarCreated can still add the icon later.
            add_icon_or_retry(hwnd);
            let _ = ready.send(Ok(hwnd as usize));
            let mut message = std::mem::zeroed();
            while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            Shell_NotifyIconW(NIM_DELETE, &icon);
            ICON.with(|slot| *slot.borrow_mut() = None);
            ACTIONS.with(|slot| *slot.borrow_mut() = None);
        })?;
    let window = initialized.recv().map_err(std::io::Error::other)??;
    Ok(SystemTrayHandle {
        window,
        thread: Some(thread),
    })
}
pub fn settings_executable_for_current_process() -> std::io::Result<PathBuf> {
    Ok(std::env::current_exe()?.with_file_name("agentdictate.exe"))
}
fn execute_tray_action(
    action: TrayAction,
    runtime_directory: &Path,
    settings_executable: &Path,
) -> anyhow::Result<()> {
    match action {
        TrayAction::OpenSettings => {
            drop(
                Command::new(settings_executable)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()?,
            );
            Ok(())
        }
        TrayAction::ToggleDictation | TrayAction::StartLiteral => {
            let (mut client, initial) = IpcClient::connect(runtime_directory)?;
            let ServerMessageKind::Snapshot { snapshot, .. } = initial.kind else {
                anyhow::bail!("daemon did not provide its current workflow")
            };
            let request_id = TRAY_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
            let Some(command) = tray_command_for_phase(action, snapshot.workflow.phase, request_id)
            else {
                tracing::info!("dictation is busy; tray toggle ignored");
                return Ok(());
            };
            reject_command_error(client.send(command)?.kind)
        }
        TrayAction::OpenCopilot => {
            crate::launch_copilot()?;
            Ok(())
        }
        TrayAction::OpenMeetingNotes => {
            drop(
                Command::new(settings_executable)
                    .args(["--route", "meetings"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()?,
            );
            Ok(())
        }
        TrayAction::Quit => {
            let (mut client, _) = IpcClient::connect(runtime_directory)?;
            let request_id = TRAY_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
            reject_command_error(client.send(ClientCommand::quit(request_id))?.kind)
        }
    }
}

fn reject_command_error(message: ServerMessageKind) -> anyhow::Result<()> {
    if let ServerMessageKind::CommandRejected { error, .. } = message {
        anyhow::bail!(error)
    }
    Ok(())
}
