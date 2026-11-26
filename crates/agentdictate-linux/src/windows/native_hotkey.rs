#[path = "hotkey_events.rs"]
mod events;
use crate::hotkey::*;
pub use events::*;
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    io,
    sync::{
        Arc,
        mpsc::{self, Receiver},
    },
    time::Instant,
};
use windows_sys::Win32::{
    Foundation::*,
    System::{
        LibraryLoader::*,
        Power::{
            HPOWERNOTIFY, RegisterSuspendResumeNotification, UnregisterSuspendResumeNotification,
        },
        RemoteDesktop::{
            NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification,
            WTSUnRegisterSessionNotification,
        },
        Threading::*,
    },
    UI::{Input::KeyboardAndMouse::GetAsyncKeyState, WindowsAndMessaging::*},
};
struct HookState {
    tracker: HotkeyTracker,
    /// The virtual key each tracked key last went down as, so its physical
    /// state can be checked with `GetAsyncKeyState`. Low-level hooks report
    /// left/right-specific modifier keys (VK_LCONTROL, VK_RCONTROL, ...).
    virtual_keys: HashMap<KeyCode, u32>,
    sender: mpsc::Sender<NativeHotkeyEvent>,
}
/// Whether `virtual_key` is physically down right now. Called only for keys
/// other than the one the hook is handling, whose async state the system
/// updates after the hook returns.
fn virtual_key_is_down(virtual_key: u32) -> bool {
    // SAFETY: GetAsyncKeyState has no preconditions; any key code is valid.
    let state = unsafe { GetAsyncKeyState(virtual_key as i32) };
    state as u16 & 0x8000 != 0
}
thread_local! {
    static STATE: RefCell<Option<HookState>> = const { RefCell::new(None) };
    /// The hook currently installed by this thread, replaced on reinstall.
    static HOOK: Cell<HHOOK> = const { Cell::new(std::ptr::null_mut()) };
}
/// How often the hook is reinstalled. Windows silently removes a low-level
/// hook whose callback ever exceeds `LowLevelHooksTimeout` and never says so;
/// reinstalling bounds how long the shortcut can stay dead.
const HOOK_REFRESH_INTERVAL_MS: u32 = 60_000;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HookMaintenance {
    /// Install a fresh hook; `forget_held_keys` also drops every tracked key,
    /// because releases made while the session was away were never seen.
    Reinstall { forget_held_keys: bool },
}
/// Decides what a message on the hook thread asks of the hook, if anything.
fn maintenance_for(message: u32, wparam: WPARAM, refresh_timer: usize) -> Option<HookMaintenance> {
    match message {
        WM_TIMER if refresh_timer != 0 && wparam == refresh_timer => {
            Some(HookMaintenance::Reinstall {
                forget_held_keys: false,
            })
        }
        WM_WTSSESSION_CHANGE
            if matches!(
                wparam as u32,
                WTS_SESSION_UNLOCK | WTS_CONSOLE_CONNECT | WTS_REMOTE_CONNECT
            ) =>
        {
            Some(HookMaintenance::Reinstall {
                forget_held_keys: true,
            })
        }
        WM_POWERBROADCAST
            if matches!(wparam as u32, PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND) =>
        {
            Some(HookMaintenance::Reinstall {
                forget_held_keys: true,
            })
        }
        _ => None,
    }
}
fn install_hook() -> io::Result<HHOOK> {
    // SAFETY: `hook` matches HOOKPROC and lives for the whole process; a null
    // module name asks for this executable's handle.
    let handle = unsafe {
        SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(hook),
            GetModuleHandleW(std::ptr::null()),
            0,
        )
    };
    if handle.is_null() {
        Err(io::Error::last_os_error())
    } else {
        Ok(handle)
    }
}
/// Runs on the hook thread only: the new hook goes in before the old one is
/// removed, so no key is missed; a key seen by both hooks is idempotent.
fn apply_maintenance(maintenance: HookMaintenance) {
    let HookMaintenance::Reinstall { forget_held_keys } = maintenance;
    match install_hook() {
        Ok(handle) => {
            let previous = HOOK.replace(handle);
            if !previous.is_null() {
                // SAFETY: `previous` was installed by this thread. Unhooking
                // one that Windows already removed fails harmlessly.
                unsafe { UnhookWindowsHookEx(previous) };
            }
            tracing::debug!(forget_held_keys, "reinstalled the keyboard hook");
        }
        Err(error) => {
            tracing::warn!(%error, "could not reinstall the keyboard hook; keeping the old one");
        }
    }
    if forget_held_keys {
        STATE.with(|slot| {
            if let Some(state) = slot.borrow_mut().as_mut() {
                state.virtual_keys.clear();
                if let Some(signal) = state.tracker.release_all() {
                    let _ = state
                        .sender
                        .send(NativeHotkeyEvent::Signal(NativeHotkeySignal {
                            signal,
                            device: windows_keyboard(),
                            trigger: NativeHotkeySignalTrigger::DeviceDisconnected,
                            observed_at: Instant::now(),
                        }));
                }
            }
        });
    }
}
fn windows_keyboard() -> NativeHotkeyDevice {
    NativeHotkeyDevice {
        id: 1,
        path: "Windows keyboard".into(),
        name: "Windows keyboard hook".into(),
    }
}
unsafe extern "system" fn session_window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if let Some(maintenance) = maintenance_for(message, wparam, 0) {
        apply_maintenance(maintenance);
    }
    // SAFETY: forwards this window's own message unchanged.
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}
/// A message-only window on the hook thread that hears session unlock and
/// resume, so the hook is refreshed right away instead of at the next timer.
struct SessionWatch {
    window: HWND,
    power: HPOWERNOTIFY,
}
impl SessionWatch {
    fn create() -> Option<Self> {
        let class_name: Vec<u16> = "AgentDictateKeyboardSession\0".encode_utf16().collect();
        // SAFETY: the class name is NUL-terminated and outlives both calls;
        // the window procedure lives for the whole process. Registering a
        // class that already exists (a restarted listener) fails harmlessly.
        let window = unsafe {
            let instance = GetModuleHandleW(std::ptr::null());
            let class = WNDCLASSW {
                lpfnWndProc: Some(session_window_proc),
                hInstance: instance,
                lpszClassName: class_name.as_ptr(),
                ..std::mem::zeroed()
            };
            RegisterClassW(&class);
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                std::ptr::null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                instance,
                std::ptr::null(),
            )
        };
        if window.is_null() {
            tracing::warn!(
                error = %io::Error::last_os_error(),
                "could not watch for session unlock; the keyboard hook refreshes on its timer only"
            );
            return None;
        }
        // SAFETY: `window` is a live window owned by this thread.
        let power = unsafe {
            if WTSRegisterSessionNotification(window, NOTIFY_FOR_THIS_SESSION) == 0 {
                tracing::warn!(error = %io::Error::last_os_error(), "could not watch for session unlock");
            }
            RegisterSuspendResumeNotification(window, DEVICE_NOTIFY_WINDOW_HANDLE)
        };
        if power == 0 {
            tracing::warn!(error = %io::Error::last_os_error(), "could not watch for resume from sleep");
        }
        Some(Self { window, power })
    }
}
impl Drop for SessionWatch {
    fn drop(&mut self) {
        // SAFETY: both registrations and the window belong to this thread and
        // are released exactly once.
        unsafe {
            if self.power != 0 {
                UnregisterSuspendResumeNotification(self.power);
            }
            WTSUnRegisterSessionNotification(self.window);
            DestroyWindow(self.window);
        }
    }
}
pub const SELF_INJECTION: usize = 0x41474449;
pub fn scan_code(scan: u32, extended: bool) -> u16 {
    if !extended {
        return scan as u16;
    }
    match scan {
        0x1d => KEY_RIGHT_CTRL,
        0x38 => KEY_RIGHT_ALT,
        0x5b => KEY_LEFT_META,
        0x5c => KEY_RIGHT_META,
        0x1c => 96,
        0x35 => 98,
        0x47 => 102,
        0x48 => 103,
        0x49 => 104,
        0x4b => 105,
        0x4d => 106,
        0x4f => 107,
        0x50 => 108,
        0x51 => 109,
        0x52 => 110,
        0x53 => 111,
        _ => scan as u16,
    }
}
unsafe extern "system" fn hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: WH_KEYBOARD_LL supplies a KBDLLHOOKSTRUCT for HC_ACTION.
        let event = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        if event.dwExtraInfo != SELF_INJECTION {
            let state = if wparam as u32 == WM_KEYUP || wparam as u32 == WM_SYSKEYUP {
                KeyState::Released
            } else {
                KeyState::Pressed
            };
            let input = KeyInput::new(
                scan_code(event.scanCode, event.flags & LLKHF_EXTENDED != 0),
                state,
            );
            STATE.with(|slot| {
                let mut slot = slot.borrow_mut();
                let Some(state) = slot.as_mut() else {
                    return;
                };
                match input.state {
                    KeyState::Released => {
                        state.virtual_keys.remove(&input.code);
                    }
                    KeyState::Pressed | KeyState::Repeated => {
                        state.virtual_keys.insert(input.code, event.vkCode);
                    }
                }
                let virtual_keys = &state.virtual_keys;
                // Missed releases (secure desktop, elevated focus) must not
                // leave a modifier "held": check each other tracked key.
                if let Some(signal) = state.tracker.input_reconciled(1, input, |code| {
                    virtual_keys.get(&code).map(|&key| virtual_key_is_down(key))
                }) {
                    let _ = state
                        .sender
                        .send(NativeHotkeyEvent::Signal(NativeHotkeySignal {
                            signal,
                            device: windows_keyboard(),
                            trigger: NativeHotkeySignalTrigger::Input(input),
                            observed_at: Instant::now(),
                        }));
                }
            });
        }
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}
pub struct NativeHotkeyListener {
    incoming: Receiver<NativeHotkeyEvent>,
    control: NativeHotkeyControl,
    readiness: NativeHotkeyReadiness,
}
impl NativeHotkeyListener {
    pub fn start(spec: HotkeySpec) -> io::Result<Self> {
        let (sender, incoming) = mpsc::channel();
        let (commands, control_rx) = mpsc::channel();
        let (ready, initialized) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("agentdictate-windows-keyboard".into())
            .spawn(move || unsafe {
                let mut message = std::mem::zeroed();
                PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);
                STATE.with(|slot| {
                    *slot.borrow_mut() = Some(HookState {
                        tracker: HotkeyTracker::new(spec),
                        virtual_keys: HashMap::new(),
                        sender: sender.clone(),
                    })
                });
                // A preempted callback that overruns LowLevelHooksTimeout gets
                // the hook silently removed, so this thread runs first.
                if SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) == 0 {
                    tracing::warn!(
                        error = %io::Error::last_os_error(),
                        "could not raise the keyboard hook thread's priority"
                    );
                }
                match install_hook() {
                    Ok(handle) => HOOK.set(handle),
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                }
                let refresh_timer =
                    SetTimer(std::ptr::null_mut(), 0, HOOK_REFRESH_INTERVAL_MS, None);
                if refresh_timer == 0 {
                    tracing::warn!(
                        error = %io::Error::last_os_error(),
                        "could not schedule keyboard hook refreshes"
                    );
                }
                let session_watch = SessionWatch::create();
                let _ = ready.send(Ok(GetCurrentThreadId()));
                'messages: while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
                    if message.hwnd.is_null()
                        && let Some(maintenance) =
                            maintenance_for(message.message, message.wParam, refresh_timer)
                    {
                        apply_maintenance(maintenance);
                    }
                    while let Ok(command) = control_rx.try_recv() {
                        match command {
                            events::ListenerCommand::Stop => break 'messages,
                            events::ListenerCommand::Reconfigure {
                                requested,
                                response,
                            } => {
                                let hotkey = requested.display().to_owned();
                                STATE.with(|slot| {
                                    if let Some(state) = slot.borrow_mut().as_mut() {
                                        state.tracker = HotkeyTracker::new(requested);
                                    }
                                });
                                let _ = sender.send(NativeHotkeyEvent::Reconfigured { hotkey });
                                let _ = response.send(Ok(()));
                            }
                        }
                    }
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                drop(session_watch);
                if refresh_timer != 0 {
                    KillTimer(std::ptr::null_mut(), refresh_timer);
                }
                UnhookWindowsHookEx(HOOK.replace(std::ptr::null_mut()));
                STATE.with(|slot| *slot.borrow_mut() = None);
            })?;
        let thread_id = initialized.recv().map_err(io::Error::other)??;
        Ok(Self {
            incoming,
            control: NativeHotkeyControl {
                commands,
                wake: Arc::new(events::Wake(thread_id)),
            },
            readiness: NativeHotkeyReadiness {
                status: HotkeyListenerStatus::Ready { active_devices: 1 },
                discovered_devices: 1,
                failed_devices: vec![],
            },
        })
    }
    pub fn readiness(&self) -> &NativeHotkeyReadiness {
        &self.readiness
    }
    pub fn control_handle(&self) -> NativeHotkeyControl {
        self.control.clone()
    }
    pub fn recv(&self) -> Result<NativeHotkeyEvent, mpsc::RecvError> {
        self.incoming.recv()
    }
}
impl Drop for NativeHotkeyListener {
    fn drop(&mut self) {
        let _ = self.control.stop();
    }
}
pub struct NativeHotkeyRetryWatcher;
impl NativeHotkeyRetryWatcher {
    pub fn new() -> io::Result<Self> {
        Ok(Self)
    }
    pub fn wait(self) -> io::Result<()> {
        std::thread::sleep(std::time::Duration::from_secs(5));
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_refresh_timer_reinstalls_without_forgetting_held_keys() {
        assert_eq!(
            maintenance_for(WM_TIMER, 7, 7),
            Some(HookMaintenance::Reinstall {
                forget_held_keys: false
            })
        );
        assert_eq!(maintenance_for(WM_TIMER, 8, 7), None);
        assert_eq!(maintenance_for(WM_TIMER, 0, 0), None);
    }
    #[test]
    fn unlock_and_resume_reinstall_and_forget_held_keys() {
        let forget = Some(HookMaintenance::Reinstall {
            forget_held_keys: true,
        });
        for (message, wparam) in [
            (WM_WTSSESSION_CHANGE, WTS_SESSION_UNLOCK),
            (WM_WTSSESSION_CHANGE, WTS_CONSOLE_CONNECT),
            (WM_POWERBROADCAST, PBT_APMRESUMEAUTOMATIC),
            (WM_POWERBROADCAST, PBT_APMRESUMESUSPEND),
        ] {
            assert_eq!(maintenance_for(message, wparam as WPARAM, 0), forget);
        }
        assert_eq!(
            maintenance_for(WM_WTSSESSION_CHANGE, WTS_SESSION_LOCK as WPARAM, 0),
            None
        );
        assert_eq!(
            maintenance_for(WM_POWERBROADCAST, PBT_APMSUSPEND as WPARAM, 0),
            None
        );
    }
    #[test]
    fn forgetting_held_keys_releases_a_matched_chord_once() {
        let mut tracker = HotkeyTracker::new("ctrl+space".parse().unwrap());
        tracker.input(1, KeyInput::new(KEY_LEFT_CTRL, KeyState::Pressed));
        tracker.input(1, KeyInput::new(KEY_SPACE, KeyState::Pressed));
        assert_eq!(tracker.release_all(), Some(HotkeySignal::Released));
        assert_eq!(tracker.release_all(), None);
        assert_eq!(
            tracker.input(1, KeyInput::new(KEY_SPACE, KeyState::Pressed)),
            None
        );
    }
    #[test]
    fn extended_keys_distinguish_right_modifiers() {
        assert_eq!(scan_code(0x1d, true), KEY_RIGHT_CTRL);
        assert_eq!(scan_code(0x1d, false), KEY_LEFT_CTRL);
        assert_eq!(scan_code(0x5b, true), KEY_LEFT_META);
    }
    #[test]
    fn windows_scan_codes_drive_existing_toggle_edges() {
        let mut tracker = HotkeyTracker::new("ctrl+space".parse().unwrap());
        assert_eq!(
            tracker.input(1, KeyInput::new(scan_code(0x1d, false), KeyState::Pressed)),
            None
        );
        assert_eq!(
            tracker.input(1, KeyInput::new(scan_code(0x39, false), KeyState::Pressed)),
            Some(HotkeySignal::Pressed)
        );
        assert_eq!(
            tracker.input(1, KeyInput::new(scan_code(0x39, false), KeyState::Released)),
            Some(HotkeySignal::Released)
        );
    }
}
