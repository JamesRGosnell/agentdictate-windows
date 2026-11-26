use serde::{Deserialize, Serialize};

/// How a dictation the user just made ended, when its text was not pasted
/// normally. The recording pill shows it briefly; the ones that need the
/// user to act are also sent as a tray notification.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(tag = "notice", rename_all = "snake_case")]
pub enum DictationNotice {
    /// The text is on the clipboard but was not pasted.
    CopiedNotPasted,
    /// The paste stopped before any key was sent and nothing was copied;
    /// the dictation waits in Recovery.
    NotPasted,
    /// The paste was sent but could not be confirmed.
    MayNotHavePasted { copied: bool },
    /// The recording held no speech that could be transcribed.
    NothingHeard,
    /// The transcription service's usage limit was reached.
    LimitReached,
    /// The transcription service needs a sign-in or a valid API key.
    SignInNeeded,
    /// Transcription failed for another reason; the audio waits in Recovery.
    TranscriptionFailed,
    /// The microphone or recorder did not start.
    MicrophoneFailed,
    /// The recording stopped or could not be finalized; what was captured
    /// waits in Recovery.
    RecordingFailed,
}

impl DictationNotice {
    /// The pill's first line.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::CopiedNotPasted => "Copied \u{2014} press Ctrl+V",
            Self::NotPasted => "Not pasted",
            Self::MayNotHavePasted { .. } => "May not have pasted",
            Self::NothingHeard => "Didn't hear anything",
            Self::LimitReached => "Transcription limit reached",
            Self::SignInNeeded => "Sign in needed",
            Self::TranscriptionFailed => "Couldn't transcribe",
            Self::MicrophoneFailed => "Microphone didn't start",
            Self::RecordingFailed => "Recording stopped",
        }
    }

    /// The pill's smaller second line, when there is one.
    #[must_use]
    pub const fn detail(self) -> Option<&'static str> {
        match self {
            Self::NotPasted
            | Self::MayNotHavePasted { copied: false }
            | Self::TranscriptionFailed
            | Self::RecordingFailed => Some("Saved to Recovery"),
            Self::MayNotHavePasted { copied: true } => Some("Copied"),
            Self::CopiedNotPasted
            | Self::NothingHeard
            | Self::LimitReached
            | Self::SignInNeeded
            | Self::MicrophoneFailed => None,
        }
    }

    /// Both pill lines as one string, for logs and accessibility.
    #[must_use]
    pub fn pill_text(self) -> String {
        match self.detail() {
            Some(detail) => format!("{} \u{b7} {detail}", self.title()),
            None => self.title().to_owned(),
        }
    }

    /// Whether the user has to do something, so the notice is also sent as
    /// a tray notification. A silent recording explains itself.
    #[must_use]
    pub const fn needs_action(self) -> bool {
        !matches!(self, Self::NothingHeard)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [DictationNotice; 10] = [
        DictationNotice::CopiedNotPasted,
        DictationNotice::NotPasted,
        DictationNotice::MayNotHavePasted { copied: true },
        DictationNotice::MayNotHavePasted { copied: false },
        DictationNotice::NothingHeard,
        DictationNotice::LimitReached,
        DictationNotice::SignInNeeded,
        DictationNotice::TranscriptionFailed,
        DictationNotice::MicrophoneFailed,
        DictationNotice::RecordingFailed,
    ];

    #[test]
    fn pill_lines_stay_short() {
        for notice in ALL {
            assert!(notice.title().chars().count() <= 27, "{notice:?}");
            assert!(
                notice
                    .detail()
                    .is_none_or(|detail| detail.chars().count() <= 20),
                "{notice:?}"
            );
        }
    }

    #[test]
    fn pill_text_joins_the_two_lines() {
        assert_eq!(
            DictationNotice::TranscriptionFailed.pill_text(),
            "Couldn't transcribe \u{b7} Saved to Recovery"
        );
        assert_eq!(
            DictationNotice::MayNotHavePasted { copied: true }.pill_text(),
            "May not have pasted \u{b7} Copied"
        );
        assert_eq!(
            DictationNotice::CopiedNotPasted.pill_text(),
            "Copied \u{2014} press Ctrl+V"
        );
    }

    #[test]
    fn only_a_silent_recording_skips_the_tray_notification() {
        for notice in ALL {
            assert_eq!(
                notice.needs_action(),
                notice != DictationNotice::NothingHeard,
                "{notice:?}"
            );
        }
    }

    #[test]
    fn notices_round_trip_through_json() {
        for notice in ALL {
            let encoded = serde_json::to_string(&notice).unwrap();
            assert_eq!(
                serde_json::from_str::<DictationNotice>(&encoded).unwrap(),
                notice
            );
        }
    }
}
