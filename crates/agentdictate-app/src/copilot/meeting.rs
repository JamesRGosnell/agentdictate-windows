//! Meeting detection from desktop signals.
//!
//! A call is assumed when a communication app or browser is actively using
//! the microphone, or when a meeting window is open. Audio processors that
//! hold the microphone permanently are ignored.

/// What the detector saw, for logs and the launch decision.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MeetingSignal {
    pub in_meeting: bool,
    pub reason: String,
}

const CALL_APPS: [&str; 26] = [
    "zoom",
    "teams",
    "webex",
    "slack",
    "discord",
    "skype",
    "chrome",
    "msedge",
    "firefox",
    "brave",
    "opera",
    "arc",
    "vivaldi",
    "whatsapp",
    "telegram",
    "signal",
    "gotomeeting",
    "g2mcomm",
    "ringcentral",
    "bluejeans",
    "chime",
    "around",
    "tuple",
    "gather",
    "facetime",
    "lync",
];

const ALWAYS_ON_PROCESSORS: [&str; 8] = [
    "nvidia broadcast",
    "nvbroadcast",
    "krisp",
    "voicemeeter",
    "sonar",
    "wavelink",
    "agentdictate",
    "cluely",
];

const MEETING_TITLES: [&str; 9] = [
    "zoom meeting",
    "zoom webinar",
    "meet - ",
    "google meet",
    "webex meeting",
    "huddle",
    "| microsoft teams",
    "teams meeting",
    "slack call",
];

fn short_name(app: &str) -> String {
    let lower = app.to_lowercase();
    lower
        .rsplit(['#', '\\', '/'])
        .next()
        .unwrap_or(&lower)
        .trim_end_matches(".exe")
        .to_owned()
}

/// Decides whether the user is in a meeting.
///
/// `active_microphone_apps` are app identifiers currently capturing audio
/// (packaged family names or `#`-separated executable paths).
#[must_use]
pub(crate) fn classify(
    active_microphone_apps: &[String],
    window_titles: &[String],
) -> MeetingSignal {
    for app in active_microphone_apps {
        let lower = app.to_lowercase();
        if ALWAYS_ON_PROCESSORS.iter().any(|name| lower.contains(name)) {
            continue;
        }
        let name = short_name(app);
        // Short names must match the executable; longer ones may appear
        // anywhere (packaged family names such as MSTeams_8wekyb3d8bbwe).
        if CALL_APPS.iter().any(|candidate| {
            name == *candidate
                || name.starts_with(&format!("{candidate}_"))
                || (candidate.len() >= 5 && name.contains(candidate))
        }) {
            return MeetingSignal {
                in_meeting: true,
                reason: format!("{name} is using the microphone"),
            };
        }
    }
    for title in window_titles {
        let lower = title.to_lowercase();
        if MEETING_TITLES.iter().any(|pattern| lower.contains(pattern))
            && (!lower.contains("| microsoft teams")
                || lower.contains("meeting")
                || lower.contains("call"))
        {
            return MeetingSignal {
                in_meeting: true,
                reason: format!("meeting window \"{title}\""),
            };
        }
    }
    MeetingSignal::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apps(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn call_apps_holding_the_microphone_mean_a_meeting() {
        let signal = classify(
            &apps(&["C:#Program Files#Google#Chrome#Application#chrome.exe"]),
            &[],
        );
        assert!(signal.in_meeting);
        assert!(signal.reason.contains("chrome"));
        assert!(classify(&apps(&["MSTeams_8wekyb3d8bbwe"]), &[]).in_meeting);
        assert!(
            classify(
                &apps(&["C:#Users#me#AppData#Roaming#Zoom#bin#Zoom.exe"]),
                &[]
            )
            .in_meeting
        );
        assert!(classify(&apps(&["C:#Program Files#Arc#Arc.exe"]), &[]).in_meeting);
        assert!(
            !classify(
                &apps(&["MicrosoftWindows.Client.CBS_cw5n1h2txyewy#SearchHost.exe"]),
                &[]
            )
            .in_meeting
        );
    }

    #[test]
    fn always_on_audio_processors_and_agentdictate_are_ignored() {
        assert!(
            !classify(
                &apps(&[
                    "C:#Program Files#NVIDIA Corporation#NVIDIA Broadcast#NVIDIA Broadcast.exe",
                    "C:#Users#me#AppData#Local#Programs#AgentDictate#agentdictated.exe",
                ]),
                &[],
            )
            .in_meeting
        );
    }

    #[test]
    fn meeting_windows_count_without_microphone_use() {
        assert!(classify(&[], &apps(&["Zoom Meeting"])).in_meeting);
        assert!(classify(&[], &apps(&["Meet - abc-defg-hij - Google Chrome"])).in_meeting);
        assert!(classify(&[], &apps(&["Weekly sync | Meeting | Microsoft Teams"])).in_meeting);
        assert!(!classify(&[], &apps(&["Chat | Microsoft Teams"])).in_meeting);
        assert!(!classify(&[], &apps(&["Inbox - Outlook"])).in_meeting);
    }
}
