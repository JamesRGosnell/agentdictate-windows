//! The meeting happening now, read from a calendar feed (iCalendar).
//!
//! Outlook, Google Calendar, and most other calendars can publish a private
//! `.ics` address. The feed names the meeting and who was invited, so the
//! copilot knows both before anyone speaks.

use agentdictate_core::clean_person_name;
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};

/// A meeting counts as "now" from this long before it starts.
const EARLY_MINUTES: i64 = 10;
const MAX_ATTENDEES: usize = 12;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CalendarEvent {
    pub title: String,
    /// The people invited, by name, without the organizer's duplicates.
    pub attendees: Vec<String>,
}

#[derive(Default)]
struct Entry {
    title: String,
    start: Option<Moment>,
    end: Option<Moment>,
    attendees: Vec<String>,
    /// Repeats daily or weekly.
    repeat: Option<Repeat>,
    cancelled: bool,
}

#[derive(Clone, Copy)]
enum Moment {
    Utc(NaiveDateTime),
    /// A time in the calendar's own zone, read as this computer's zone.
    Local(NaiveDateTime),
}

impl Moment {
    fn utc(self) -> Option<DateTime<Utc>> {
        match self {
            Self::Utc(time) => Some(Utc.from_utc_datetime(&time)),
            Self::Local(time) => Local
                .from_local_datetime(&time)
                .earliest()
                .map(|time| time.with_timezone(&Utc)),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Repeat {
    Daily,
    Weekly,
}

/// Joins folded lines: a line that starts with a space or tab continues the
/// one before it.
fn unfold(feed: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for line in feed.lines() {
        match line.strip_prefix([' ', '\t']) {
            Some(rest) if !lines.is_empty() => {
                if let Some(last) = lines.last_mut() {
                    last.push_str(rest);
                }
            }
            _ => lines.push(line.trim_end().to_owned()),
        }
    }
    lines
}

fn moment(parameters: &str, value: &str) -> Option<Moment> {
    // All-day entries are dates, not meetings.
    if parameters.to_ascii_uppercase().contains("VALUE=DATE") && !value.contains('T') {
        return None;
    }
    let utc = value.ends_with('Z');
    let time = NaiveDateTime::parse_from_str(value.trim_end_matches('Z'), "%Y%m%dT%H%M%S").ok()?;
    Some(if utc {
        Moment::Utc(time)
    } else {
        Moment::Local(time)
    })
}

fn unescape(text: &str) -> String {
    text.replace("\\,", ",")
        .replace("\\;", ";")
        .replace("\\n", " ")
        .replace("\\N", " ")
        .replace("\\\\", "\\")
}

/// An attendee's name from the `CN` parameter, when it is a name and not
/// an address.
fn attendee_name(parameters: &str) -> Option<String> {
    let lower = parameters.to_ascii_lowercase();
    let start = lower.find("cn=")? + 3;
    let rest = &parameters[start..];
    let name = if let Some(quoted) = rest.strip_prefix('"') {
        quoted.split('"').next()?
    } else {
        rest.split([';', ':']).next()?
    };
    if name.contains('@') {
        return None;
    }
    // "Lee, Sam" is how some directories write names.
    let name = match name.split_once(',') {
        Some((family, given)) if !given.trim().is_empty() => {
            format!("{} {}", given.trim(), family.trim())
        }
        _ => name.to_owned(),
    };
    clean_person_name(&name)
}

fn entries(feed: &str) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut current: Option<Entry> = None;
    for line in unfold(feed) {
        if line.eq_ignore_ascii_case("BEGIN:VEVENT") {
            current = Some(Entry::default());
            continue;
        }
        if line.eq_ignore_ascii_case("END:VEVENT") {
            entries.extend(current.take());
            continue;
        }
        let Some(entry) = current.as_mut() else {
            continue;
        };
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (name, parameters) = key.split_once(';').unwrap_or((key, ""));
        match name.to_ascii_uppercase().as_str() {
            "SUMMARY" => entry.title = unescape(value).trim().to_owned(),
            "DTSTART" => entry.start = moment(parameters, value),
            "DTEND" => entry.end = moment(parameters, value),
            "STATUS" => entry.cancelled = value.eq_ignore_ascii_case("CANCELLED"),
            "RRULE" => {
                let rule = value.to_ascii_uppercase();
                entry.repeat = if rule.contains("FREQ=DAILY") {
                    Some(Repeat::Daily)
                } else if rule.contains("FREQ=WEEKLY") && !rule.contains("INTERVAL=") {
                    Some(Repeat::Weekly)
                } else {
                    None
                };
            }
            "ATTENDEE" => {
                // The address follows "mailto:", which split the line early.
                let parameters = line
                    .split_once(';')
                    .map_or("", |(_, rest)| rest)
                    .split(":mailto")
                    .next()
                    .unwrap_or_default();
                if let Some(name) = attendee_name(parameters)
                    && !entry
                        .attendees
                        .iter()
                        .any(|known: &String| known.eq_ignore_ascii_case(&name))
                    && entry.attendees.len() < MAX_ATTENDEES
                {
                    entry.attendees.push(name);
                }
            }
            _ => {}
        }
    }
    entries
}

/// Moves a repeating meeting's times to the day of `now`, when it falls on
/// that day.
fn on_day_of(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    repeat: Repeat,
    now: DateTime<Utc>,
) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    if now < start {
        return None;
    }
    // Compare in local time, where "the same weekday" means what the user
    // expects.
    let local_start = start.with_timezone(&Local);
    let today: NaiveDate = now.with_timezone(&Local).date_naive();
    if repeat == Repeat::Weekly && local_start.weekday() != today.weekday() {
        return None;
    }
    let moved = Local
        .from_local_datetime(&today.and_time(local_start.time()))
        .earliest()?
        .with_timezone(&Utc);
    Some((moved, moved + (end - start)))
}

/// The meeting in the feed that is under way at `now`, or about to start.
/// When several overlap, the one that started last wins.
pub(crate) fn current_event(feed: &str, now: DateTime<Utc>) -> Option<CalendarEvent> {
    let mut best: Option<(DateTime<Utc>, Entry)> = None;
    for entry in entries(feed) {
        if entry.cancelled || entry.title.is_empty() {
            continue;
        }
        let Some(start) = entry.start.and_then(Moment::utc) else {
            continue;
        };
        let end = entry
            .end
            .and_then(Moment::utc)
            .unwrap_or(start + Duration::hours(1));
        let (start, end) = match entry.repeat {
            Some(repeat) if now > end => match on_day_of(start, end, repeat, now) {
                Some(times) => times,
                None => continue,
            },
            _ => (start, end),
        };
        if now < start - Duration::minutes(EARLY_MINUTES) || now > end {
            continue;
        }
        if best.as_ref().is_none_or(|(known, _)| start > *known) {
            best = Some((start, entry));
        }
    }
    best.map(|(_, entry)| CalendarEvent {
        title: entry.title,
        attendees: entry.attendees,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> DateTime<Utc> {
        Utc.from_utc_datetime(&NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").unwrap())
    }

    const FEED: &str = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
SUMMARY:Pricing review\\, Acme\r\n\
DTSTART:20261110T150000Z\r\n\
DTEND:20261110T153000Z\r\n\
ORGANIZER;CN=James Gosnell:mailto:james@example.com\r\n\
ATTENDEE;ROLE=REQ-PARTICIPANT;CN=\"Lee, Sam\":mailto:sam@example.com\r\n\
ATTENDEE;CN=Joey Tran;PARTSTAT=ACCEPTED:mailto:joey@example.com\r\n\
ATTENDEE;CN=room-4@example.com:mailto:room-4@example.com\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
SUMMARY:Company holiday\r\n\
DTSTART;VALUE=DATE:20261110\r\n\
DTEND;VALUE=DATE:20261111\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
SUMMARY:Cancelled sync\r\n\
STATUS:CANCELLED\r\n\
DTSTART:20261110T150000Z\r\n\
DTEND:20261110T160000Z\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";

    #[test]
    fn the_meeting_under_way_is_found_with_who_was_invited() {
        let event = current_event(FEED, at("2026-11-10 15:05")).unwrap();
        assert_eq!(event.title, "Pricing review, Acme");
        assert_eq!(event.attendees, ["Sam Lee", "Joey Tran"]);
    }

    #[test]
    fn a_meeting_about_to_start_counts_and_one_long_over_does_not() {
        assert!(current_event(FEED, at("2026-11-10 14:52")).is_some());
        assert!(current_event(FEED, at("2026-11-10 14:30")).is_none());
        assert!(current_event(FEED, at("2026-11-10 15:45")).is_none());
    }

    #[test]
    fn folded_lines_are_joined() {
        let feed = "BEGIN:VEVENT\nSUMMARY:Quarterly plan\n ning with Sam\nDTSTART:20261110T150000Z\nEND:VEVENT\n";
        let event = current_event(feed, at("2026-11-10 15:10")).unwrap();
        assert_eq!(event.title, "Quarterly planning with Sam");
    }

    #[test]
    fn a_weekly_meeting_is_found_on_a_later_week() {
        let feed = "BEGIN:VEVENT\nSUMMARY:Weekly sync\nDTSTART:20261103T150000Z\nDTEND:20261103T153000Z\nRRULE:FREQ=WEEKLY;BYDAY=TU\nEND:VEVENT\n";
        assert!(current_event(feed, at("2026-11-10 15:10")).is_some());
        assert!(current_event(feed, at("2026-11-11 15:10")).is_none());
    }
}
