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
    /// What the user promised in the meeting, and whether each is done.
    pub promises: Vec<(String, bool)>,
    /// The share of the words the user spoke, in percent.
    pub talk_share: Option<u8>,
}

/// Someone the user has met, across saved meetings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersonViewModel {
    pub name: String,
    pub meetings: usize,
    /// The date of the latest meeting with them.
    pub last: String,
    /// What the user still owes after meetings with them.
    pub open: usize,
}

/// Something the user promised in a meeting and has not ticked off.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenPromiseViewModel {
    pub meeting_id: String,
    pub meeting: String,
    /// The promise's place in its meeting's list.
    pub index: usize,
    pub text: String,
}

/// An answer to a question asked across all meetings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeetingsAnswer {
    pub question: String,
    /// Markdown.
    pub answer: String,
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
            promises: notes.promises.clone(),
            talk_share: notes.talk_share(),
        }
    }

    /// The people this meeting was with.
    #[must_use]
    pub fn people(&self) -> Vec<&str> {
        self.with
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .collect()
    }

    /// The meeting's date, without its time.
    #[must_use]
    pub fn date(&self) -> &str {
        self.started.split(' ').next().unwrap_or_default()
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
    /// What the user promised, and whether each is done.
    pub promises: Vec<(String, bool)>,
    /// What the user typed during the meeting, in Markdown.
    pub my_notes: String,
    /// How the user spoke, in Markdown.
    pub scorecard: String,
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
            promises: notes.promises,
            my_notes: notes.my_notes,
            scorecard: notes.scorecard,
        }
    }

    /// The meeting without its transcript, as Markdown to paste into a
    /// document, a CRM, or a chat.
    #[must_use]
    pub fn export_markdown(&self) -> String {
        let mut text = format!("# {}\n\n{}\n", self.title, self.metadata());
        if !self.with.trim().is_empty() {
            text.push_str(&format!("With {}\n", self.with.trim()));
        }
        if !self.summary.trim().is_empty() {
            text.push_str(&format!("\n{}\n", self.summary.trim()));
        }
        if !self.promises.is_empty() {
            text.push_str("\n**Your commitments**\n");
            for (promise, done) in &self.promises {
                text.push_str(&format!(
                    "- [{}] {promise}\n",
                    if *done { 'x' } else { ' ' }
                ));
            }
        }
        if !self.my_notes.trim().is_empty() {
            text.push_str(&format!("\n**Your notes**\n{}\n", self.my_notes.trim()));
        }
        if let Some(email) = &self.follow_up {
            text.push_str(&format!("\n**Follow-up email**\n{}\n", email.trim()));
        }
        text
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
    /// The latest answer to a question asked across the meetings.
    pub answer: Option<MeetingsAnswer>,
}

impl MeetingsViewModel {
    /// The people met most recently, each with how often and what is still
    /// owed to them.
    #[must_use]
    pub fn people(&self) -> Vec<PersonViewModel> {
        let mut people: Vec<PersonViewModel> = Vec::new();
        // Notes are newest first, so the first sighting is the latest.
        for note in &self.notes {
            let open = note.promises.iter().filter(|(_, done)| !done).count();
            for name in note.people() {
                match people
                    .iter_mut()
                    .find(|person| person.name.eq_ignore_ascii_case(name))
                {
                    Some(person) => {
                        person.meetings += 1;
                        person.open += open;
                    }
                    None => people.push(PersonViewModel {
                        name: name.to_owned(),
                        meetings: 1,
                        last: note.date().to_owned(),
                        open,
                    }),
                }
            }
        }
        people
    }

    /// Everything the user still owes, newest meeting first.
    #[must_use]
    pub fn open_promises(&self) -> Vec<OpenPromiseViewModel> {
        self.notes
            .iter()
            .flat_map(|note| {
                note.promises
                    .iter()
                    .enumerate()
                    .filter(|(_, (_, done))| !done)
                    .map(|(index, (text, _))| OpenPromiseViewModel {
                        meeting_id: note.id.clone(),
                        meeting: match note.date() {
                            "" => note.title.clone(),
                            date => format!("{} \u{00b7} {date}", note.title),
                        },
                        index,
                        text: text.clone(),
                    })
            })
            .collect()
    }

    /// How much of the talking the user did lately: the latest meeting's
    /// share and the average over the last few, when at least two are known.
    #[must_use]
    pub fn talk_trend(&self) -> Option<String> {
        let shares: Vec<u8> = self
            .notes
            .iter()
            .filter_map(|note| note.talk_share)
            .take(5)
            .collect();
        let latest = *shares.first()?;
        if shares.len() < 2 {
            return None;
        }
        let average =
            shares.iter().map(|share| u32::from(*share)).sum::<u32>() / shares.len() as u32;
        Some(format!(
            "You spoke {latest}% of the words in your latest meeting; {average}% on average over the last {}.",
            shares.len()
        ))
    }

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
            ..MeetingsViewModel::default()
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

    #[test]
    fn people_commitments_and_talk_share_are_gathered_across_meetings() {
        let note = |id: &str, with: &str, promises: &[(&str, bool)], share: Option<u8>| {
            MeetingNoteViewModel {
                id: format!("{id}.md"),
                title: id.to_owned(),
                started: "2026-11-03 09:00".into(),
                with: with.to_owned(),
                promises: promises
                    .iter()
                    .map(|(text, done)| ((*text).to_owned(), *done))
                    .collect(),
                talk_share: share,
                ..MeetingNoteViewModel::default()
            }
        };
        let meetings = MeetingsViewModel {
            notes: vec![
                note(
                    "Renewal",
                    "Sam Lee, Joey Tran",
                    &[("Send the contract", false), ("Share the deck", true)],
                    Some(40),
                ),
                note("Intro", "Sam Lee", &[("Send pricing", false)], Some(60)),
                note("Solo review", "", &[], None),
            ],
            ..MeetingsViewModel::default()
        };
        let people = meetings.people();
        assert_eq!(people.len(), 2);
        assert_eq!(
            people[0],
            PersonViewModel {
                name: "Sam Lee".into(),
                meetings: 2,
                last: "2026-11-03".into(),
                open: 2,
            }
        );
        assert_eq!(people[1].meetings, 1);
        let open = meetings.open_promises();
        assert_eq!(open.len(), 2);
        assert_eq!(open[0].text, "Send the contract");
        assert_eq!(
            (open[0].meeting_id.as_str(), open[0].index),
            ("Renewal.md", 0)
        );
        assert_eq!(open[1].text, "Send pricing");
        let trend = meetings.talk_trend().unwrap();
        assert!(trend.contains("40%") && trend.contains("50% on average over the last 2"));
        assert!(MeetingsViewModel::default().talk_trend().is_none());
    }

    #[test]
    fn a_meeting_exports_without_its_transcript() {
        let detail = MeetingDetailViewModel {
            title: "Renewal".into(),
            started: "2026-11-03 09:00".into(),
            with: "Sam Lee".into(),
            summary: "They will renew.".into(),
            promises: vec![("Send the contract".into(), false)],
            transcript: "SECRET TRANSCRIPT".into(),
            ..MeetingDetailViewModel::default()
        };
        let text = detail.export_markdown();
        assert!(text.starts_with("# Renewal\n"));
        assert!(text.contains("With Sam Lee"));
        assert!(text.contains("- [ ] Send the contract"));
        assert!(!text.contains("SECRET"));
    }
}
