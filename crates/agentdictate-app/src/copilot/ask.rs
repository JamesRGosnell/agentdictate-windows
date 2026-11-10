//! Questions asked outside a meeting: across saved meetings from the
//! Meetings page, and of the user's documents from Settings, so the user can
//! see what the copilot would answer, and from where, before a call.

use std::{
    path::Path,
    sync::{Arc, atomic::AtomicBool},
};

use agentdictate_core::{CopilotEffort, CopilotSettings};

use super::{
    knowledge::Knowledge,
    providers::{AnswerProvider, AnswerRequest, complete},
};

/// What a question is answered from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    /// Notes of earlier meetings.
    Meetings,
    /// The knowledge folder.
    Documents,
}

impl Source {
    const fn noun(self) -> &'static str {
        match self {
            Self::Meetings => "notes of my earlier meetings",
            Self::Documents => "documents",
        }
    }

    const fn example(self) -> &'static str {
        match self {
            Self::Meetings => "(Meeting 2026-11-03 0900 Pricing review)",
            Self::Documents => "(pricing.md)",
        }
    }
}

fn system_prompt(source: Source) -> String {
    format!(
        "You answer my question using only the excerpts from the {} that I give you. Be brief: the answer \
first, then at most four short bullets. Quote figures, names, and dates exactly. After each fact, name where it \
comes from in parentheses, using the label in square brackets above the excerpt, like {}. If the excerpts do \
not answer the question, say so in one sentence and say what is missing; never guess or use outside knowledge.",
        source.noun(),
        source.example()
    )
}

/// The request for a question about `excerpts`, or `None` when nothing in
/// the source bears on it.
fn request(
    settings: &CopilotSettings,
    question: &str,
    excerpts: &str,
    source: Source,
) -> Option<AnswerRequest> {
    if excerpts.trim().is_empty() {
        return None;
    }
    Some(AnswerRequest {
        system: system_prompt(source),
        user: format!(
            "<excerpts>\n{}\n</excerpts>\n\nMy question: {}",
            excerpts.trim(),
            question.trim()
        ),
        image_jpeg: None,
        model: settings.fast_model(),
        effort: CopilotEffort::Low,
        web_search: false,
        max_output_tokens: 1_024,
        cache_key: "agentdictate-ask".into(),
    })
}

/// Answers a question from the loaded knowledge, in Markdown.
pub(crate) fn answer_from(
    provider: &dyn AnswerProvider,
    settings: &CopilotSettings,
    knowledge: &Knowledge,
    question: &str,
    source: Source,
) -> Result<String, String> {
    let question = question.trim();
    if question.is_empty() {
        return Err("Type a question first.".into());
    }
    let excerpts = match source {
        Source::Meetings => knowledge.meeting_excerpts(question),
        Source::Documents => knowledge.document_excerpts(question),
    };
    let Some(mut request) = request(settings, question, &excerpts, source) else {
        return Ok(match source {
            Source::Meetings => {
                "None of your saved meetings mention that. Try the names or terms that were used in the call."
            }
            Source::Documents => {
                "Nothing in your knowledge folder covers that. Add a document that does, and the copilot will use it."
            }
        }
        .to_owned());
    };
    // The fast model answers; it is the one that drafts lines in a call.
    let mut fast = settings.clone();
    fast.set_answer_model(settings.fast_model());
    request.model = fast.answer_model();
    let text = complete(provider, &request, &AtomicBool::new(false))?;
    let text = text.trim();
    if text.is_empty() {
        return Err("The model returned no answer.".into());
    }
    Ok(text.to_owned())
}

/// Answers a question with the user's own engine and saved settings.
#[cfg(windows)]
pub(crate) fn answer(
    config_file: &Path,
    knowledge_dir: &Path,
    meetings_dir: &Path,
    question: &str,
    source: Source,
) -> Result<String, String> {
    use crate::codex_subscription::ChatGptCredentials;

    let settings = super::store::load(&super::store::settings_path(config_file));
    let dictation_key = agentdictate_runtime::load_settings(config_file)
        .map(|dictation| dictation.openai_api_key.trim().to_owned())
        .unwrap_or_default();
    let chatgpt = Arc::new(ChatGptCredentials::discover());
    let mut fast = settings.clone();
    fast.set_answer_model(settings.fast_model());
    let provider = super::providers::provider_for(&fast, &chatgpt, &dictation_key, &[])?;
    let knowledge = match source {
        Source::Meetings => Knowledge::load(Path::new(""), Some(meetings_dir)),
        Source::Documents => Knowledge::load(knowledge_dir, None),
    };
    answer_from(provider.as_ref(), &settings, &knowledge, question, source)
}

#[cfg(not(windows))]
pub(crate) fn answer(
    _config_file: &Path,
    _knowledge_dir: &Path,
    _meetings_dir: &Path,
    _question: &str,
    _source: Source,
) -> Result<String, String> {
    let _ = Arc::new(());
    Err("Asking is available on Windows.".into())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use agentdictate_core::{Speaker, TranscriptLine, VoiceNames, meeting_notes_markdown};

    use super::super::stream::StreamEvent;
    use super::*;

    struct Recorder {
        requests: Mutex<Vec<AnswerRequest>>,
    }

    impl AnswerProvider for Recorder {
        fn stream(
            &self,
            request: &AnswerRequest,
            _cancel: &AtomicBool,
            sink: &mut dyn FnMut(StreamEvent),
        ) -> Result<(), String> {
            self.requests.lock().unwrap().push(request.clone());
            sink(StreamEvent::Text("They have **$50k** set aside.".into()));
            sink(StreamEvent::Done);
            Ok(())
        }
    }

    fn recorder() -> Recorder {
        Recorder {
            requests: Mutex::new(Vec::new()),
        }
    }

    #[test]
    fn a_question_across_meetings_is_answered_from_their_notes_alone() {
        let folder = tempfile::tempdir().unwrap();
        let line = |id, text: &str| TranscriptLine {
            id,
            speaker: Speaker::Them,
            text: text.into(),
            at_ms: id * 1_000,
            voice: None,
        };
        let notes = meeting_notes_markdown(
            "Acme renewal",
            "2026-11-03 09:00",
            60_000,
            Some("Acme wants to renew."),
            &[line(
                1,
                "Our budget for the renewal is fifty thousand dollars.",
            )],
            &VoiceNames::new(),
            &[],
        );
        std::fs::write(folder.path().join("2026-11-03 0900 Acme renewal.md"), notes).unwrap();
        let knowledge = Knowledge::load(Path::new(""), Some(folder.path()));
        let provider = recorder();
        let settings = CopilotSettings::default();
        let answer = answer_from(
            &provider,
            &settings,
            &knowledge,
            "What budget did Acme mention for the renewal?",
            Source::Meetings,
        )
        .unwrap();
        assert_eq!(answer, "They have **$50k** set aside.");
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].user.contains("fifty thousand dollars"));
        assert!(
            requests[0]
                .user
                .contains("[Meeting 2026-11-03 0900 Acme renewal]")
        );
        assert!(requests[0].system.contains("never guess"));
        assert!(!requests[0].web_search);
        assert_eq!(requests[0].model, settings.fast_model());
    }

    #[test]
    fn a_question_nothing_covers_is_answered_without_asking_a_model() {
        let folder = tempfile::tempdir().unwrap();
        std::fs::write(folder.path().join("pricing.md"), "Team plan: $49 per seat.").unwrap();
        let knowledge = Knowledge::load(folder.path(), None);
        let provider = recorder();
        let settings = CopilotSettings::default();
        let answer = answer_from(
            &provider,
            &settings,
            &knowledge,
            "Which airports do you fly from?",
            Source::Documents,
        )
        .unwrap();
        assert!(answer.contains("Nothing in your knowledge folder"));
        assert!(provider.requests.lock().unwrap().is_empty());
        // What the documents do cover is asked, with the document named.
        answer_from(
            &provider,
            &settings,
            &knowledge,
            "What does the Team plan cost per seat?",
            Source::Documents,
        )
        .unwrap();
        let requests = provider.requests.lock().unwrap();
        assert!(requests[0].user.contains("[pricing.md]"));
        assert!(answer_from(&provider, &settings, &knowledge, "  ", Source::Documents).is_err());
    }
}
