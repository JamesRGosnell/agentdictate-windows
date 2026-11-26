//! Decides what the pill and the tray say about a dictation that was not
//! pasted normally.

use agentdictate_core::{DictationNotice, JobStage};
use agentdictate_runtime::{
    DeliveryStatus, ExternalError, ExternalFailureKind, RecordingJob, RuntimeError,
};

/// How a dictation the user just made ended.
#[derive(Clone, Copy, Debug)]
pub enum DictationEnd<'a> {
    /// Processing returned this job: delivered, silent, not sent, or
    /// delivered ambiguously.
    Finished(&'a RecordingJob),
    /// Processing failed with `error`; `job` is the job as persisted
    /// afterwards, when it could be read.
    Failed {
        error: &'a RuntimeError,
        job: Option<&'a RecordingJob>,
    },
    /// The microphone or recorder did not start.
    RecorderDidNotStart { message: &'a str },
    /// The recording stopped or could not be finalized; what was captured
    /// waits in Recovery.
    RecordingInterrupted,
}

/// What to say: the pill notice, and the tray notification's text when the
/// user has to act.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Notice {
    pub notice: DictationNotice,
    pub balloon: Option<String>,
}

const SAVED: &str = "It is saved in Recovery.";
const COPIED: &str = "It is on the clipboard: press Ctrl+V where you want it.";

/// The notice for a dictation's end, or `None` after a normal paste.
#[must_use]
pub fn dictation_notice(end: DictationEnd<'_>) -> Option<Notice> {
    let (notice, detail) = match end {
        DictationEnd::Finished(job) => finished(job)?,
        DictationEnd::Failed { error, job } => failed(error, job),
        DictationEnd::RecorderDidNotStart { message } => (
            DictationNotice::MicrophoneFailed,
            sentence(format!("The microphone didn't start: {message}")),
        ),
        DictationEnd::RecordingInterrupted => (
            DictationNotice::RecordingFailed,
            format!("The recording stopped unexpectedly. {SAVED}"),
        ),
    };
    Some(Notice {
        notice,
        balloon: notice.needs_action().then_some(detail),
    })
}

fn finished(job: &RecordingJob) -> Option<(DictationNotice, String)> {
    if job.delivery_status == DeliveryStatus::Ambiguous {
        return Some(ambiguous(job.copied_to_clipboard));
    }
    match job.stage {
        JobStage::Delivered if job.copied_to_clipboard && !job.paste_triggered => Some((
            DictationNotice::CopiedNotPasted,
            format!("The text wasn't pasted because the window changed. {COPIED}"),
        )),
        JobStage::Delivered => None,
        JobStage::NoSpeech => Some((DictationNotice::NothingHeard, String::new())),
        // Delivery stopped before any key was sent: the job waits in
        // Recovery, ready to paste again.
        _ => {
            let reason = job
                .error_message
                .as_deref()
                .map_or_else(|| "The text wasn't pasted.".to_owned(), sentence);
            Some(if job.copied_to_clipboard {
                (
                    DictationNotice::CopiedNotPasted,
                    format!("{reason} {COPIED}"),
                )
            } else {
                (DictationNotice::NotPasted, format!("{reason} {SAVED}"))
            })
        }
    }
}

fn ambiguous(copied: bool) -> (DictationNotice, String) {
    let next = if copied {
        "The text is on the clipboard if you need to paste it again."
    } else {
        SAVED
    };
    (
        DictationNotice::MayNotHavePasted { copied },
        format!("AgentDictate couldn't confirm the paste. {next}"),
    )
}

fn failed(error: &RuntimeError, job: Option<&RecordingJob>) -> (DictationNotice, String) {
    if let Some(job) = job
        && job.delivery_status == DeliveryStatus::Ambiguous
    {
        return ambiguous(job.copied_to_clipboard);
    }
    let error = root(error);
    if let RuntimeError::DeliveryBlocked(gate) = error {
        return (
            DictationNotice::NotPasted,
            format!("The paste was stopped before it started ({gate}). {SAVED}"),
        );
    }
    let transcribed = job.is_some_and(|job| !job.final_text.trim().is_empty());
    let (kind, message) = match error {
        RuntimeError::External(external) => (failure_kind(external), external.to_string()),
        other => (ExternalFailureKind::Other, other.to_string()),
    };
    let notice = match kind {
        ExternalFailureKind::SignInRequired => DictationNotice::SignInNeeded,
        ExternalFailureKind::LimitReached => DictationNotice::LimitReached,
        ExternalFailureKind::NothingRecognized => DictationNotice::NothingHeard,
        // The text exists, so a later step failed and nothing was pasted.
        ExternalFailureKind::Other if transcribed => DictationNotice::NotPasted,
        ExternalFailureKind::Other => DictationNotice::TranscriptionFailed,
    };
    (notice, format!("{} {SAVED}", sentence(message)))
}

/// The failure a `FailureNotRecorded` wraps.
fn root(error: &RuntimeError) -> &RuntimeError {
    match error {
        RuntimeError::FailureNotRecorded { error, .. } => root(error),
        error => error,
    }
}

/// The source's typed kind, or for an untyped failure a guess from its
/// message, since some adapters only pass a message along.
fn failure_kind(error: &ExternalError) -> ExternalFailureKind {
    match error.failure_kind() {
        None => ExternalFailureKind::NothingRecognized,
        Some(ExternalFailureKind::Other) => classify_message(&error.to_string()),
        Some(kind) => kind,
    }
}

/// Recognizes the limit and sign-in failures of services that report them
/// only in words.
#[must_use]
pub fn classify_message(message: &str) -> ExternalFailureKind {
    let message = message.to_lowercase();
    let mentions = |phrases: &[&str]| phrases.iter().any(|phrase| message.contains(phrase));
    if mentions(&[
        "limit reached",
        "usage limit",
        "rate limit",
        "quota",
        "too many requests",
    ]) {
        ExternalFailureKind::LimitReached
    } else if mentions(&[
        "sign in",
        "sign-in",
        "signed in",
        "unauthorized",
        "401",
        "authentication failed",
        "api key",
    ]) {
        ExternalFailureKind::SignInRequired
    } else if mentions(&["no speech"]) {
        ExternalFailureKind::NothingRecognized
    } else {
        ExternalFailureKind::Other
    }
}

/// `text` ending with a full stop, so a second sentence can follow it.
fn sentence(text: impl Into<String>) -> String {
    let mut text = text.into().trim().to_owned();
    if !text.ends_with(['.', '!', '?']) {
        text.push('.');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentdictate_core::{JobId, TranscriptionProvider};
    use agentdictate_runtime::DeliveryGateError;
    use chrono::Utc;

    fn job(stage: JobStage) -> RecordingJob {
        RecordingJob {
            options: None,
            id: JobId::new(),
            legacy_id: 1,
            started_at: Utc::now(),
            updated_at: Utc::now(),
            stage,
            audio_path: "dictation.wav".into(),
            duration_seconds: 3.0,
            transcription_provider: TranscriptionProvider::OpenAiApi,
            transcription_model: "model".into(),
            raw_transcript: String::new(),
            final_text: String::new(),
            copied_to_clipboard: false,
            paste_triggered: false,
            delivery_status: DeliveryStatus::NotAttempted,
            error_message: None,
            cleanup_error: None,
        }
    }

    fn notice_of(end: DictationEnd<'_>) -> Option<DictationNotice> {
        dictation_notice(end).map(|notice| notice.notice)
    }

    fn external(kind: ExternalFailureKind, message: &str) -> RuntimeError {
        RuntimeError::External(ExternalError::with_kind(kind, message))
    }

    #[test]
    fn a_normal_paste_says_nothing() {
        let delivered = RecordingJob {
            copied_to_clipboard: true,
            paste_triggered: true,
            delivery_status: DeliveryStatus::Submitted,
            ..job(JobStage::Delivered)
        };
        assert_eq!(dictation_notice(DictationEnd::Finished(&delivered)), None);
        let typed = RecordingJob {
            copied_to_clipboard: false,
            ..delivered
        };
        assert_eq!(dictation_notice(DictationEnd::Finished(&typed)), None);
    }

    #[test]
    fn a_copy_without_a_paste_asks_for_ctrl_v() {
        let copied = RecordingJob {
            copied_to_clipboard: true,
            paste_triggered: false,
            delivery_status: DeliveryStatus::Submitted,
            ..job(JobStage::Delivered)
        };
        let notice = dictation_notice(DictationEnd::Finished(&copied)).unwrap();
        assert_eq!(notice.notice, DictationNotice::CopiedNotPasted);
        assert!(notice.balloon.unwrap().contains("Ctrl+V"));
    }

    #[test]
    fn a_paste_not_sent_gives_its_reason() {
        let reason = "The focused window runs as administrator";
        let not_sent = RecordingJob {
            error_message: Some(reason.into()),
            ..job(JobStage::ReadyToDeliver)
        };
        let notice = dictation_notice(DictationEnd::Finished(&not_sent)).unwrap();
        assert_eq!(notice.notice, DictationNotice::NotPasted);
        assert_eq!(
            notice.notice.pill_text(),
            "Not pasted \u{b7} Saved to Recovery"
        );
        let balloon = notice.balloon.unwrap();
        assert!(balloon.starts_with(reason), "{balloon}");
        assert!(balloon.contains("Recovery"));

        let copied = RecordingJob {
            copied_to_clipboard: true,
            ..not_sent
        };
        let notice = dictation_notice(DictationEnd::Finished(&copied)).unwrap();
        assert_eq!(notice.notice, DictationNotice::CopiedNotPasted);
        let balloon = notice.balloon.unwrap();
        assert!(balloon.starts_with(reason) && balloon.contains("Ctrl+V"));
    }

    #[test]
    fn an_ambiguous_paste_may_not_have_landed() {
        for copied in [true, false] {
            let ambiguous = RecordingJob {
                copied_to_clipboard: copied,
                delivery_status: DeliveryStatus::Ambiguous,
                ..job(JobStage::Failed)
            };
            assert_eq!(
                notice_of(DictationEnd::Finished(&ambiguous)),
                Some(DictationNotice::MayNotHavePasted { copied })
            );
            let error = RuntimeError::External(ExternalError::new("injection failed"));
            assert_eq!(
                notice_of(DictationEnd::Failed {
                    error: &error,
                    job: Some(&ambiguous),
                }),
                Some(DictationNotice::MayNotHavePasted { copied })
            );
        }
    }

    #[test]
    fn silence_is_shown_on_the_pill_only() {
        let silent = job(JobStage::NoSpeech);
        let notice = dictation_notice(DictationEnd::Finished(&silent)).unwrap();
        assert_eq!(notice.notice, DictationNotice::NothingHeard);
        assert_eq!(notice.balloon, None);

        let unrecognized = external(
            ExternalFailureKind::NothingRecognized,
            "No speech was recognized. Audio is saved for another attempt.",
        );
        let notice = dictation_notice(DictationEnd::Failed {
            error: &unrecognized,
            job: None,
        })
        .unwrap();
        assert_eq!(notice.notice, DictationNotice::NothingHeard);
        assert_eq!(notice.balloon, None);
    }

    #[test]
    fn typed_transcription_failures_map_to_their_notice() {
        for (kind, expected) in [
            (
                ExternalFailureKind::LimitReached,
                DictationNotice::LimitReached,
            ),
            (
                ExternalFailureKind::SignInRequired,
                DictationNotice::SignInNeeded,
            ),
            (
                ExternalFailureKind::Other,
                DictationNotice::TranscriptionFailed,
            ),
        ] {
            let error = external(kind, "Something went wrong");
            let notice = dictation_notice(DictationEnd::Failed {
                error: &error,
                job: Some(&job(JobStage::Failed)),
            })
            .unwrap();
            assert_eq!(notice.notice, expected);
            assert_eq!(
                notice.balloon.as_deref(),
                Some("Something went wrong. It is saved in Recovery.")
            );
        }
    }

    #[test]
    fn a_failure_whose_record_failed_is_classified_by_its_cause() {
        let error = RuntimeError::FailureNotRecorded {
            error: Box::new(external(
                ExternalFailureKind::LimitReached,
                "ChatGPT transcription limit reached. Try again later.",
            )),
            record_error: rusqlite::Error::InvalidQuery,
        };
        assert_eq!(
            notice_of(DictationEnd::Failed {
                error: &error,
                job: None
            }),
            Some(DictationNotice::LimitReached)
        );
    }

    #[test]
    fn untyped_messages_are_classified_by_their_words() {
        for (message, expected) in [
            (
                "ChatGPT transcription limit reached. Try again later.",
                ExternalFailureKind::LimitReached,
            ),
            (
                "insufficient_quota: You exceeded your current quota",
                ExternalFailureKind::LimitReached,
            ),
            ("Rate limit exceeded", ExternalFailureKind::LimitReached),
            (
                "Codex is not signed in with ChatGPT.",
                ExternalFailureKind::SignInRequired,
            ),
            (
                "ChatGPT sign-in expired. Open Codex and sign in again.",
                ExternalFailureKind::SignInRequired,
            ),
            (
                "OpenAI request failed with status 401 Unauthorized",
                ExternalFailureKind::SignInRequired,
            ),
            (
                "No speech detected.",
                ExternalFailureKind::NothingRecognized,
            ),
            (
                "Could not reach OpenAI: connection reset",
                ExternalFailureKind::Other,
            ),
            ("", ExternalFailureKind::Other),
        ] {
            assert_eq!(classify_message(message), expected, "{message}");
        }
        let error = RuntimeError::External(ExternalError::new(
            "Live transcription failed: ChatGPT transcription limit reached.",
        ));
        assert_eq!(
            notice_of(DictationEnd::Failed {
                error: &error,
                job: None
            }),
            Some(DictationNotice::LimitReached)
        );
    }

    #[test]
    fn a_failure_after_transcription_says_the_text_was_not_pasted() {
        let error = RuntimeError::External(ExternalError::new("replacement processing failed"));
        let transcribed = RecordingJob {
            final_text: "Hello there.".into(),
            ..job(JobStage::Failed)
        };
        assert_eq!(
            notice_of(DictationEnd::Failed {
                error: &error,
                job: Some(&transcribed),
            }),
            Some(DictationNotice::NotPasted)
        );
        let blocked = RuntimeError::DeliveryBlocked(DeliveryGateError::new("overlay stuck"));
        let notice = dictation_notice(DictationEnd::Failed {
            error: &blocked,
            job: Some(&transcribed),
        })
        .unwrap();
        assert_eq!(notice.notice, DictationNotice::NotPasted);
        assert!(notice.balloon.unwrap().contains("overlay stuck"));
    }

    #[test]
    fn a_database_failure_while_transcribing_is_a_transcription_failure() {
        let error = RuntimeError::Database(rusqlite::Error::InvalidQuery);
        assert_eq!(
            notice_of(DictationEnd::Failed {
                error: &error,
                job: None
            }),
            Some(DictationNotice::TranscriptionFailed)
        );
    }

    #[test]
    fn recorder_failures_need_the_user() {
        let notice = dictation_notice(DictationEnd::RecorderDidNotStart {
            message: "No default microphone is available. Check Windows microphone settings.",
        })
        .unwrap();
        assert_eq!(notice.notice, DictationNotice::MicrophoneFailed);
        assert!(notice.balloon.unwrap().contains("No default microphone"));

        let notice = dictation_notice(DictationEnd::RecordingInterrupted).unwrap();
        assert_eq!(notice.notice, DictationNotice::RecordingFailed);
        assert!(notice.balloon.unwrap().contains("Recovery"));
    }
}
