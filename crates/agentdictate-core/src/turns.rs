//! Which of the other side's lines the user gets help answering.
//!
//! The copilot engine and the `--questions` evaluation share these rules, so
//! a change measured by the evaluation is the change users get.

use crate::{
    QUESTION_OPENERS, Speaker, TranscriptLine, echo_containment, looks_like_question, spoken_words,
    worth_judging,
};

/// Quiet after the other side's line before an answer to it is shown:
/// someone asking the user something waits; a rhetorical question runs on.
pub const QUESTION_PAUSE_MS: u64 = 1_000;

/// Other-side speech starting this long after a line ended means the
/// speaker kept talking rather than waiting for the user.
pub const RESUMED_SPEECH_MS: u64 = 300;

/// Other-side speech heard after a candidate turn.
#[derive(Clone, Copy, Debug)]
pub struct HeardSpan<'a> {
    pub start_ms: u64,
    pub end_ms: u64,
    /// The other side's voice, when voices are told apart.
    pub voice: Option<u8>,
    /// The span's transcript, once it is known.
    pub text: Option<&'a str>,
}

/// How much of the latest transcript the turn judge reads. A 3,000-character
/// cap measured no faster and no more accurate on the question evaluation.
pub const JUDGE_TRANSCRIPT_CHARS: usize = 8_000;

/// Backchannels from another voice this short do not end a turn.
const BACKCHANNEL_WORDS: usize = 3;
/// Untranscribed speech from another voice this brief is a backchannel.
const BACKCHANNEL_MS: u64 = 1_000;

/// Whether a short line is still plainly a question: "And onboarding?",
/// "What's your budget?".
fn short_question(text: &str) -> bool {
    let normalized = text.trim().to_lowercase();
    let words = spoken_words(&normalized);
    words.len() >= 2
        && (normalized.contains('?')
            || words
                .first()
                .is_some_and(|first| QUESTION_OPENERS.contains(first)))
}

/// Whether a line from the other side is worth asking the judge about.
#[must_use]
pub fn turn_candidate(text: &str) -> bool {
    worth_judging(text) || short_question(text)
}

/// Whether a line is worded so plainly as a question that its answer is
/// drafted before the judge confirms it. A question mark anywhere counts,
/// since a question is often followed by its reason.
#[must_use]
pub fn turn_likely_question(text: &str) -> bool {
    looks_like_question(text) || (turn_candidate(text) && text.contains('?'))
}

/// Whether `span` shows the other side kept talking after the turn that
/// ended at `turn_end_ms`, so the turn should not be answered. Another
/// voice's brief "mm-hmm" or "right" is not the speaker moving on.
#[must_use]
pub fn kept_talking(turn_end_ms: u64, turn_voice: Option<u8>, span: &HeardSpan<'_>) -> bool {
    if span.start_ms < turn_end_ms + RESUMED_SPEECH_MS {
        return false;
    }
    let other_voice = turn_voice.is_some() && span.voice.is_some() && span.voice != turn_voice;
    let backchannel = match span.text {
        Some(text) => spoken_words(&text.to_lowercase()).len() <= BACKCHANNEL_WORDS,
        None => span.end_ms.saturating_sub(span.start_ms) < BACKCHANNEL_MS,
    };
    !(other_voice && backchannel)
}

/// Other-side speech within this long of a microphone line can be its echo.
const ECHO_NEIGHBORHOOD_MS: u64 = 8_000;
/// Share of a microphone line's words heard from the other side nearby that
/// marks it as their audio leaking into the microphone.
const JUDGE_ECHO_CONTAINMENT: f32 = 0.5;

/// The transcript the turn judge reads: microphone lines that repeat what
/// the other side said nearby are their audio leaking into the microphone,
/// and would make the user look like part of a conversation they are only
/// hearing, such as a video.
/// Whether the microphone line at `index` repeats other-side speech heard
/// around the same time. Only nearby lines are compared.
fn is_echo_line(lines: &[TranscriptLine], index: usize) -> bool {
    let line = &lines[index];
    if line.speaker != Speaker::Me {
        return false;
    }
    let near = |other: &&TranscriptLine| other.at_ms.abs_diff(line.at_ms) <= ECHO_NEIGHBORHOOD_MS;
    let is_echo = |other: &TranscriptLine| {
        other.speaker == Speaker::Them
            && echo_containment(&line.text, &other.text) >= JUDGE_ECHO_CONTAINMENT
    };
    // Lines arrive in transcription order, which is close to time order.
    lines[..index]
        .iter()
        .rev()
        .take(12)
        .filter(near)
        .any(&is_echo)
        || lines[index + 1..]
            .iter()
            .take(12)
            .filter(near)
            .any(&is_echo)
}

#[must_use]
pub fn judge_transcript(lines: &[TranscriptLine]) -> Vec<TranscriptLine> {
    (0..lines.len())
        .filter(|&index| !is_echo_line(lines, index))
        .map(|index| lines[index].clone())
        .collect()
}

/// Milliseconds from the user's last own line to `now_ms`, ignoring echo.
#[must_use]
pub fn since_user_spoke_ms(lines: &[TranscriptLine], now_ms: u64) -> Option<u64> {
    (0..lines.len())
        .rev()
        .find(|&index| lines[index].speaker == Speaker::Me && !is_echo_line(lines, index))
        .map(|index| now_ms.saturating_sub(lines[index].at_ms))
}

/// How far back [`only_listening`] looks.
const LISTENING_WINDOW_MS: u64 = 5 * 60_000;
/// Microphone lines that echo the other side, at least this many...
const LISTENING_ECHO_LINES: usize = 3;
/// ...while the user said no more than this many words of their own, mean
/// the user is only listening, such as to a video playing on speakers.
const LISTENING_OWN_WORDS: usize = 12;

/// Whether the microphone has recently heard mostly the other side's audio
/// echoing back and hardly a word from the user, so nothing is said to them.
#[must_use]
pub fn only_listening(lines: &[TranscriptLine], now_ms: u64) -> bool {
    let since = now_ms.saturating_sub(LISTENING_WINDOW_MS);
    let mut echoes = 0;
    let mut own_words = 0;
    for index in 0..lines.len() {
        if lines[index].speaker != Speaker::Me || lines[index].at_ms < since {
            continue;
        }
        if is_echo_line(lines, index) {
            echoes += 1;
        } else {
            own_words += spoken_words(&lines[index].text.to_lowercase()).len();
        }
    }
    echoes >= LISTENING_ECHO_LINES && own_words <= LISTENING_OWN_WORDS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start_ms: u64) -> HeardSpan<'static> {
        HeardSpan {
            start_ms,
            end_ms: start_ms + 500,
            voice: None,
            text: None,
        }
    }

    #[test]
    fn later_speech_means_they_kept_talking() {
        assert!(kept_talking(1_000, None, &span(1_400)));
        assert!(!kept_talking(1_000, None, &span(1_100)));
    }

    #[test]
    fn another_voices_backchannel_does_not_end_the_turn() {
        let backchannel = HeardSpan {
            voice: Some(1),
            text: Some("Mm-hmm."),
            ..span(1_400)
        };
        assert!(!kept_talking(1_000, Some(0), &backchannel));
        let same_voice = HeardSpan {
            voice: Some(0),
            ..backchannel
        };
        assert!(kept_talking(1_000, Some(0), &same_voice));
        let answer = HeardSpan {
            text: Some("I can take that one, we keep it thirty days."),
            ..backchannel
        };
        assert!(kept_talking(1_000, Some(0), &answer));
    }

    fn line(speaker: Speaker, at_ms: u64, text: &str) -> TranscriptLine {
        TranscriptLine {
            id: at_ms,
            speaker,
            text: text.into(),
            at_ms,
            voice: None,
        }
    }

    #[test]
    fn the_judge_does_not_read_echo_as_the_user_speaking() {
        let lines = [
            line(
                Speaker::Them,
                0,
                "With your help, I'm ready to scale it. Walk me through your pricing.",
            ),
            line(Speaker::Me, 3_000, "Your app, I'm ready to scale it."),
            line(Speaker::Me, 20_000, "We price per seat, billed yearly."),
        ];
        let read = judge_transcript(&lines);
        assert_eq!(read.len(), 2);
        assert_eq!(since_user_spoke_ms(&lines[..2], 5_000), None);
        assert_eq!(since_user_spoke_ms(&lines, 25_000), Some(5_000));
    }

    #[test]
    fn short_plain_questions_are_judged() {
        assert!(turn_candidate("And onboarding?"));
        assert!(turn_candidate("What's your budget?"));
        assert!(!turn_candidate("Okay."));
        assert!(!turn_candidate("Sorry?"));
        assert!(turn_likely_question(
            "Do you have SOC 2? Our security team will ask."
        ));
    }

    fn heard(speaker: Speaker, text: &str, at_s: u64) -> TranscriptLine {
        TranscriptLine {
            id: at_s,
            speaker,
            text: text.into(),
            at_ms: at_s * 1_000,
            voice: None,
        }
    }

    #[test]
    fn a_microphone_hearing_only_echo_means_the_user_is_listening() {
        let mut lines = vec![
            heard(
                Speaker::Them,
                "Deal size for event planning was thirty four thousand",
                0,
            ),
            heard(Speaker::Me, "event planning was thirty four thousand", 1),
            heard(
                Speaker::Them,
                "What's holding you back right now in the business",
                10,
            ),
            heard(Speaker::Me, "holding you back right now", 11),
            heard(Speaker::Me, "Average.", 15),
            heard(
                Speaker::Them,
                "So why do you think this can scale to the next level",
                20,
            ),
            heard(Speaker::Me, "you think this can scale", 21),
        ];
        assert!(only_listening(&lines, 25_000));
        lines.push(heard(
            Speaker::Me,
            "Honestly I think the margins are the real problem we have to fix first",
            24,
        ));
        assert!(!only_listening(&lines, 25_000));
        // Echo long ago no longer counts.
        assert!(!only_listening(&lines[..4], 20 * 60_000));
    }
}
