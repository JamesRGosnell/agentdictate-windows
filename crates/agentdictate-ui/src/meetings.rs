//! Saved meetings, as listed and opened in the Meetings page.

use agentdictate_core::MeetingNotes;

/// A saved meeting in the list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MeetingNoteViewModel {
    /// The notes file name, which identifies the meeting.
    pub id: String,
    pub title: String,
    pub started: String,
    pub duration: String,
    /// The summary's opening sentence.
    pub overview: String,
}

impl MeetingNoteViewModel {
    #[must_use]
    pub fn from_notes(id: impl Into<String>, notes: &MeetingNotes) -> Self {
        Self {
            id: id.into(),
            title: notes.title.clone(),
            started: notes.started.clone(),
            duration: notes.duration.clone(),
            overview: notes.overview(),
        }
    }

    /// "2026-10-13 09:00 · 11:33", leaving out what is unknown.
    #[must_use]
    pub fn metadata(&self) -> String {
        metadata(&self.started, &self.duration)
    }
}

/// An opened meeting: its notes in Markdown sections.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MeetingDetailViewModel {
    pub id: String,
    pub title: String,
    pub started: String,
    pub duration: String,
    pub summary: String,
    pub follow_up: Option<String>,
    pub answers: String,
    pub transcript: String,
}

impl MeetingDetailViewModel {
    #[must_use]
    pub fn from_notes(id: impl Into<String>, notes: MeetingNotes) -> Self {
        Self {
            id: id.into(),
            title: notes.title,
            started: notes.started,
            duration: notes.duration,
            summary: notes.summary,
            follow_up: notes.follow_up,
            answers: notes.answers,
            transcript: notes.transcript,
        }
    }

    #[must_use]
    pub fn metadata(&self) -> String {
        metadata(&self.started, &self.duration)
    }
}

/// The Meetings page: saved meetings newest first, and the one opened.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MeetingsViewModel {
    pub notes: Vec<MeetingNoteViewModel>,
    pub selected: Option<MeetingDetailViewModel>,
}

fn metadata(started: &str, duration: &str) -> String {
    [started.trim(), duration.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" \u{00b7} ")
}
