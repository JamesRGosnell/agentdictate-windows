use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    UI::{HiDpi::*, WindowsAndMessaging::*},
};
pub struct OverlayPlacementWatcher {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
fn place(window: usize, metrics: [u32; 3]) -> io::Result<()> {
    unsafe {
        let hwnd = window as HWND;
        let monitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(monitor, &mut info) == 0 {
            return Err(io::Error::last_os_error());
        }
        let scale = (GetDpiForWindow(hwnd).max(96) as f32) / 96.;
        let width = (metrics[0] as f32 * scale).round() as i32;
        let height = (metrics[1] as f32 * scale).round() as i32;
        let gap = (metrics[2] as f32 * scale).round() as i32;
        let x = info.rcWork.left + (info.rcWork.right - info.rcWork.left - width) / 2;
        let y = info.rcWork.bottom - height - gap;
        let mut current = std::mem::zeroed();
        GetWindowRect(hwnd, &mut current);
        if (current.left != x
            || current.top != y
            || current.right - current.left != width
            || current.bottom - current.top != height)
            && SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                x,
                y,
                width,
                height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
impl OverlayPlacementWatcher {
    pub fn start(
        window: usize,
        _scale: f32,
        metrics: [u32; 3],
        on_error: impl Fn(io::Error) + Send + 'static,
    ) -> io::Result<Self> {
        unsafe {
            let hwnd = window as HWND;
            let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(
                hwnd,
                GWL_EXSTYLE,
                style
                    | WS_EX_NOACTIVATE as isize
                    | WS_EX_TOOLWINDOW as isize
                    | WS_EX_TRANSPARENT as isize,
            );
        }
        place(window, metrics)?;
        let stop = Arc::new(AtomicBool::new(false));
        let control = stop.clone();
        let worker = std::thread::spawn(move || {
            while !control.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(250));
                if unsafe { IsWindow(window as HWND) } == 0 {
                    return;
                }
                if let Err(error) = place(window, metrics) {
                    on_error(error);
                    return;
                }
            }
        });
        Ok(Self {
            stop,
            worker: Some(worker),
        })
    }
}
impl Drop for OverlayPlacementWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
