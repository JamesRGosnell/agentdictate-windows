//! One listening copilot per desktop session. A second launch asks the
//! listening copilot to show itself and exits.
//!
//! The guard is held only while a copilot listens. Once its meeting ends it
//! lets go, so a new session can start while the old process finishes its
//! notes out of sight.

use windows_sys::Win32::{Foundation::*, System::Threading::*};

const MUTEX_NAME: &str = "Local\\AgentDictateMeetingCopilot";
const SHOW_EVENT_NAME: &str = "Local\\AgentDictateMeetingCopilotShow";
const SYNCHRONIZE: u32 = 0x0010_0000;
const EVENT_MODIFY_STATE: u32 = 0x0002;

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

/// Held while the copilot listens.
pub(crate) struct InstanceGuard {
    mutex: HANDLE,
    show_event: HANDLE,
    /// Ends the show watcher, so a show request is never taken by a copilot
    /// that has stopped listening.
    stop_event: HANDLE,
    watcher: Option<std::thread::JoinHandle<()>>,
}

// SAFETY: the handles are kernel objects usable from any thread; the guard
// is the only owner and closes each once, in `drop`, after the watcher that
// waits on them has ended.
unsafe impl Send for InstanceGuard {}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe {
            if !self.stop_event.is_null() {
                SetEvent(self.stop_event);
            }
        }
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
        unsafe {
            if !self.stop_event.is_null() {
                CloseHandle(self.stop_event);
            }
            if !self.show_event.is_null() {
                CloseHandle(self.show_event);
            }
            CloseHandle(self.mutex);
        }
    }
}

impl InstanceGuard {
    /// Calls `on_show` whenever another launch asks this copilot to appear,
    /// until the guard is dropped.
    pub(crate) fn watch_show_requests(&mut self, on_show: impl Fn() + Send + 'static) {
        if self.stop_event.is_null() || self.show_event.is_null() {
            return;
        }
        let handles = [self.show_event as usize, self.stop_event as usize];
        self.watcher = std::thread::Builder::new()
            .name("copilot-instance".into())
            .spawn(move || {
                let handles = handles.map(|handle| handle as HANDLE);
                loop {
                    let result =
                        unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
                    if result != WAIT_OBJECT_0 {
                        return;
                    }
                    on_show();
                }
            })
            .ok();
    }
}

/// Becomes the listening copilot, or signals the one already listening.
pub(crate) fn acquire() -> Option<InstanceGuard> {
    acquire_as(MUTEX_NAME, SHOW_EVENT_NAME)
}

fn acquire_as(mutex_name: &str, event_name: &str) -> Option<InstanceGuard> {
    let mutex_name = wide(mutex_name);
    let event_name = wide(event_name);
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
        // Manual reset: once set, it stays set for the watcher to see.
        let stop_event = CreateEventW(std::ptr::null(), 1, 0, std::ptr::null());
        Some(InstanceGuard {
            mutex,
            show_event,
            stop_event,
            watcher: None,
        })
    }
}

/// True while a copilot is listening in this session. One that is only
/// finishing its notes does not count.
pub(crate) fn is_running() -> bool {
    is_running_as(MUTEX_NAME)
}

fn is_running_as(name: &str) -> bool {
    let name = wide(name);
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
    request_show_as(SHOW_EVENT_NAME)
}

fn request_show_as(name: &str) -> bool {
    let name = wide(name);
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

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    #[test]
    fn a_copilot_that_stops_listening_makes_way_for_a_new_one() {
        // Names of this test's own, so a copilot the user is running is
        // never signalled.
        let id = uuid::Uuid::new_v4();
        let mutex = format!(r"Local\AgentDictateTestCopilot-{id}");
        let event = format!(r"Local\AgentDictateTestCopilotShow-{id}");
        let mut listening = acquire_as(&mutex, &event).expect("the first copilot listens");
        let shown = Arc::new(AtomicUsize::new(0));
        {
            let shown = Arc::clone(&shown);
            listening.watch_show_requests(move || {
                shown.fetch_add(1, Ordering::Relaxed);
            });
        }
        assert!(is_running_as(&mutex));
        // A second launch shows the listening copilot instead.
        assert!(acquire_as(&mutex, &event).is_none());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while shown.load(Ordering::Relaxed) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(shown.load(Ordering::Relaxed), 1);
        // Its meeting over, it lets go while it writes its notes.
        drop(listening);
        assert!(!is_running_as(&mutex));
        assert!(!request_show_as(&event), "nobody is left to show");
        let next = acquire_as(&mutex, &event);
        assert!(next.is_some(), "a new session starts");
    }
}
