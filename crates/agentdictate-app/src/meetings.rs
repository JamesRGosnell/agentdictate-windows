//! Saved meeting notes, read for the Meetings page in the settings window.

use std::path::{Path, PathBuf};

use agentdictate_core::{EmailDraft, parse_meeting_notes};
use agentdictate_ui::{MeetingDetailViewModel, MeetingNoteViewModel};

/// Saved meetings, newest first. Notes files start with their date and time,
/// so names sort chronologically.
pub(crate) fn scan(folder: &Path) -> Vec<MeetingNoteViewModel> {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| is_notes_file(name))
        .collect();
    names.sort_unstable_by(|left, right| right.cmp(left));
    names
        .into_iter()
        .filter_map(|name| {
            let markdown = std::fs::read_to_string(folder.join(&name)).ok()?;
            let mut notes = parse_meeting_notes(&markdown);
            if notes.title.is_empty() {
                notes.title = name.trim_end_matches(".md").to_owned();
            }
            Some(MeetingNoteViewModel::from_notes(name, &notes).with_search_text(&markdown))
        })
        .collect()
}

/// One saved meeting's notes, or `None` when the file is gone.
pub(crate) fn open(folder: &Path, id: &str) -> Option<MeetingDetailViewModel> {
    let markdown = std::fs::read_to_string(note_path(folder, id)?).ok()?;
    Some(MeetingDetailViewModel::from_notes(
        id,
        parse_meeting_notes(&markdown),
    ))
}

/// A saved meeting's follow-up email as a draft, or `None` when the meeting
/// has none.
pub(crate) fn email_draft(folder: &Path, id: &str) -> Option<EmailDraft> {
    let notes = parse_meeting_notes(&std::fs::read_to_string(note_path(folder, id)?).ok()?);
    let email = notes.follow_up?;
    Some(EmailDraft::from_follow_up(&email, &notes.title))
}

/// The notes file for a meeting id. Ids are plain file names in the folder,
/// never paths, so a crafted id cannot reach other files.
pub(crate) fn note_path(folder: &Path, id: &str) -> Option<PathBuf> {
    let plain = !id.contains(['/', '\\', ':']) && id != "." && id != "..";
    (plain && is_notes_file(id)).then(|| folder.join(id))
}

fn is_notes_file(name: &str) -> bool {
    !name.starts_with('.')
        && Path::new(name)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
}

/// Moves a file to the Recycle Bin, where the user can restore it.
#[cfg(windows)]
pub(crate) fn recycle(path: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::UI::Shell::{
        FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, SHFILEOPSTRUCTW,
        SHFileOperationW,
    };

    // The list of files ends with an empty name.
    let from: Vec<u16> = path.as_os_str().encode_wide().chain([0, 0]).collect();
    let mut operation = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: from.as_ptr(),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT) as u16,
        ..SHFILEOPSTRUCTW::default()
    };
    // SAFETY: `operation` points at `from`, a double-null-terminated path
    // list that outlives the call; no target or name mappings are used.
    let status = unsafe { SHFileOperationW(&mut operation) };
    if status != 0 || operation.fAnyOperationsAborted != 0 {
        return Err(std::io::Error::other(format!(
            "the file could not be moved to the Recycle Bin (code {status})"
        )));
    }
    Ok(())
}

/// Moves a file to the desktop trash, where the user can restore it.
#[cfg(not(windows))]
pub(crate) fn recycle(path: &Path) -> std::io::Result<()> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .ok_or_else(|| std::io::Error::other("no trash folder"))?;
    let trash = data.join("Trash").join("files");
    std::fs::create_dir_all(&trash)?;
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("not a file"))?;
    std::fs::rename(path, trash.join(name))
}

/// Opens a link, such as a `mailto:` draft, with its default app.
pub(crate) fn open_link(link: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};

        let wide = |value: &str| value.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let (operation, target) = (wide("open"), wide(link));
        // SAFETY: both strings are null-terminated and outlive the call.
        let result = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                operation.as_ptr(),
                target.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        // Values above 32 mean success.
        if result as usize <= 32 {
            return Err(std::io::Error::other(
                "no mail app is set up to open email drafts",
            ));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("xdg-open")
            .arg(link)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(drop)
    }
}

/// Opens a folder in the file manager, or a file in its default app.
pub(crate) fn reveal(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    let program = "explorer.exe";
    #[cfg(not(windows))]
    let program = "xdg-open";
    std::process::Command::new(program)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(drop)
}

#[cfg(test)]
mod tests {
    use agentdictate_core::{Speaker, TranscriptLine, meeting_notes_markdown};

    use super::*;

    fn save(folder: &Path, name: &str, title: &str, summary: Option<&str>) {
        let markdown = meeting_notes_markdown(
            title,
            "2026-10-13 09:00",
            65_000,
            summary,
            &[TranscriptLine {
                id: 1,
                speaker: Speaker::Them,
                text: "Hello".into(),
                at_ms: 0,
                voice: None,
            }],
            &agentdictate_core::VoiceNames::new(),
            &[],
        );
        std::fs::write(folder.join(name), markdown).unwrap();
    }

    #[test]
    fn meetings_are_listed_newest_first_and_opened_by_name() {
        let folder = tempfile::tempdir().unwrap();
        save(
            folder.path(),
            "2026-10-12 1500 Hiring sync.md",
            "Hiring sync",
            None,
        );
        save(
            folder.path(),
            "2026-10-13 0900 Pilot planning.md",
            "Pilot planning",
            Some("Agreed on a pilot.\n\n**Follow-up email**\nHi Sam,"),
        );
        std::fs::write(folder.path().join("notes.txt"), "not a meeting").unwrap();
        let notes = scan(folder.path());
        let titles: Vec<&str> = notes.iter().map(|note| note.title.as_str()).collect();
        assert_eq!(titles, ["Pilot planning", "Hiring sync"]);
        // Search covers the whole notes, transcript included.
        assert!(notes[0].matches("pilot hello"));
        assert!(!notes[1].matches("pilot"));
        assert_eq!(notes[0].overview, "Agreed on a pilot.");
        assert_eq!(notes[0].metadata(), "2026-10-13 09:00 \u{00b7} 1:05");
        let detail = open(folder.path(), &notes[0].id).unwrap();
        assert_eq!(detail.follow_up.as_deref(), Some("Hi Sam,"));
        assert!(detail.transcript.contains("Hello"));
        assert!(open(folder.path(), "missing.md").is_none());
        let draft = email_draft(folder.path(), &notes[0].id).unwrap();
        assert_eq!(draft.subject, "Follow-up: Pilot planning");
        assert_eq!(draft.body, "Hi Sam,");
        assert!(email_draft(folder.path(), &notes[1].id).is_none());
        assert!(scan(&folder.path().join("absent")).is_empty());
    }

    #[test]
    fn meeting_ids_never_leave_the_folder() {
        let folder = Path::new("C:/meetings");
        assert!(note_path(folder, "2026-10-13 0900 Pilot.md").is_some());
        assert!(note_path(folder, "../secrets.md").is_none());
        assert!(note_path(folder, "..\\secrets.md").is_none());
        assert!(note_path(folder, "C:secrets.md").is_none());
        assert!(note_path(folder, "notes.txt").is_none());
    }
}
