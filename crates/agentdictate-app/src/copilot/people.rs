//! What earlier meetings say about the people on a call: when the user last
//! met them, what that meeting concluded, and what the user still owes.

use std::path::Path;

use agentdictate_core::{MeetingNotes, given_name, parse_meeting_notes};

/// Notes files read for one brief, newest first.
const MAX_FILES: usize = 300;
const MAX_MEETINGS: usize = 3;
const MAX_PROMISES: usize = 6;

/// A saved meeting's notes.
pub(crate) struct Saved {
    pub notes: MeetingNotes,
}

/// Saved meetings, newest first. Notes files start with their date and
/// time, so names sort chronologically.
pub(crate) fn saved_meetings(folder: &Path, limit: usize) -> Vec<Saved> {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.ends_with(".md") && !name.starts_with('.'))
        .collect();
    names.sort_unstable_by(|left, right| right.cmp(left));
    names
        .into_iter()
        .take(limit)
        .filter_map(|file| {
            let markdown = std::fs::read_to_string(folder.join(&file)).ok()?;
            let mut notes = parse_meeting_notes(&markdown);
            if notes.title.is_empty() {
                notes.title = file.trim_end_matches(".md").to_owned();
            }
            Some(Saved { notes })
        })
        .collect()
}

/// The people a saved meeting was with.
pub(crate) fn people_of(notes: &MeetingNotes) -> Vec<String> {
    notes
        .with
        .split(',')
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .collect()
}

/// Whether a saved meeting included a person, going by given name when the
/// notes only have that.
pub(crate) fn met_with(notes: &MeetingNotes, person: &str) -> bool {
    people_of(notes).iter().any(|name| {
        name.eq_ignore_ascii_case(person)
            || (given_name(name).eq_ignore_ascii_case(given_name(person))
                && (name.split_whitespace().count() == 1 || person.split_whitespace().count() == 1))
    })
}

/// What to know before talking with `people`, in Markdown, or `None` when
/// no earlier meeting included any of them.
pub(crate) fn brief(folder: &Path, people: &[String]) -> Option<String> {
    let meetings: Vec<Saved> = saved_meetings(folder, MAX_FILES)
        .into_iter()
        .filter(|saved| people.iter().any(|person| met_with(&saved.notes, person)))
        .collect();
    let latest = meetings.first()?;
    let date = latest.notes.started.split(' ').next().unwrap_or_default();
    let mut brief = format!(
        "**Last time with {}** \u{00b7} {} \u{00b7} {}\n\n",
        latest.notes.with, date, latest.notes.title
    );
    let overview = latest.notes.overview();
    if !overview.is_empty() {
        brief.push_str(&overview);
        brief.push_str("\n\n");
    }
    let open: Vec<&str> = meetings
        .iter()
        .flat_map(|saved| saved.notes.promises.iter())
        .filter(|(_, done)| !done)
        .map(|(promise, _)| promise.as_str())
        .take(MAX_PROMISES)
        .collect();
    if !open.is_empty() {
        brief.push_str("**You still owe them:**\n");
        for promise in open {
            brief.push_str(&format!("- {promise}\n"));
        }
        brief.push('\n');
    }
    let earlier: Vec<String> = meetings
        .iter()
        .skip(1)
        .take(MAX_MEETINGS - 1)
        .map(|saved| {
            format!(
                "{} ({})",
                saved.notes.title,
                saved.notes.started.split(' ').next().unwrap_or_default()
            )
        })
        .collect();
    if !earlier.is_empty() {
        brief.push_str(&format!("Earlier: {}.\n", earlier.join("; ")));
    }
    Some(brief.trim_end().to_owned())
}

#[cfg(test)]
mod tests {
    use agentdictate_core::{NotesExtras, Speaker, TranscriptLine, VoiceNames, meeting_notes_with};

    use super::*;

    fn save(folder: &Path, file: &str, title: &str, who: &str, promises: &[&str]) {
        let lines = [TranscriptLine {
            id: 1,
            speaker: Speaker::Them,
            text: "Thanks for the walkthrough.".into(),
            at_ms: 0,
            voice: Some(0),
        }];
        let names: VoiceNames = [(0, who.to_owned())].into_iter().collect();
        let markdown = meeting_notes_with(
            title,
            &file[..16].replace("09", "09:"),
            60_000,
            Some("They want a pilot before committing.\n\n**Key points**\n- Budget is tight"),
            &lines,
            &names,
            &[],
            &NotesExtras {
                promises: promises
                    .iter()
                    .map(|promise| (*promise).to_owned())
                    .collect(),
                ..NotesExtras::default()
            },
        );
        std::fs::write(folder.join(file), markdown).unwrap();
    }

    #[test]
    fn the_brief_recalls_the_last_meeting_and_what_is_still_owed() {
        let folder = tempfile::tempdir().unwrap();
        save(
            folder.path(),
            "2026-11-03 0900 Pricing review.md",
            "Pricing review",
            "Sam Lee",
            &["Send the pricing sheet by Friday"],
        );
        save(
            folder.path(),
            "2026-10-20 0900 Intro call.md",
            "Intro call",
            "Sam Lee",
            &[],
        );
        save(
            folder.path(),
            "2026-11-05 0900 Hiring sync.md",
            "Hiring sync",
            "Dana Ortiz",
            &["Share the job post"],
        );
        let brief = brief(folder.path(), &["Sam".to_owned()]).unwrap();
        assert!(brief.contains("Last time with Sam Lee"));
        assert!(brief.contains("Pricing review"));
        assert!(brief.contains("They want a pilot before committing."));
        assert!(brief.contains("- Send the pricing sheet by Friday"));
        assert!(brief.contains("Earlier: Intro call"));
        assert!(
            !brief.contains("job post"),
            "other people's meetings stay out"
        );
    }

    #[test]
    fn people_never_met_have_no_brief() {
        let folder = tempfile::tempdir().unwrap();
        save(
            folder.path(),
            "2026-11-03 0900 Pricing review.md",
            "Pricing review",
            "Sam Lee",
            &[],
        );
        assert!(brief(folder.path(), &["Priya Shah".to_owned()]).is_none());
        assert!(brief(folder.path(), &["Samantha".to_owned()]).is_none());
    }
}
