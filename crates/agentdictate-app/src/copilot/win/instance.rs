//! One copilot per desktop session. A second launch asks the running copilot
//! to show itself and exits.

use windows_sys::Win32::{Foundation::*, System::Threading::*};

const MUTEX_NAME: &str = "Local\\AgentDictateMeetingCopilot";
const SHOW_EVENT_NAME: &str = "Local\\AgentDictateMeetingCopilotShow";
const SYNCHRONIZE: u32 = 0x0010_0000;
const EVENT_MODIFY_STATE: u32 = 0x0002;

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

/// Held for the copilot's lifetime.
pub(crate) struct InstanceGuard {
    mutex: HANDLE,
    show_event: HANDLE,
}

unsafe impl Send for InstanceGuard {}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.show_event);
            CloseHandle(self.mutex);
        }
    }
}

impl InstanceGuard {
    /// Calls `on_show` whenever another launch asks this copilot to appear.
    pub(crate) fn watch_show_requests(&self, on_show: impl Fn() + Send + 'static) {
        let event = self.show_event as usize;
        let _ = std::thread::Builder::new()
            .name("copilot-instance".into())
            .spawn(move || {
                loop {
                    let result = unsafe { WaitForSingleObject(event as HANDLE, INFINITE) };
                    if result != WAIT_OBJECT_0 {
                        return;
                    }
                    on_show();
                }
            });
    }
}

/// Becomes the primary copilot, or signals the existing one.
pub(crate) fn acquire() -> Option<InstanceGuard> {
    let mutex_name = wide(MUTEX_NAME);
    let event_name = wide(SHOW_EVENT_NAME);
    unsafe {
        let mutex = CreateMutexW(std::ptr::null(), 1, mutex_name.as_ptr());
        let already_running = GetLastError() == ERROR_ALREADY_EXISTS;
        let show_event = CreateEventW(std::ptr::null(), 0, 0, event_name.as_ptr());
        if already_running || mutex.is_null() {
            if !show_event.is_null() {
                SetEvent(show_event);
                CloseHandle(show_event);
            }
            if !mutex.is_null() {
                CloseHandle(mutex);
            }
            return None;
        }
        Some(InstanceGuard { mutex, show_event })
    }
}

/// True while a copilot is running in this session.
pub(crate) fn is_running() -> bool {
    let name = wide(MUTEX_NAME);
    unsafe {
        let handle = OpenMutexW(SYNCHRONIZE, 0, name.as_ptr());
        if handle.is_null() {
            return false;
        }
        CloseHandle(handle);
        true
    }
}

/// Asks a running copilot to show itself.
pub(crate) fn request_show() -> bool {
    let name = wide(SHOW_EVENT_NAME);
    unsafe {
        let handle = OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr());
        if handle.is_null() {
            return false;
        }
        SetEvent(handle);
        CloseHandle(handle);
        true
    }
}
