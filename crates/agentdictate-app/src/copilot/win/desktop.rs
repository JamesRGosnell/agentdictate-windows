//! Screen capture, clipboard, and shell helpers for the copilot.

use std::io::Cursor;

use image::{ImageBuffer, Rgb, codecs::jpeg::JpegEncoder, imageops::FilterType};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    System::{DataExchange::*, Memory::*},
    UI::{Shell::ShellExecuteW, WindowsAndMessaging::*},
};

/// Longest edge sent to models; larger screenshots only add latency.
const MAX_EDGE: u32 = 1_568;

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

/// Captures the primary monitor as a JPEG. Windows marked as excluded from
/// capture, including the copilot itself, are omitted by the compositor.
pub(crate) fn capture_primary_jpeg() -> Result<Vec<u8>, String> {
    let (width, height, left, top, pixels) = unsafe { capture_primary_bgra()? };
    let mut rgb = Vec::with_capacity((width * height * 3) as usize);
    for pixel in pixels.chunks_exact(4) {
        rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
    }
    let image: ImageBuffer<Rgb<u8>, Vec<u8>> =
        ImageBuffer::from_raw(width, height, rgb).ok_or("screen capture was incomplete")?;
    let scale = (MAX_EDGE as f32 / width.max(height) as f32).min(1.0);
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
        left,
        top,
        width,
        height,
        bytes = jpeg.get_ref().len(),
        "screen captured"
    );
    Ok(jpeg.into_inner())
}

unsafe fn capture_primary_bgra() -> Result<(u32, u32, i32, i32, Vec<u8>), String> {
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
        let screen = GetDC(std::ptr::null_mut());
        if screen.is_null() {
            return Err("screen device context is unavailable".into());
        }
        let memory = CreateCompatibleDC(screen);
        let bitmap = CreateCompatibleBitmap(screen, width, height);
        let previous = SelectObject(memory, bitmap as HGDIOBJ);
        let copied = BitBlt(
            memory,
            0,
            0,
            width,
            height,
            screen,
            left,
            top,
            SRCCOPY | CAPTUREBLT,
        );
        let mut header: BITMAPINFO = std::mem::zeroed();
        header.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        header.bmiHeader.biWidth = width;
        header.bmiHeader.biHeight = -height;
        header.bmiHeader.biPlanes = 1;
        header.bmiHeader.biBitCount = 32;
        header.bmiHeader.biCompression = BI_RGB;
        let mut pixels = vec![0_u8; (width * height * 4) as usize];
        let lines = if copied != 0 {
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
        Ok((width as u32, height as u32, left, top, pixels))
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
