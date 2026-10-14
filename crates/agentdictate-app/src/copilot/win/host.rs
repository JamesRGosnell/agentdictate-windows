//! Native window behavior for the copilot: hidden from screen capture,
//! always on top, never stealing focus unless the user is typing, and
//! clickable only where the bar and panel are drawn.

use std::cell::Cell;

use agentdictate_ui::{CopilotShape, CopilotWindowHost};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    System::Threading::{AttachThreadInput, GetCurrentThreadId},
    UI::{HiDpi::GetDpiForWindow, Input::KeyboardAndMouse::SetFocus, WindowsAndMessaging::*},
};

pub(crate) struct Win32CopilotHost {
    hwnd: usize,
    placed: Cell<bool>,
    previous_foreground: Cell<usize>,
}

impl Win32CopilotHost {
    /// Applies capture exclusion and overlay styles. Placement waits for the
    /// first resize, which runs after GPUI finishes creating the window.
    pub(crate) fn attach(hwnd: usize, exclude_from_capture: bool) -> Self {
        let host = Self {
            hwnd,
            placed: Cell::new(false),
            previous_foreground: Cell::new(0),
        };
        host.set_capture_excluded(exclude_from_capture);
        unsafe {
            let handle = host.handle();
            let style = GetWindowLongPtrW(handle, GWL_EXSTYLE);
            SetWindowLongPtrW(
                handle,
                GWL_EXSTYLE,
                style
                    | WS_EX_NOACTIVATE as isize
                    | WS_EX_TOOLWINDOW as isize
                    | WS_EX_TOPMOST as isize,
            );
        }
        host
    }

    fn handle(&self) -> HWND {
        self.hwnd as HWND
    }

    fn scale(&self) -> f32 {
        (unsafe { GetDpiForWindow(self.handle()) }.max(96) as f32) / 96.0
    }

    /// Centers the window at the top of the primary monitor's work area.
    fn place(&self, width: i32, height: i32) {
        unsafe {
            let monitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
            let mut info: MONITORINFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            if GetMonitorInfoW(monitor, &mut info) == 0 {
                return;
            }
            let x = info.rcWork.left + (info.rcWork.right - info.rcWork.left - width) / 2;
            SetWindowPos(
                self.handle(),
                HWND_TOPMOST,
                x,
                info.rcWork.top,
                width,
                height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
    }

    /// Moves the window up when growing it would push the panel off the
    /// bottom of the monitor the user dragged it to.
    fn keep_on_screen(&self) {
        unsafe {
            let mut rect: RECT = std::mem::zeroed();
            if GetWindowRect(self.handle(), &mut rect) == 0 {
                return;
            }
            let monitor = MonitorFromWindow(self.handle(), MONITOR_DEFAULTTONEAREST);
            let mut info: MONITORINFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            if GetMonitorInfoW(monitor, &mut info) == 0 {
                return;
            }
            let work = info.rcWork;
            let height = rect.bottom - rect.top;
            let width = rect.right - rect.left;
            let y = rect.top.min(work.bottom - height).max(work.top);
            let x = rect.left.min(work.right - width).max(work.left);
            if (x, y) != (rect.left, rect.top) {
                SetWindowPos(
                    self.handle(),
                    HWND_TOPMOST,
                    x,
                    y,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
    }

    fn set_no_activate(&self, enabled: bool) {
        unsafe {
            let style = GetWindowLongPtrW(self.handle(), GWL_EXSTYLE);
            let style = if enabled {
                style | WS_EX_NOACTIVATE as isize
            } else {
                style & !(WS_EX_NOACTIVATE as isize)
            };
            SetWindowLongPtrW(self.handle(), GWL_EXSTYLE, style);
        }
    }
}

/// Brings a window to the foreground, attaching to the current foreground
/// thread's input when Windows' focus-stealing rules would refuse it.
unsafe fn force_foreground(target: HWND) {
    unsafe {
        if SetForegroundWindow(target) != 0 {
            return;
        }
        let foreground = GetForegroundWindow();
        let foreground_thread = GetWindowThreadProcessId(foreground, std::ptr::null_mut());
        let current_thread = GetCurrentThreadId();
        if foreground_thread != 0 && foreground_thread != current_thread {
            AttachThreadInput(current_thread, foreground_thread, 1);
            BringWindowToTop(target);
            SetForegroundWindow(target);
            SetFocus(target);
            AttachThreadInput(current_thread, foreground_thread, 0);
        }
    }
}

impl CopilotWindowHost for Win32CopilotHost {
    fn resize(&self, width: f32, height: f32) {
        let scale = self.scale();
        let width = (width * scale).round() as i32;
        let height = (height * scale).round() as i32;
        if !self.placed.replace(true) {
            self.place(width, height);
            return;
        }
        unsafe {
            SetWindowPos(
                self.handle(),
                HWND_TOPMOST,
                0,
                0,
                width,
                height,
                SWP_NOMOVE | SWP_NOACTIVATE,
            );
        }
        // The window keeps wherever the user dragged it.
        self.keep_on_screen();
    }

    fn set_visible(&self, visible: bool) {
        tracing::info!(visible, "copilot window visibility changed");
        unsafe {
            if visible {
                ShowWindow(self.handle(), SW_SHOWNOACTIVATE);
                SetWindowPos(
                    self.handle(),
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            } else {
                ShowWindow(self.handle(), SW_HIDE);
            }
        }
    }

    fn begin_typing(&self) {
        unsafe {
            let foreground = GetForegroundWindow();
            if !foreground.is_null() && foreground != self.handle() {
                self.previous_foreground.set(foreground as usize);
            }
            self.set_no_activate(false);
            force_foreground(self.handle());
        }
    }

    fn end_typing(&self, restore_focus: bool) {
        self.set_no_activate(true);
        let previous = self.previous_foreground.replace(0) as HWND;
        unsafe {
            if restore_focus
                && !previous.is_null()
                && IsWindow(previous) != 0
                && GetForegroundWindow() == self.handle()
            {
                force_foreground(previous);
            }
        }
    }

    fn set_shape(&self, shapes: &[CopilotShape]) {
        if shapes.is_empty() {
            return;
        }
        let scale = self.scale();
        let at = |value: f32| (value * scale).round() as i32;
        unsafe {
            let region = CreateRectRgn(0, 0, 0, 0);
            if region.is_null() {
                return;
            }
            for shape in shapes {
                let diameter = at(shape.radius * 2.0);
                // Round-rect regions exclude their right and bottom edges.
                let part = CreateRoundRectRgn(
                    at(shape.x),
                    at(shape.y),
                    at(shape.x + shape.width) + 1,
                    at(shape.y + shape.height) + 1,
                    diameter,
                    diameter,
                );
                if !part.is_null() {
                    CombineRgn(region, region, part, RGN_OR);
                    DeleteObject(part);
                }
            }
            // On success the system owns the region.
            if SetWindowRgn(self.handle(), region, 1) == 0 {
                DeleteObject(region);
                tracing::warn!(
                    error = %std::io::Error::last_os_error(),
                    "copilot window shape could not be applied"
                );
            }
        }
    }

    fn set_capture_excluded(&self, excluded: bool) {
        let affinity = if excluded {
            WDA_EXCLUDEFROMCAPTURE
        } else {
            WDA_NONE
        };
        unsafe {
            if SetWindowDisplayAffinity(self.handle(), affinity) == 0 {
                tracing::warn!(
                    error = %std::io::Error::last_os_error(),
                    excluded,
                    "copilot screen-capture visibility could not be changed"
                );
            } else {
                tracing::info!(excluded, "copilot screen-capture visibility applied");
            }
        }
    }
}
