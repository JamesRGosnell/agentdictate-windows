//! Screen capture, clipboard, and shell helpers for the copilot.

use std::{io::Cursor, sync::Arc};

use image::{ImageBuffer, Rgb, codecs::jpeg::JpegEncoder, imageops::FilterType};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::{
        Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute},
        Gdi::*,
    },
    Storage::Xps::PrintWindow,
    System::{DataExchange::*, Memory::*},
    UI::{Shell::ShellExecuteW, WindowsAndMessaging::*},
};

use super::super::{engine::CallShot, meeting::is_meeting_title};

/// Longest edge sent to models; larger screenshots only add latency.
const MAX_EDGE: u32 = 1_568;
/// Longest edge of meeting-window captures, which keeps the small names on
/// video tiles legible.
const CALL_MAX_EDGE: u32 = 2_048;

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

/// Captures the primary monitor as a JPEG. Windows marked as excluded from
/// capture, including the copilot itself, are omitted by the compositor.
pub(crate) fn capture_primary_jpeg() -> Result<Vec<u8>, String> {
    let (width, height, pixels) = unsafe { capture_primary_bgra()? };
    encode_jpeg(width, height, &pixels, MAX_EDGE)
}

/// Captures the meeting app's window as a JPEG, even when other windows
/// cover it; the primary monitor when no meeting window is open or it
/// cannot be drawn.
pub(crate) fn capture_call_window() -> Result<CallShot, String> {
    let meeting = find_meeting_window();
    if let Some((window, title)) = &meeting {
        match unsafe { capture_window_bgra(*window) } {
            Ok((width, height, pixels)) if !looks_blank(&pixels) => {
                return Ok(CallShot {
                    title: Some(title.clone()),
                    jpeg: Arc::new(encode_jpeg(width, height, &pixels, CALL_MAX_EDGE)?),
                });
            }
            Ok(_) => tracing::debug!("meeting window drew blank; capturing the screen"),
            Err(error) => tracing::debug!(%error, "meeting window capture failed"),
        }
    }
    let (width, height, pixels) = unsafe { capture_primary_bgra()? };
    Ok(CallShot {
        title: meeting.map(|(_, title)| title),
        jpeg: Arc::new(encode_jpeg(width, height, &pixels, CALL_MAX_EDGE)?),
    })
}

/// Whether a capture is a single flat color, as windows that cannot be
/// drawn off screen come out.
fn looks_blank(pixels: &[u8]) -> bool {
    let mut samples = pixels.chunks_exact(4).step_by(97);
    let Some(first) = samples.next() else {
        return true;
    };
    samples.all(|pixel| {
        pixel[..3]
            .iter()
            .zip(&first[..3])
            .all(|(value, base)| value.abs_diff(*base) < 8)
    })
}

fn encode_jpeg(width: u32, height: u32, pixels: &[u8], max_edge: u32) -> Result<Vec<u8>, String> {
    let mut rgb = Vec::with_capacity((width * height * 3) as usize);
    for pixel in pixels.chunks_exact(4) {
        rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
    }
    let image: ImageBuffer<Rgb<u8>, Vec<u8>> =
        ImageBuffer::from_raw(width, height, rgb).ok_or("screen capture was incomplete")?;
    let scale = (max_edge as f32 / width.max(height) as f32).min(1.0);
    let image = if scale < 1.0 {
        image::imageops::resize(
            &image,
            ((width as f32 * scale).round() as u32).max(1),
            ((height as f32 * scale).round() as u32).max(1),
            FilterType::Triangle,
        )
    } else {
        image
    };
    let mut jpeg = Cursor::new(Vec::new());
    JpegEncoder::new_with_quality(&mut jpeg, 78)
        .encode_image(&image)
        .map_err(|error| error.to_string())?;
    tracing::debug!(
        width,
        height,
        bytes = jpeg.get_ref().len(),
        "screen captured"
    );
    Ok(jpeg.into_inner())
}

unsafe fn capture_primary_bgra() -> Result<(u32, u32, Vec<u8>), String> {
    unsafe {
        let monitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(monitor, &mut info) == 0 {
            return Err("primary monitor is unavailable".into());
        }
        let left = info.rcMonitor.left;
        let top = info.rcMonitor.top;
        let width = (info.rcMonitor.right - left).max(1);
        let height = (info.rcMonitor.bottom - top).max(1);
        capture_bgra(width, height, |memory, screen| {
            BitBlt(
                memory,
                0,
                0,
                width,
                height,
                screen,
                left,
                top,
                SRCCOPY | CAPTUREBLT,
            ) != 0
        })
    }
}

/// Draws a window into a bitmap, as the compositor has it, whether or not
/// other windows cover it.
unsafe fn capture_window_bgra(window: HWND) -> Result<(u32, u32, Vec<u8>), String> {
    unsafe {
        let mut rect: RECT = std::mem::zeroed();
        if GetWindowRect(window, &mut rect) == 0 {
            return Err("meeting window bounds are unavailable".into());
        }
        let width = (rect.right - rect.left).max(1);
        let height = (rect.bottom - rect.top).max(1);
        capture_bgra(width, height, |memory, _| {
            PrintWindow(window, memory, PW_RENDERFULLCONTENT) != 0
        })
    }
}

/// Runs `draw` into a screen-compatible bitmap and reads it back as
/// top-down BGRA pixels.
unsafe fn capture_bgra(
    width: i32,
    height: i32,
    draw: impl FnOnce(HDC, HDC) -> bool,
) -> Result<(u32, u32, Vec<u8>), String> {
    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        if screen.is_null() {
            return Err("screen device context is unavailable".into());
        }
        let memory = CreateCompatibleDC(screen);
        let bitmap = CreateCompatibleBitmap(screen, width, height);
        let previous = SelectObject(memory, bitmap as HGDIOBJ);
        let copied = draw(memory, screen);
        let mut header: BITMAPINFO = std::mem::zeroed();
        header.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        header.bmiHeader.biWidth = width;
        header.bmiHeader.biHeight = -height;
        header.bmiHeader.biPlanes = 1;
        header.bmiHeader.biBitCount = 32;
        header.bmiHeader.biCompression = BI_RGB;
        let mut pixels = vec![0_u8; (width * height * 4) as usize];
        let lines = if copied {
            GetDIBits(
                memory,
                bitmap,
                0,
                height as u32,
                pixels.as_mut_ptr().cast(),
                &mut header,
                DIB_RGB_COLORS,
            )
        } else {
            0
        };
        SelectObject(memory, previous);
        DeleteObject(bitmap as HGDIOBJ);
        DeleteDC(memory);
        ReleaseDC(std::ptr::null_mut(), screen);
        if lines == 0 {
            return Err("the screen could not be captured".into());
        }
        Ok((width as u32, height as u32, pixels))
    }
}

/// The largest visible window whose title says a meeting is in progress.
fn find_meeting_window() -> Option<(HWND, String)> {
    let mut found: Option<(HWND, String, i64)> = None;
    unsafe {
        EnumWindows(Some(collect_meeting_window), (&raw mut found) as LPARAM);
    }
    found.map(|(window, title, _)| (window, title))
}

unsafe extern "system" fn collect_meeting_window(
    window: HWND,
    state: LPARAM,
) -> windows_sys::core::BOOL {
    unsafe {
        let found = &mut *(state as *mut Option<(HWND, String, i64)>);
        if IsWindowVisible(window) == 0 || IsIconic(window) != 0 {
            return 1;
        }
        // Hidden UWP and WebView windows are cloaked, not invisible.
        let mut cloaked: u32 = 0;
        if DwmGetWindowAttribute(
            window,
            DWMWA_CLOAKED as u32,
            (&raw mut cloaked).cast(),
            std::mem::size_of::<u32>() as u32,
        ) == 0
            && cloaked != 0
        {
            return 1;
        }
        let length = GetWindowTextLengthW(window);
        if length <= 0 {
            return 1;
        }
        let mut buffer = vec![0_u16; length as usize + 1];
        let copied = GetWindowTextW(window, buffer.as_mut_ptr(), buffer.len() as i32);
        let title = String::from_utf16_lossy(&buffer[..copied.max(0) as usize]);
        if !is_meeting_title(&title) {
            return 1;
        }
        let mut rect: RECT = std::mem::zeroed();
        if GetWindowRect(window, &mut rect) == 0 {
            return 1;
        }
        let area = i64::from(rect.right - rect.left) * i64::from(rect.bottom - rect.top);
        if area > 0 && found.as_ref().is_none_or(|(_, _, best)| area > *best) {
            *found = Some((window, title, area));
        }
        1
    }
}

/// Places Unicode text on the clipboard.
pub(crate) fn copy_text(text: &str) {
    let units: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return;
        }
        EmptyClipboard();
        let bytes = units.len() * 2;
        let memory = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if !memory.is_null() {
            let target = GlobalLock(memory);
            if !target.is_null() {
                std::ptr::copy_nonoverlapping(
                    units.as_ptr().cast::<u8>(),
                    target.cast::<u8>(),
                    bytes,
                );
                GlobalUnlock(memory);
                // CF_UNICODETEXT; the clipboard owns the memory on success.
                if SetClipboardData(13, memory as HANDLE).is_null() {
                    GlobalFree(memory);
                }
            } else {
                GlobalFree(memory);
            }
        }
        CloseClipboard();
    }
}

/// Opens a folder, file, or URL with its default handler.
pub(crate) fn open(target: &str) {
    let operation = wide("open");
    let target = wide(target);
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}
