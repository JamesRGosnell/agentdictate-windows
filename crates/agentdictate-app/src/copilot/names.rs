//! Who is who on the other side of the call.
//!
//! Voices are told apart by sound; names come from three places, in order of
//! trust: the user, the conversation (an introduction, or someone addressed
//! by name), and the meeting app's window (whose tile is highlighted while a
//! voice speaks). A name sticks once given, so labels never flicker: the
//! conversation replaces one only when it says so twice, the window never
//! does, and the user's own names are final.

use std::collections::BTreeMap;

use agentdictate_core::VoiceNames;

/// Window readings that must agree before they name a voice.
const SEEN_TO_NAME: u8 = 2;
/// Window readings spent on one voice before giving up on it.
const MAX_LOOKS: u8 = 6;
/// Longest name the user can give, in characters.
const MAX_NAME_CHARS: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Clue {
    /// The user named the voice.
    User,
    /// The conversation makes the name certain.
    Heard,
    /// The meeting window showed this person speaking while the voice spoke.
    Seen,
    /// The meeting window shows one other person, and one voice was heard.
    OnlyOther,
}

#[derive(Default)]
struct Entry {
    name: Option<String>,
    from_user: bool,
    /// A different name the conversation reported, and how often.
    challenger: Option<(String, u8)>,
    /// Names the window showed while this voice spoke, and how often.
    seen: Vec<(String, u8)>,
    looks: u8,
}

impl Entry {
    fn set(&mut self, name: &str) -> bool {
        self.challenger = None;
        self.seen.clear();
        let changed = self.name.as_deref() != Some(name);
        self.name = Some(name.to_owned());
        changed
    }
}

/// Names of the other side's voices for one session.
#[derive(Default)]
pub(crate) struct Roster {
    entries: BTreeMap<u8, Entry>,
}

impl Roster {
    pub(crate) fn names(&self) -> VoiceNames {
        self.entries
            .iter()
            .filter_map(|(voice, entry)| Some((*voice, entry.name.clone()?)))
            .collect()
    }

    /// Whether reading the meeting window could still name this voice.
    pub(crate) fn needs_look(&self, voice: u8) -> bool {
        self.entries
            .get(&voice)
            .is_none_or(|entry| entry.name.is_none() && entry.looks < MAX_LOOKS)
    }

    /// Counts a window reading spent on this voice.
    pub(crate) fn looked(&mut self, voice: u8) {
        let entry = self.entries.entry(voice).or_default();
        entry.looks = entry.looks.saturating_add(1);
    }

    /// Weighs a clue about a voice's name; returns whether its name changed.
    /// From the user, an empty name forgets the voice's name.
    pub(crate) fn note(&mut self, voice: u8, name: &str, clue: Clue) -> bool {
        let entry = self.entries.entry(voice).or_default();
        if clue == Clue::User {
            let name: String = name
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(MAX_NAME_CHARS)
                .collect();
            let name = name.trim();
            if name.is_empty() {
                let changed = entry.name.is_some();
                *entry = Entry {
                    looks: entry.looks,
                    ..Entry::default()
                };
                return changed;
            }
            entry.from_user = true;
            return entry.set(name);
        }
        if entry.from_user {
            return false;
        }
        if let Some(current) = entry.name.clone()
            && same_person(&current, name)
        {
            entry.challenger = None;
            // A fuller form of the same name, such as a surname on screen.
            if words(name).len() > words(&current).len() {
                return entry.set(name);
            }
            return false;
        }
        match clue {
            Clue::User => false,
            Clue::Heard if entry.name.is_none() => entry.set(name),
            Clue::Heard => {
                let count = match &mut entry.challenger {
                    Some((challenger, count)) if same_person(challenger, name) => {
                        *count += 1;
                        *count
                    }
                    _ => {
                        entry.challenger = Some((name.to_owned(), 1));
                        1
                    }
                };
                count >= 2 && entry.set(name)
            }
            // The window never replaces a name; it fills in missing ones.
            Clue::OnlyOther | Clue::Seen if entry.name.is_some() => false,
            Clue::OnlyOther => entry.set(name),
            Clue::Seen => {
                let count = match entry
                    .seen
                    .iter_mut()
                    .find(|(seen, _)| same_person(seen, name))
                {
                    Some((seen, count)) => {
                        if words(name).len() > words(seen).len() {
                            name.clone_into(seen);
                        }
                        *count += 1;
                        *count
                    }
                    None => {
                        entry.seen.push((name.to_owned(), 1));
                        1
                    }
                };
                if count < SEEN_TO_NAME {
                    return false;
                }
                let best = entry
                    .seen
                    .iter()
                    .find(|(seen, _)| same_person(seen, name))
                    .map(|(seen, _)| seen.clone())
                    .unwrap_or_else(|| name.to_owned());
                entry.set(&best)
            }
        }
    }
}

fn words(name: &str) -> Vec<String> {
    name.split_whitespace().map(str::to_lowercase).collect()
}

/// Whether two names are the same person: equal, or one is the start of the
/// other, as with "Sam" and "Sam Lee".
fn same_person(left: &str, right: &str) -> bool {
    let (left, right) = (words(left), words(right));
    let shorter = left.len().min(right.len());
    shorter > 0 && left[..shorter] == right[..shorter]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_conversation_names_a_voice_at_once_and_renames_it_only_when_repeated() {
        let mut roster = Roster::default();
        assert!(roster.note(0, "Sam", Clue::Heard));
        assert_eq!(roster.names(), VoiceNames::from([(0, "Sam".to_owned())]));
        assert!(!roster.needs_look(0));
        // One stray mention does not rename; a second one does.
        assert!(!roster.note(0, "Pam", Clue::Heard));
        assert!(roster.note(0, "Pam", Clue::Heard));
        assert_eq!(roster.names()[&0], "Pam");
        // Repeating the current name changes nothing.
        assert!(!roster.note(0, "Pam", Clue::Heard));
    }

    #[test]
    fn the_window_needs_two_agreeing_readings_and_never_replaces_a_name() {
        let mut roster = Roster::default();
        assert!(roster.needs_look(1));
        assert!(!roster.note(1, "Joey", Clue::Seen));
        assert!(!roster.note(1, "Ana", Clue::Seen));
        assert!(roster.note(1, "Joey Tran", Clue::Seen));
        // The fuller form seen is kept.
        assert_eq!(roster.names()[&1], "Joey Tran");
        assert!(!roster.note(1, "Ana", Clue::Seen));
        assert!(!roster.note(1, "Ana", Clue::OnlyOther));
        assert_eq!(roster.names()[&1], "Joey Tran");
    }

    #[test]
    fn a_lone_other_participant_names_the_lone_voice() {
        let mut roster = Roster::default();
        assert!(roster.note(0, "Ana Silva", Clue::OnlyOther));
        assert_eq!(roster.names()[&0], "Ana Silva");
        // The conversation's short form is the same person.
        assert!(!roster.note(0, "Ana", Clue::Heard));
        assert_eq!(roster.names()[&0], "Ana Silva");
        // A surname heard later fills in a short name.
        let mut short = Roster::default();
        short.note(0, "Ana", Clue::Heard);
        assert!(short.note(0, "Ana Silva", Clue::Seen));
        assert_eq!(short.names()[&0], "Ana Silva");
    }

    #[test]
    fn the_users_names_are_final_and_can_be_cleared() {
        let mut roster = Roster::default();
        roster.note(0, "Sam", Clue::Heard);
        assert!(roster.note(0, "  Samuel   Lee ", Clue::User));
        assert_eq!(roster.names()[&0], "Samuel Lee");
        for clue in [Clue::Heard, Clue::Heard, Clue::Seen, Clue::OnlyOther] {
            assert!(!roster.note(0, "Pam", clue));
        }
        assert_eq!(roster.names()[&0], "Samuel Lee");
        assert!(roster.note(0, " ", Clue::User));
        assert!(roster.names().is_empty());
        // A cleared voice can be named again.
        assert!(roster.note(0, "Pam", Clue::Heard));
    }

    #[test]
    fn window_readings_stop_after_a_few_tries() {
        let mut roster = Roster::default();
        for _ in 0..MAX_LOOKS {
            assert!(roster.needs_look(2));
            roster.looked(2);
        }
        assert!(!roster.needs_look(2));
    }

    #[test]
    fn names_match_by_their_leading_words() {
        assert!(same_person("Sam", "sam lee"));
        assert!(same_person("Sam Lee", "Sam"));
        assert!(!same_person("Sam Lee", "Sam Jones"));
        assert!(!same_person("Sam", "Samantha"));
        assert!(!same_person("", "Sam"));
    }
}
