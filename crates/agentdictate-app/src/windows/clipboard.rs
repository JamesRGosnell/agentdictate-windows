//! Clipboard handling for a paste: the user's clipboard is saved before the
//! transcript is written and given back once the target has read the paste,
//! unless the user copied something else in the meantime.
use agentdictate_runtime::{DeliveryDisposition, ExternalError};
use std::time::{Duration, Instant};
use windows_sys::Win32::{
    Foundation::*,
    System::{DataExchange::*, Memory::*},
    UI::WindowsAndMessaging::*,
};

/// How long the target application gets to read the clipboard after the
/// paste keys before the user's clipboard is given back. Applications read
/// it asynchronously while handling Ctrl+V.
pub(super) const RESTORE_DELAY: Duration = Duration::from_millis(500);
/// Larger clipboards are not saved, so the transcript simply stays.
const SNAPSHOT_SIZE_LIMIT: usize = 64 * 1024 * 1024;
/// Saving must not delay the paste noticeably; an owner that renders its
/// formats slowly leaves the transcript on the clipboard instead.
const SNAPSHOT_TIME_LIMIT: Duration = Duration::from_millis(300);

/// The clipboard operations one delivery needs.
pub(super) trait Clipboard {
    type Snapshot: Send + 'static;
    /// The current contents, or `None` when they are too large, too slow, or
    /// cannot be duplicated faithfully.
    fn snapshot(&mut self) -> Option<Self::Snapshot>;
    /// Replaces the contents with `text`. A `private` text is kept out of
    /// Windows clipboard history and cloud sync.
    fn write_text(&mut self, text: &str, private: bool) -> Result<(), ExternalError>;
    /// Changes whenever anyone writes the clipboard.
    fn sequence_number(&self) -> u32;
    /// Puts `snapshot` back, unless the clipboard changed after `expected`
    /// (checked again while the clipboard is held). Returns whether it did.
    fn restore(&mut self, snapshot: &Self::Snapshot, expected: u32) -> Result<bool, ExternalError>;
}

/// The transcript written over a saved clipboard.
pub(super) struct StagedText<S> {
    previous: Option<S>,
    /// The sequence number right after the transcript was written.
    sequence: u32,
}

/// Saves the clipboard and writes the transcript, marked private only when
/// the clipboard will be given back.
pub(super) fn stage_text<C: Clipboard>(
    clipboard: &mut C,
    text: &str,
) -> Result<StagedText<C::Snapshot>, ExternalError> {
    let previous = clipboard.snapshot();
    clipboard.write_text(text, previous.is_some())?;
    Ok(StagedText {
        previous,
        sequence: clipboard.sequence_number(),
    })
}

/// Only a paste whose keys all went out hands the clipboard back. Every
/// other outcome leaves the transcript there for the user to paste.
pub(super) fn restores_after(disposition: &DeliveryDisposition) -> bool {
    matches!(
        disposition,
        DeliveryDisposition::Submitted {
            paste_triggered: true,
            ..
        }
    )
}

/// A paste's outcome, and the clipboard to give back afterwards, if any.
pub(super) type StagedPaste<S> = (DeliveryDisposition, Option<StagedText<S>>);

/// Writes the transcript over a saved clipboard and runs `paste`. Returns
/// its outcome and, when the clipboard should be given back, what to give.
pub(super) fn paste_staged<C: Clipboard>(
    clipboard: &mut C,
    text: &str,
    paste: impl FnOnce() -> DeliveryDisposition,
) -> Result<StagedPaste<C::Snapshot>, ExternalError> {
    let staged = stage_text(clipboard, text)?;
    let disposition = paste();
    let restore = (restores_after(&disposition) && staged.previous.is_some()).then_some(staged);
    Ok((disposition, restore))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Restore {
    Restored,
    /// Someone wrote the clipboard after the transcript; theirs stays.
    Replaced,
    NothingSaved,
}

/// Gives the saved clipboard back unless someone wrote the clipboard after
/// the transcript.
pub(super) fn restore_unless_replaced<C: Clipboard>(
    clipboard: &mut C,
    staged: StagedText<C::Snapshot>,
) -> Result<Restore, ExternalError> {
    let Some(previous) = staged.previous else {
        return Ok(Restore::NothingSaved);
    };
    if clipboard.sequence_number() != staged.sequence {
        return Ok(Restore::Replaced);
    }
    Ok(if clipboard.restore(&previous, staged.sequence)? {
        Restore::Restored
    } else {
        Restore::Replaced
    })
}

/// How a clipboard format is saved.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FormatCopy {
    Copy,
    /// One of a set Windows converts between; only the first is kept and
    /// Windows synthesizes the rest again after the restore.
    Family(FormatFamily),
    /// A GDI handle, owner-drawn or private format, or OLE bookkeeping
    /// that cannot be duplicated as plain memory.
    Skip,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FormatFamily {
    Text,
    Bitmap,
}
const CF_TEXT_FORMAT: u32 = 1;
const CF_OEMTEXT_FORMAT: u32 = 7;
const CF_DIB_FORMAT: u32 = 8;
const CF_UNICODETEXT_FORMAT: u32 = 13;
const CF_DIBV5_FORMAT: u32 = 17;
fn format_copy(format: u32, name: Option<&str>) -> FormatCopy {
    match format {
        CF_TEXT_FORMAT | CF_OEMTEXT_FORMAT | CF_UNICODETEXT_FORMAT => {
            FormatCopy::Family(FormatFamily::Text)
        }
        CF_DIB_FORMAT | CF_DIBV5_FORMAT => FormatCopy::Family(FormatFamily::Bitmap),
        // SYLK, DIF, TIFF, pen data, RIFF, WAVE, HDROP, locale.
        4 | 5 | 6 | 10 | 11 | 12 | 15 | 16 => FormatCopy::Copy,
        0xC000..=0xFFFF => match name {
            Some(name) if name == "DataObject" || name.starts_with("Ole ") => FormatCopy::Skip,
            Some(_) => FormatCopy::Copy,
            None => FormatCopy::Skip,
        },
        // Bitmap, metafile, palette, enhanced metafile, owner display and
        // private formats hold handles Windows cannot copy as memory.
        _ => FormatCopy::Skip,
    }
}

const EXCLUDE_FROM_MONITORS: &str = "ExcludeClipboardContentFromMonitorProcessing";
const HISTORY: &str = "CanIncludeInClipboardHistory";
const CLOUD: &str = "CanUploadToCloudClipboard";

pub(super) struct ClipboardGuard(HWND);
impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: the guard exists only while this thread holds the
        // clipboard open with its own message-only owner window.
        unsafe {
            CloseClipboard();
            DestroyWindow(self.0);
        }
    }
}
pub(super) fn open_clipboard() -> Result<ClipboardGuard, ExternalError> {
    let class: Vec<u16> = "STATIC".encode_utf16().chain(Some(0)).collect();
    // SAFETY: creates a message-only window owned by this thread, destroyed
    // by the guard or on the timeout path.
    let owner = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            std::ptr::null(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
        )
    };
    if owner.is_null() {
        return Err(ExternalError::new(
            "Could not create clipboard owner window",
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if unsafe { OpenClipboard(owner) } != 0 {
            return Ok(ClipboardGuard(owner));
        }
        if Instant::now() >= deadline {
            unsafe {
                DestroyWindow(owner);
            }
            return Err(ExternalError::new("Windows clipboard is busy"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
fn registered_format(name: &str) -> Option<u32> {
    // SAFETY: the name is a NUL-terminated UTF-16 string.
    let format = unsafe { RegisterClipboardFormatW(wide(name).as_ptr()) };
    (format != 0).then_some(format)
}
fn format_name(format: u32) -> Option<String> {
    let mut name = [0u16; 256];
    // SAFETY: the buffer length passed matches the buffer.
    let length = unsafe { GetClipboardFormatNameW(format, name.as_mut_ptr(), name.len() as i32) };
    (length > 0).then(|| String::from_utf16_lossy(&name[..length as usize]))
}
/// Publishes a copy of `bytes` as `format`.
///
/// # Safety
/// The caller must hold the clipboard open and have emptied it.
unsafe fn set_bytes(format: u32, bytes: &[u8]) -> bool {
    // SAFETY: Windows takes ownership of the allocation only after a
    // successful SetClipboardData; every other path frees it here.
    unsafe {
        let allocation = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1));
        if allocation.is_null() {
            return false;
        }
        let destination = GlobalLock(allocation);
        if destination.is_null() {
            GlobalFree(allocation);
            return false;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), destination.cast::<u8>(), bytes.len());
        GlobalUnlock(allocation);
        if SetClipboardData(format, allocation).is_null() {
            GlobalFree(allocation);
            return false;
        }
        true
    }
}
/// Marks the contents being published as private to history and cloud.
///
/// # Safety
/// The caller must hold the clipboard open and have emptied it.
unsafe fn mark_private(names: &[&str]) {
    for name in names {
        let marked = registered_format(name)
            // SAFETY: forwarded from this function's contract.
            .is_some_and(|format| unsafe { set_bytes(format, &0u32.to_ne_bytes()) });
        if !marked {
            tracing::warn!(format = name, "could not mark the clipboard private");
        }
    }
}

/// Saved clipboard formats, in the order they were offered.
pub(super) struct Win32Snapshot {
    formats: Vec<(u32, Vec<u8>)>,
}

/// The real Windows clipboard.
pub(super) struct Win32Clipboard;
impl Clipboard for Win32Clipboard {
    type Snapshot = Win32Snapshot;
    fn snapshot(&mut self) -> Option<Win32Snapshot> {
        let started = Instant::now();
        let _guard = open_clipboard().ok()?;
        let mut formats = Vec::new();
        let mut families = Vec::new();
        let mut skipped = false;
        let mut total = 0usize;
        let mut format = 0;
        loop {
            // SAFETY: the clipboard is open on this thread; every handle
            // read stays owned by the clipboard and is only locked while
            // its bytes are copied out.
            unsafe {
                format = EnumClipboardFormats(format);
                if format == 0 {
                    if GetLastError() != ERROR_SUCCESS {
                        return None;
                    }
                    break;
                }
                let name = if format >= 0xC000 {
                    format_name(format)
                } else {
                    None
                };
                match format_copy(format, name.as_deref()) {
                    FormatCopy::Skip => {
                        skipped = true;
                        continue;
                    }
                    FormatCopy::Family(family) if families.contains(&family) => continue,
                    FormatCopy::Family(family) => families.push(family),
                    FormatCopy::Copy => {}
                }
                let handle = GetClipboardData(format);
                let size = if handle.is_null() {
                    0
                } else {
                    GlobalSize(handle)
                };
                if size == 0 {
                    skipped = true;
                    continue;
                }
                total += size;
                if total > SNAPSHOT_SIZE_LIMIT {
                    tracing::info!(
                        total,
                        "clipboard too large to save; it will not be restored"
                    );
                    return None;
                }
                let contents = GlobalLock(handle);
                if contents.is_null() {
                    skipped = true;
                    continue;
                }
                let bytes = std::slice::from_raw_parts(contents.cast::<u8>(), size).to_vec();
                GlobalUnlock(handle);
                formats.push((format, bytes));
            }
            if started.elapsed() > SNAPSHOT_TIME_LIMIT {
                tracing::info!("clipboard too slow to save; it will not be restored");
                return None;
            }
        }
        // Contents that cannot be duplicated at all must not be replaced by
        // an empty clipboard later.
        if formats.is_empty() && skipped {
            return None;
        }
        Some(Win32Snapshot { formats })
    }
    fn write_text(&mut self, text: &str, private: bool) -> Result<(), ExternalError> {
        let text: Vec<u16> = wide(text);
        let bytes: Vec<u8> = text.iter().flat_map(|unit| unit.to_ne_bytes()).collect();
        let _guard = open_clipboard()?;
        // SAFETY: the clipboard is open on this thread and emptied before
        // publishing; the readback handle stays owned by the clipboard.
        unsafe {
            if EmptyClipboard() == 0 || !set_bytes(CF_UNICODETEXT_FORMAT, &bytes) {
                return Err(ExternalError::new(format!(
                    "clipboard publication failed: {}",
                    std::io::Error::last_os_error()
                )));
            }
            if private {
                mark_private(&[EXCLUDE_FROM_MONITORS, HISTORY, CLOUD]);
            }
            let handle = GetClipboardData(CF_UNICODETEXT_FORMAT);
            if handle.is_null() {
                return Err(ExternalError::new("clipboard readback failed"));
            }
            let contents = GlobalLock(handle);
            if contents.is_null() {
                return Err(ExternalError::new("clipboard readback lock failed"));
            }
            let matches = GlobalSize(handle) >= text.len() * 2
                && std::slice::from_raw_parts(contents.cast::<u16>(), text.len())
                    == text.as_slice();
            GlobalUnlock(handle);
            if !matches {
                return Err(ExternalError::new(
                    "clipboard readback differs from transcript",
                ));
            }
        }
        Ok(())
    }
    fn sequence_number(&self) -> u32 {
        // SAFETY: a plain query with no arguments.
        unsafe { GetClipboardSequenceNumber() }
    }
    fn restore(&mut self, snapshot: &Win32Snapshot, expected: u32) -> Result<bool, ExternalError> {
        let _guard = open_clipboard()?;
        // SAFETY: the clipboard is open on this thread, so nobody else can
        // write it between the sequence check and the restore.
        unsafe {
            if GetClipboardSequenceNumber() != expected {
                return Ok(false);
            }
            if EmptyClipboard() == 0 {
                return Err(ExternalError::new(format!(
                    "could not empty the clipboard: {}",
                    std::io::Error::last_os_error()
                )));
            }
            for (format, bytes) in &snapshot.formats {
                if !set_bytes(*format, bytes) {
                    tracing::warn!(format, "could not restore a clipboard format");
                }
            }
            // The restored contents are already in clipboard history once;
            // do not add them again, unless their owner said otherwise.
            let marked = |name: &str| {
                registered_format(name)
                    .is_some_and(|format| snapshot.formats.iter().any(|(f, _)| *f == format))
            };
            let unmarked: Vec<&str> = [HISTORY, CLOUD]
                .into_iter()
                .filter(|name| !marked(name))
                .collect();
            mark_private(&unmarked);
        }
        Ok(true)
    }
}

/// Gives the user's clipboard back after the target had time to read the
/// paste, without delaying the delivery itself.
pub(super) fn restore_later(staged: StagedText<Win32Snapshot>) {
    let spawned = std::thread::Builder::new()
        .name("agentdictate-clipboard-restore".into())
        .spawn(move || {
            std::thread::sleep(RESTORE_DELAY);
            match restore_unless_replaced(&mut Win32Clipboard, staged) {
                Ok(outcome) => tracing::debug!(?outcome, "clipboard restore after paste"),
                Err(error) => tracing::warn!(%error, "could not restore the clipboard"),
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "could not schedule the clipboard restore");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clipboard holding one text, with Windows' sequence counter.
    struct FakeClipboard {
        contents: String,
        private: bool,
        sequence: u32,
        saveable: bool,
    }
    impl FakeClipboard {
        fn holding(contents: &str) -> Self {
            Self {
                contents: contents.into(),
                private: false,
                sequence: 7,
                saveable: true,
            }
        }
        /// The user copies something.
        fn copy(&mut self, contents: &str) {
            self.contents = contents.into();
            self.private = false;
            self.sequence += 1;
        }
    }
    impl Clipboard for FakeClipboard {
        type Snapshot = String;
        fn snapshot(&mut self) -> Option<String> {
            self.saveable.then(|| self.contents.clone())
        }
        fn write_text(&mut self, text: &str, private: bool) -> Result<(), ExternalError> {
            self.copy(text);
            self.private = private;
            Ok(())
        }
        fn sequence_number(&self) -> u32 {
            self.sequence
        }
        fn restore(&mut self, snapshot: &String, expected: u32) -> Result<bool, ExternalError> {
            if self.sequence != expected {
                return Ok(false);
            }
            self.copy(snapshot);
            Ok(true)
        }
    }
    const PASTED: DeliveryDisposition = DeliveryDisposition::Submitted {
        copied_to_clipboard: true,
        paste_triggered: true,
    };

    #[test]
    fn a_paste_reads_the_private_transcript_and_then_gives_the_clipboard_back() {
        let mut clipboard = FakeClipboard::holding("the user's link");
        let (disposition, staged) =
            paste_staged(&mut clipboard, "Dictated text.", || PASTED).unwrap();
        assert_eq!(disposition, PASTED);
        // What the target reads during the restore delay.
        assert_eq!(clipboard.contents, "Dictated text.");
        assert!(
            clipboard.private,
            "a restored transcript stays out of history"
        );
        let staged = staged.expect("a full paste restores the clipboard");
        assert_eq!(
            restore_unless_replaced(&mut clipboard, staged).unwrap(),
            Restore::Restored
        );
        assert_eq!(clipboard.contents, "the user's link");
    }

    #[test]
    fn a_copy_made_after_the_paste_is_never_overwritten() {
        let mut clipboard = FakeClipboard::holding("old");
        let (_, staged) = paste_staged(&mut clipboard, "Dictated text.", || PASTED).unwrap();
        clipboard.copy("copied during the wait");
        assert_eq!(
            restore_unless_replaced(&mut clipboard, staged.unwrap()).unwrap(),
            Restore::Replaced
        );
        assert_eq!(clipboard.contents, "copied during the wait");
    }

    #[test]
    fn a_clipboard_that_could_not_be_saved_keeps_the_transcript_in_history() {
        let mut clipboard = FakeClipboard::holding("too large");
        clipboard.saveable = false;
        let (_, staged) = paste_staged(&mut clipboard, "Dictated text.", || PASTED).unwrap();
        assert!(staged.is_none());
        assert_eq!(clipboard.contents, "Dictated text.");
        assert!(!clipboard.private);
    }

    #[test]
    fn the_transcript_stays_whenever_the_user_may_still_need_to_paste_it() {
        for disposition in [
            DeliveryDisposition::Submitted {
                copied_to_clipboard: true,
                paste_triggered: false,
            },
            DeliveryDisposition::Ambiguous {
                copied_to_clipboard: true,
            },
            DeliveryDisposition::NotSent {
                copied_to_clipboard: true,
                reason: "focus changed".into(),
            },
        ] {
            let mut clipboard = FakeClipboard::holding("old");
            let outcome = disposition.clone();
            let (_, staged) =
                paste_staged(&mut clipboard, "Dictated text.", move || outcome).unwrap();
            assert!(staged.is_none(), "{disposition:?}");
            assert_eq!(clipboard.contents, "Dictated text.");
        }
        assert!(restores_after(&PASTED));
    }

    #[test]
    fn only_memory_formats_are_saved_and_converted_families_once() {
        use FormatCopy::{Copy, Family, Skip};
        for (format, name, expected) in [
            (CF_UNICODETEXT_FORMAT, None, Family(FormatFamily::Text)),
            (CF_TEXT_FORMAT, None, Family(FormatFamily::Text)),
            (CF_DIB_FORMAT, None, Family(FormatFamily::Bitmap)),
            (CF_DIBV5_FORMAT, None, Family(FormatFamily::Bitmap)),
            (15, None, Copy), // CF_HDROP
            (16, None, Copy), // CF_LOCALE
            (2, None, Skip),  // CF_BITMAP
            (3, None, Skip),  // CF_METAFILEPICT
            (14, None, Skip), // CF_ENHMETAFILE
            (0x80, None, Skip),
            (0x200, None, Skip),
            (0x300, None, Skip),
            (0xC100, Some("HTML Format"), Copy),
            (0xC101, Some("Rich Text Format"), Copy),
            (0xC102, Some("DataObject"), Skip),
            (0xC103, Some("Ole Private Data"), Skip),
            (0xC104, None, Skip),
        ] {
            assert_eq!(format_copy(format, name), expected, "{format:#x} {name:?}");
        }
    }
}
