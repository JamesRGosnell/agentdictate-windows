//! A running record of a meeting, so a copilot closed before it finishes,
//! by a crash or a reinstall, still leaves its notes behind.
//!
//! Each published line, kept answer, note and promise is appended as one
//! JSON line to `<meetings>/.live/<session>.jsonl` and handed to the system
//! at once, so killing the process loses at most the entry being written. A
//! session that saves its notes deletes its record; one found when the next
//! copilot starts becomes a notes file titled as recovered.

use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use agentdictate_core::{NotesExtras, Scorecard, TranscriptLine, VoiceNames, meeting_notes_with};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

/// Hidden, so neither the Meetings page nor the knowledge reader lists it.
const LIVE_DIR: &str = ".live";
const EXTENSION: &str = "jsonl";
/// Shown where the summary would be in notes rebuilt from a record.
pub(crate) const RECOVERED_NOTICE: &str = "This meeting ended before the copilot finished, so these notes were rebuilt from what it saved during the call. No summary was written.";

/// One thing worth keeping from the meeting.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Entry {
    /// First in every record: when the meeting started, as RFC 3339.
    Started {
        at: String,
    },
    Line {
        line: TranscriptLine,
    },
    Answer {
        id: u64,
        title: String,
        text: String,
    },
    /// A rewritten line the user kept for an answer, or `None` to drop it.
    Kept {
        id: u64,
        text: Option<String>,
    },
    Note {
        at_ms: u64,
        text: String,
    },
    Promise {
        text: String,
    },
    Title {
        title: String,
    },
    Names {
        names: VoiceNames,
    },
}

/// The record a session appends to. It is created with the first entry,
/// so a session in which nothing was said leaves nothing behind.
pub(crate) struct Journal {
    path: PathBuf,
    started: DateTime<Local>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    file: Option<File>,
    /// Writing failed once; the session goes on without a record.
    failed: bool,
    removed: bool,
    title: String,
}

impl Journal {
    pub(crate) fn new(notes_dir: &Path, session_key: &str, started: DateTime<Local>) -> Self {
        Self {
            path: notes_dir
                .join(LIVE_DIR)
                .join(format!("{session_key}.{EXTENSION}")),
            started,
            state: Mutex::new(State::default()),
        }
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Appends `entry`, creating the record first if need be.
    pub(crate) fn append(&self, entry: &Entry) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.failed || state.removed {
            return;
        }
        if let Entry::Title { title } = entry {
            if state.title == *title {
                return;
            }
            state.title.clone_from(title);
        }
        if state.file.is_none() {
            match self.create() {
                Ok(file) => state.file = Some(file),
                Err(error) => {
                    tracing::warn!(%error, "the meeting record could not be started");
                    state.failed = true;
                    return;
                }
            }
        }
        let Some(file) = state.file.as_mut() else {
            return;
        };
        if let Err(error) = write_entry(file, entry) {
            tracing::warn!(%error, "the meeting record could not be written");
            state.failed = true;
        }
    }

    fn create(&self) -> std::io::Result<File> {
        if let Some(folder) = self.path.parent() {
            std::fs::create_dir_all(folder)?;
        }
        // Opened for writing rather than appending: Windows locks only a
        // file opened with read or write access.
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.path)?;
        file.seek(SeekFrom::End(0))?;
        // Held for the life of the session, so a copilot started meanwhile
        // never mistakes this record for one left behind.
        if let Err(error) = file.try_lock() {
            tracing::warn!(%error, "the meeting record could not be locked");
        }
        write_entry(
            &mut file,
            &Entry::Started {
                at: self.started.to_rfc3339(),
            },
        )?;
        Ok(file)
    }

    /// Deletes the record once the notes it backs are saved.
    pub(crate) fn remove(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.removed = true;
        if state.file.take().is_some()
            && let Err(error) = std::fs::remove_file(&self.path)
        {
            tracing::warn!(%error, "the meeting record could not be removed");
        }
    }
}

fn write_entry(file: &mut File, entry: &Entry) -> std::io::Result<()> {
    let mut line = serde_json::to_string(entry).map_err(std::io::Error::other)?;
    line.push('\n');
    // One write per entry, straight to the system: nothing waits in this
    // process to be lost if it is killed.
    file.write_all(line.as_bytes())?;
    file.flush()
}

/// Turns every record a closed session left behind into notes, skipping
/// `current` (this session's) and any another running copilot still holds.
/// Returns the notes written.
pub(crate) fn recover(notes_dir: &Path, current: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(notes_dir.join(LIVE_DIR)) else {
        return Vec::new();
    };
    let mut records: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == EXTENSION)
                && path
                    .file_stem()
                    .is_some_and(|stem| stem.to_string_lossy() != current)
        })
        .collect();
    records.sort();
    let mut written = Vec::new();
    for record in records {
        match recover_one(notes_dir, &record) {
            Ok(Some(path)) => written.push(path),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, record = %record.display(), "a meeting record could not be recovered");
            }
        }
    }
    written
}

fn recover_one(notes_dir: &Path, record: &Path) -> std::io::Result<Option<PathBuf>> {
    let mut file = OpenOptions::new().read(true).write(true).open(record)?;
    match file.try_lock() {
        Ok(()) => {}
        // Another copilot is still writing it.
        Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
        Err(std::fs::TryLockError::Error(error)) => return Err(error),
    }
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let meeting = Recovered::parse(&text);
    let written = if meeting.is_empty() {
        None
    } else {
        let started = meeting.started.unwrap_or_else(|| {
            std::fs::metadata(record)
                .and_then(|metadata| metadata.modified())
                .map(DateTime::<Local>::from)
                .unwrap_or_else(|_| Local::now())
        });
        std::fs::create_dir_all(notes_dir)?;
        let stem = format!(
            "{} (recovered)",
            notes_file_stem(&started, &meeting.title())
        );
        let path = unused_path(notes_dir, &stem);
        write_atomically(&path, &meeting.markdown(&started))?;
        tracing::info!(path = %path.display(), lines = meeting.lines.len(), "recovered the notes of a meeting that was cut off");
        Some(path)
    };
    drop(file);
    std::fs::remove_file(record)?;
    Ok(written)
}

/// What a record holds, rebuilt in the order it was written.
#[derive(Default)]
struct Recovered {
    started: Option<DateTime<Local>>,
    lines: Vec<TranscriptLine>,
    answers: Vec<(u64, String, String)>,
    kept: std::collections::HashMap<u64, String>,
    notes: Vec<(u64, String)>,
    promises: Vec<String>,
    title: String,
    names: VoiceNames,
}

impl Recovered {
    fn parse(text: &str) -> Self {
        let mut meeting = Self::default();
        // A line cut short by the process ending is skipped.
        for entry in text
            .lines()
            .filter_map(|line| serde_json::from_str::<Entry>(line).ok())
        {
            match entry {
                Entry::Started { at } => {
                    meeting.started = DateTime::parse_from_rfc3339(&at)
                        .ok()
                        .map(|at| at.with_timezone(&Local));
                }
                Entry::Line { line } => meeting.lines.push(line),
                Entry::Answer { id, title, text } => meeting.answers.push((id, title, text)),
                Entry::Kept {
                    id,
                    text: Some(text),
                } => {
                    meeting.kept.insert(id, text);
                }
                Entry::Kept { id, text: None } => {
                    meeting.kept.remove(&id);
                }
                Entry::Note { at_ms, text } => meeting.notes.push((at_ms, text)),
                Entry::Promise { text } => meeting.promises.push(text),
                Entry::Title { title } => meeting.title = title,
                Entry::Names { names } => meeting.names = names,
            }
        }
        meeting.lines.sort_by_key(|line| line.at_ms);
        meeting
    }

    fn is_empty(&self) -> bool {
        self.lines.is_empty() && self.notes.is_empty() && self.answers.is_empty()
    }

    fn title(&self) -> String {
        let title = self.title.trim();
        if title.is_empty() {
            "Meeting notes".to_owned()
        } else {
            title.to_owned()
        }
    }

    fn markdown(&self, started: &DateTime<Local>) -> String {
        let duration_ms = self
            .lines
            .iter()
            .map(|line| line.at_ms)
            .chain(self.notes.iter().map(|(at_ms, _)| *at_ms))
            .max()
            .unwrap_or_default();
        let answers: Vec<(String, String)> = self
            .answers
            .iter()
            .map(|(id, title, text)| {
                let text = match self.kept.get(id) {
                    Some(kept) => format!("{text}\n\n**Said instead:** {kept}"),
                    None => text.clone(),
                };
                (title.clone(), text)
            })
            .collect();
        let extras = NotesExtras {
            promises: self.promises.clone(),
            my_notes: self.notes.clone(),
            scorecard: Scorecard::measure(&self.lines).markdown(),
            invited: Vec::new(),
        };
        meeting_notes_with(
            &format!("{} (recovered)", self.title()),
            &started.format("%Y-%m-%d %H:%M").to_string(),
            duration_ms,
            Some(RECOVERED_NOTICE),
            &self.lines,
            &self.names,
            &answers,
            &extras,
        )
    }
}

/// A notes file's name without its extension: when the meeting started and
/// what it was about, so names sort by date.
pub(crate) fn notes_file_stem(started: &DateTime<Local>, title: &str) -> String {
    let slug: String = title
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || character == ' ' || character == '-' {
                character
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "{} {}",
        started.format("%Y-%m-%d %H%M"),
        slug.chars().take(60).collect::<String>()
    )
}

/// `<stem>.md` in `folder`, numbered when that name is taken.
fn unused_path(folder: &Path, stem: &str) -> PathBuf {
    let mut path = folder.join(format!("{stem}.md"));
    let mut number = 2;
    while path.exists() {
        path = folder.join(format!("{stem} {number}.md"));
        number += 1;
    }
    path
}

/// Replaces `path` with `contents` all at once, so notes rewritten when the
/// summary arrives are never left half written.
pub(crate) fn write_atomically(path: &Path, contents: &str) -> std::io::Result<()> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temporary = path.with_file_name(format!(".{name}.tmp"));
    std::fs::write(&temporary, contents)?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

/// "12 s" or "1 min 5 s", for speech that was lost.
pub(crate) fn gap_label(duration_ms: u64) -> String {
    let seconds = duration_ms.div_ceil(1_000).max(1);
    match (seconds / 60, seconds % 60) {
        (0, seconds) => format!("{seconds} s"),
        (minutes, 0) => format!("{minutes} min"),
        (minutes, seconds) => format!("{minutes} min {seconds} s"),
    }
}

#[cfg(test)]
mod tests {
    use agentdictate_core::Speaker;

    use super::*;

    fn line(id: u64, speaker: Speaker, text: &str) -> TranscriptLine {
        TranscriptLine {
            id,
            speaker,
            text: text.into(),
            at_ms: id * 1_000,
            voice: None,
        }
    }

    #[test]
    fn a_record_still_being_written_is_left_alone() {
        let folder = tempfile::tempdir().unwrap();
        let journal = Journal::new(folder.path(), "running", Local::now());
        journal.append(&Entry::Line {
            line: line(1, Speaker::Them, "Can we start with a pilot?"),
        });
        assert!(recover(folder.path(), "this-session").is_empty());
        assert!(journal.path().exists());
        journal.remove();
        assert!(!journal.path().exists());
    }

    #[test]
    fn a_torn_last_entry_and_an_empty_record_are_handled() {
        let folder = tempfile::tempdir().unwrap();
        let live = folder.path().join(LIVE_DIR);
        std::fs::create_dir_all(&live).unwrap();
        let started = serde_json::to_string(&Entry::Started {
            at: "2026-11-07T14:05:00-05:00".into(),
        })
        .unwrap();
        let said = serde_json::to_string(&Entry::Line {
            line: line(3, Speaker::Me, "Thirty days with ten seats."),
        })
        .unwrap();
        std::fs::write(
            live.join("cut.jsonl"),
            format!("{started}\n{said}\n{{\"kind\":\"line\",\"li"),
        )
        .unwrap();
        std::fs::write(live.join("silent.jsonl"), format!("{started}\n")).unwrap();
        let written = recover(folder.path(), "");
        assert_eq!(written.len(), 1, "nothing was said in the silent one");
        let notes = std::fs::read_to_string(&written[0]).unwrap();
        assert!(notes.contains("Thirty days with ten seats."));
        assert!(notes.contains(RECOVERED_NOTICE));
        assert!(
            written[0]
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("(recovered).md")
        );
        assert_eq!(std::fs::read_dir(&live).unwrap().count(), 0);
    }

    #[test]
    fn lost_speech_is_labelled_by_how_long_it_was() {
        assert_eq!(gap_label(11_200), "12 s");
        assert_eq!(gap_label(0), "1 s");
        assert_eq!(gap_label(65_000), "1 min 5 s");
        assert_eq!(gap_label(120_000), "2 min");
    }
}
