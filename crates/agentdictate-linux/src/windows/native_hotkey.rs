#[path = "hotkey_events.rs"]
mod events;
use crate::hotkey::*;
pub use events::*;
use std::{
    cell::RefCell,
    io,
    sync::{
        Arc,
        mpsc::{self, Receiver},
    },
    time::Instant,
};
use windows_sys::Win32::{
    Foundation::*,
    System::{LibraryLoader::*, Threading::*},
    UI::WindowsAndMessaging::*,
};
struct HookState {
    tracker: HotkeyTracker,
    sender: mpsc::Sender<NativeHotkeyEvent>,
}
thread_local! {static STATE:RefCell<Option<HookState>>=const{RefCell::new(None)};}
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
                if let Some(state) = slot.borrow_mut().as_mut()
                    && let Some(signal) = state.tracker.input(1, input)
                {
                    let _ = state
                        .sender
                        .send(NativeHotkeyEvent::Signal(NativeHotkeySignal {
                            signal,
                            device: NativeHotkeyDevice {
                                id: 1,
                                path: "Windows keyboard".into(),
                                name: "Windows keyboard hook".into(),
                            },
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
                        sender: sender.clone(),
                    })
                });
                let handle = SetWindowsHookExW(
                    WH_KEYBOARD_LL,
                    Some(hook),
                    GetModuleHandleW(std::ptr::null()),
                    0,
                );
                if handle.is_null() {
                    let _ = ready.send(Err(io::Error::last_os_error()));
                    return;
                }
                let _ = ready.send(Ok(GetCurrentThreadId()));
                'messages: while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
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
                UnhookWindowsHookEx(handle);
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
