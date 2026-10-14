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
    /// The other side's people, when they were named.
    pub with: String,
    /// The whole notes in lower case, for search.
    pub search_text: String,
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
            with: notes.with.clone(),
            search_text: String::new(),
        }
    }

    /// The notes' text searched for this meeting; set when the notes are
    /// read.
    #[must_use]
    pub fn with_search_text(mut self, markdown: &str) -> Self {
        self.search_text = markdown.to_lowercase();
        self
    }

    /// Whether every word of a query appears in the meeting's notes.
    #[must_use]
    pub fn matches(&self, query: &str) -> bool {
        let fallback;
        let text = if self.search_text.is_empty() {
            fallback = format!(
                "{} {} {} {}",
                self.title, self.overview, self.with, self.started
            )
            .to_lowercase();
            &fallback
        } else {
            &self.search_text
        };
        query
            .to_lowercase()
            .split_whitespace()
            .all(|word| text.contains(word))
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
    /// The other side's people, when they were named.
    pub with: String,
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
            with: notes.with,
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

impl MeetingsViewModel {
    /// The saved meetings matching a search, newest first.
    #[must_use]
    pub fn search(&self, query: &str) -> Vec<MeetingNoteViewModel> {
        self.notes
            .iter()
            .filter(|note| note.matches(query))
            .cloned()
            .collect()
    }
}

fn metadata(started: &str, duration: &str) -> String {
    [started.trim(), duration.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" \u{00b7} ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meetings_are_found_by_every_word_of_a_search() {
        let note = |title: &str, notes: &str| {
            MeetingNoteViewModel {
                id: format!("{title}.md"),
                title: title.to_owned(),
                ..MeetingNoteViewModel::default()
            }
            .with_search_text(notes)
        };
        let meetings = MeetingsViewModel {
            notes: vec![
                note(
                    "Pilot planning",
                    "# Pilot planning\n- **With:** Sam Lee\nA 25-seat pilot.",
                ),
                note("Budget review", "# Budget review\nQ3 budget approved."),
            ],
            selected: None,
        };
        let titles = |query: &str| {
            meetings
                .search(query)
                .into_iter()
                .map(|note| note.title)
                .collect::<Vec<_>>()
        };
        assert_eq!(titles(""), ["Pilot planning", "Budget review"]);
        assert_eq!(titles("sam PILOT"), ["Pilot planning"]);
        assert_eq!(titles("budget"), ["Budget review"]);
        assert!(titles("sam budget").is_empty());
        // Without the notes' text, the listed fields are searched.
        let listed = MeetingNoteViewModel {
            title: "Kickoff".into(),
            with: "Ana".into(),
            ..MeetingNoteViewModel::default()
        };
        assert!(listed.matches("ana kickoff"));
    }
}
