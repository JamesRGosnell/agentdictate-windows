//! Toolkit-free state for the meeting copilot window.

use agentdictate_core::{
    AnswerSource, AnswerStatus, CopilotEvent, CopilotInsights, CopilotSettings, CopilotShortcuts,
    FollowUp, KnowledgeSummary, ListenState, ModelChoice, RewriteStyle, Speaker, SpeakerLabels,
    TranscriptLine, VoiceNames, researched_follow_up,
};

/// Logical window metrics. The window resizes between the bar alone and the
/// bar with its panel so the transparent area never blocks the meeting app.
pub const COPILOT_WIDTH: f32 = 720.0;
pub const COPILOT_BAR_HEIGHT: f32 = 54.0;
pub const COPILOT_PANEL_HEIGHT: f32 = 480.0;
pub const COPILOT_PANEL_GAP: f32 = 6.0;
pub const COPILOT_TOP_MARGIN: f32 = 10.0;

/// Number of recent level samples kept for each mini waveform.
pub const LEVEL_HISTORY: usize = 14;

/// A visible, rounded part of the copilot window in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CopilotShape {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub radius: f32,
}

/// Host-window operations the copilot UI needs from the platform.
pub trait CopilotWindowHost: 'static {
    /// Sets the logical client size without moving or activating the window.
    fn resize(&self, width: f32, height: f32);
    fn set_visible(&self, visible: bool);
    /// Makes the window keyboard-focusable so the user can type.
    fn begin_typing(&self);
    /// Returns to the non-activating overlay, optionally refocusing the app
    /// that was active before typing began.
    fn end_typing(&self, restore_focus: bool);
    /// Includes or excludes the window from screen capture.
    fn set_capture_excluded(&self, excluded: bool);
    /// Limits the window to its visible shapes, so clicks on the transparent
    /// area around the bar and panel reach the windows underneath.
    fn set_shape(&self, _shapes: &[CopilotShape]) {}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnswerCardModel {
    pub id: u64,
    pub title: String,
    pub body: String,
    /// The fast model's draft, shown above the researched answer.
    pub instant: String,
    pub instant_active: bool,
    pub status: AnswerStatus,
    pub sources: Vec<AnswerSource>,
    pub used_screen: bool,
    pub model: String,
    pub automatic: bool,
    /// Rewrites of the line to say, oldest first.
    pub versions: Vec<String>,
    /// The version shown: 0 for the original, then each rewrite.
    pub shown: usize,
    /// The style of the rewrite streaming in.
    pub rewriting: Option<RewriteStyle>,
}

impl AnswerCardModel {
    /// The line to say as first written: the instant draft, the correction
    /// that replaced it, or the answer itself.
    #[must_use]
    pub fn original_line(&self) -> String {
        if self.instant.trim().is_empty() {
            return self.body.trim().to_owned();
        }
        match researched_follow_up(&self.body) {
            FollowUp::Correction(line) if !line.is_empty() => line,
            _ => self.instant.trim().to_owned(),
        }
    }

    /// The line to say as shown now.
    #[must_use]
    pub fn line_to_say(&self) -> String {
        match self.shown {
            0 => self.original_line(),
            version => self.versions.get(version - 1).cloned().unwrap_or_default(),
        }
    }

    /// Whether the line to say is complete and can be rewritten.
    #[must_use]
    pub fn line_ready(&self) -> bool {
        let written = if self.instant.trim().is_empty() {
            self.status == AnswerStatus::Done
        } else {
            !self.instant_active
        };
        written && self.rewriting.is_none() && !self.line_to_say().trim().is_empty()
    }

    #[must_use]
    pub fn status_label(&self) -> Option<String> {
        match &self.status {
            AnswerStatus::Thinking => Some(if self.used_screen {
                "Reading your screen and the conversation".to_owned()
            } else {
                "Thinking".to_owned()
            }),
            AnswerStatus::Searching(query) if query == "Searching the web" => {
                Some("Searching the web".to_owned())
            }
            AnswerStatus::Searching(query) => Some(format!("Searching \u{201c}{query}\u{201d}")),
            AnswerStatus::Streaming | AnswerStatus::Done => None,
            AnswerStatus::Failed(message) => Some(message.clone()),
        }
    }
}

/// What the window should do after applying an engine event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopilotEffect {
    None,
    ToggleVisibility,
    Show,
    FocusAsk,
    ScrollTranscript,
    Exit,
}

#[derive(Clone, Debug)]
pub struct CopilotViewModel {
    pub listen: ListenState,
    pub levels_me: Vec<f32>,
    pub levels_them: Vec<f32>,
    pub speaking_me: bool,
    pub speaking_them: bool,
    pub transcript: Vec<TranscriptLine>,
    /// Newest last.
    pub answers: Vec<AnswerCardModel>,
    pub insights: CopilotInsights,
    pub settings: CopilotSettings,
    pub models: Vec<ModelChoice>,
    pub shortcuts: CopilotShortcuts,
    pub notice: Option<String>,
    pub ending: bool,
    pub notes_path: Option<String>,
    /// The meeting summary shown when the session ends.
    pub wrap_up: Option<String>,
    pub follow_up: Option<String>,
    pub knowledge: KnowledgeSummary,
    /// People's names on the other side, by voice.
    pub speaker_names: VoiceNames,
    /// Names the meeting window showed, offered when naming a voice.
    pub seen_names: Vec<String>,
    pub session_started_unix_ms: i64,
}

impl CopilotViewModel {
    #[must_use]
    pub fn new(settings: CopilotSettings, session_started_unix_ms: i64) -> Self {
        let models = settings.provider.suggested_models();
        Self {
            listen: ListenState::Starting,
            levels_me: vec![0.0; LEVEL_HISTORY],
            levels_them: vec![0.0; LEVEL_HISTORY],
            speaking_me: false,
            speaking_them: false,
            transcript: Vec::new(),
            answers: Vec::new(),
            insights: CopilotInsights::default(),
            settings,
            models,
            shortcuts: CopilotShortcuts::default(),
            notice: None,
            ending: false,
            notes_path: None,
            wrap_up: None,
            follow_up: None,
            knowledge: KnowledgeSummary::default(),
            speaker_names: VoiceNames::new(),
            seen_names: Vec::new(),
            session_started_unix_ms,
        }
    }

    /// How the transcript names who spoke.
    #[must_use]
    pub fn speaker_labels(&self) -> SpeakerLabels {
        SpeakerLabels::new(&self.transcript, &self.speaker_names)
    }

    /// Names to offer when naming a voice: people the meeting window showed
    /// who have no voice yet.
    #[must_use]
    pub fn name_suggestions(&self) -> Vec<String> {
        self.seen_names
            .iter()
            .filter(|name| {
                !self
                    .speaker_names
                    .values()
                    .any(|taken| taken.eq_ignore_ascii_case(name))
            })
            .cloned()
            .collect()
    }

    /// Starts rewriting an answer's line to say as shown; returns the line.
    pub fn begin_rewrite(&mut self, id: u64, style: RewriteStyle) -> Option<String> {
        let answer = self.answer_mut(id)?;
        if !answer.line_ready() {
            return None;
        }
        let line = answer.line_to_say();
        answer.versions.push(String::new());
        answer.shown = answer.versions.len();
        answer.rewriting = Some(style);
        Some(line)
    }

    /// Shows the previous or next version of an answer's line; returns the
    /// rewrite now kept, or `None` inside for the original.
    pub fn step_version(&mut self, id: u64, forward: bool) -> Option<Option<String>> {
        let answer = self.answer_mut(id)?;
        if answer.rewriting.is_some() {
            return None;
        }
        let next = if forward {
            answer.shown + 1
        } else {
            answer.shown.checked_sub(1)?
        };
        if next > answer.versions.len() {
            return None;
        }
        answer.shown = next;
        Some(
            next.checked_sub(1)
                .map(|index| answer.versions[index].clone()),
        )
    }

    /// Names a voice right away; the engine confirms it.
    pub fn name_speaker(&mut self, voice: u8, name: &str) {
        let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
        if name.is_empty() {
            self.speaker_names.remove(&voice);
        } else {
            self.speaker_names.insert(voice, name);
        }
    }

    #[must_use]
    pub fn latest_answer(&self) -> Option<&AnswerCardModel> {
        self.answers.last()
    }

    #[must_use]
    pub fn answer_active(&self) -> bool {
        self.answers
            .last()
            .is_some_and(|answer| answer.status.is_active())
    }

    /// Label of the current answer model, as shown in the panel header.
    #[must_use]
    pub fn model_label(&self) -> String {
        let id = self.settings.answer_model();
        self.models
            .iter()
            .find(|model| model.id == id)
            .map_or(id, |model| model.label.clone())
    }

    fn push_level(levels: &mut Vec<f32>, level: f32) {
        if level < 0.0 {
            return;
        }
        levels.push(level.clamp(0.0, 1.0));
        if levels.len() > LEVEL_HISTORY {
            levels.remove(0);
        }
    }

    fn answer_mut(&mut self, id: u64) -> Option<&mut AnswerCardModel> {
        self.answers.iter_mut().rev().find(|answer| answer.id == id)
    }

    pub fn clear_answers(&mut self) {
        self.answers.clear();
    }

    pub fn apply(&mut self, event: CopilotEvent) -> CopilotEffect {
        match event {
            CopilotEvent::Listen(state) => {
                if !matches!(state, ListenState::Listening | ListenState::Starting) {
                    self.speaking_me = false;
                    self.speaking_them = false;
                }
                self.listen = state;
            }
            CopilotEvent::Levels { me, them } => {
                Self::push_level(&mut self.levels_me, me);
                Self::push_level(&mut self.levels_them, them);
            }
            CopilotEvent::Speaking { speaker, active } => match speaker {
                Speaker::Me => self.speaking_me = active,
                Speaker::Them => self.speaking_them = active,
            },
            CopilotEvent::Transcript(line) => {
                let position = self
                    .transcript
                    .iter()
                    .position(|existing| existing.at_ms > line.at_ms)
                    .unwrap_or(self.transcript.len());
                self.transcript.insert(position, line);
                return CopilotEffect::ScrollTranscript;
            }
            CopilotEvent::AnswerStarted {
                id,
                title,
                used_screen,
                model,
                automatic,
            } => {
                self.answers.push(AnswerCardModel {
                    id,
                    title,
                    body: String::new(),
                    instant: String::new(),
                    instant_active: false,
                    status: AnswerStatus::Thinking,
                    sources: Vec::new(),
                    used_screen,
                    model,
                    automatic,
                    versions: Vec::new(),
                    shown: 0,
                    rewriting: None,
                });
                if self.answers.len() > 30 {
                    self.answers.remove(0);
                }
            }
            CopilotEvent::AnswerStatus { id, status } => {
                if let Some(answer) = self.answer_mut(id) {
                    // A late "searching" update must not hide streamed text.
                    let regress =
                        matches!(status, AnswerStatus::Searching(_)) && !answer.body.is_empty();
                    let reopened = answer.status == AnswerStatus::Done && status.is_active();
                    if !(regress || reopened) {
                        answer.status = status;
                    }
                }
            }
            CopilotEvent::AnswerDelta { id, text } => {
                if let Some(answer) = self.answer_mut(id) {
                    answer.body.push_str(&text);
                    if matches!(
                        answer.status,
                        AnswerStatus::Thinking | AnswerStatus::Searching(_)
                    ) {
                        answer.status = AnswerStatus::Streaming;
                    }
                }
            }
            CopilotEvent::InstantDelta { id, text } => {
                if let Some(answer) = self.answer_mut(id) {
                    answer.instant.push_str(&text);
                    answer.instant_active = true;
                }
            }
            CopilotEvent::InstantFinished { id } => {
                if let Some(answer) = self.answer_mut(id) {
                    answer.instant_active = false;
                }
            }
            CopilotEvent::RewriteDelta { id, text } => {
                if let Some(answer) = self.answer_mut(id)
                    && answer.rewriting.is_some()
                    && let Some(version) = answer.versions.last_mut()
                {
                    version.push_str(&text);
                }
            }
            CopilotEvent::RewriteFinished { id, error } => {
                if let Some(answer) = self.answer_mut(id) {
                    answer.rewriting = None;
                    if answer
                        .versions
                        .last()
                        .is_some_and(|version| error.is_some() || version.trim().is_empty())
                    {
                        answer.versions.pop();
                        answer.shown = answer.shown.min(answer.versions.len());
                    }
                }
                if let Some(error) = error {
                    self.notice = Some(error);
                }
            }
            CopilotEvent::AnswerSource { id, source } => {
                if let Some(answer) = self.answer_mut(id)
                    && !answer
                        .sources
                        .iter()
                        .any(|existing| existing.url == source.url)
                {
                    answer.sources.push(source);
                }
            }
            CopilotEvent::Insights(insights) => self.insights = insights,
            CopilotEvent::Settings(settings) => {
                let provider_changed = settings.provider != self.settings.provider;
                self.settings = *settings;
                if provider_changed {
                    self.models = self.settings.provider.suggested_models();
                }
            }
            CopilotEvent::Models(models) => {
                if !models.is_empty() {
                    self.models = models;
                }
            }
            CopilotEvent::Shortcuts(shortcuts) => self.shortcuts = shortcuts,
            CopilotEvent::Notice(notice) => self.notice = Some(notice),
            CopilotEvent::ToggleVisibility => return CopilotEffect::ToggleVisibility,
            CopilotEvent::Show => return CopilotEffect::Show,
            CopilotEvent::FocusAsk => return CopilotEffect::FocusAsk,
            CopilotEvent::Ending => self.ending = true,
            CopilotEvent::Knowledge(knowledge) => self.knowledge = knowledge,
            CopilotEvent::Speakers { names, seen } => {
                self.speaker_names = names;
                self.seen_names = seen;
            }
            CopilotEvent::NotesSaved {
                path,
                summary,
                follow_up,
            } => {
                self.notes_path = Some(path);
                self.wrap_up = summary;
                self.follow_up = follow_up;
            }
            CopilotEvent::Exit => return CopilotEffect::Exit,
        }
        CopilotEffect::None
    }
}

/// Shortens a URL to its host for source chips.
#[must_use]
pub fn source_host(url: &str) -> String {
    let without_scheme = url.split("://").nth(1).unwrap_or(url);
    let host = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(without_scheme);
    host.trim_start_matches("www.").to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> CopilotViewModel {
        CopilotViewModel::new(CopilotSettings::default(), 0)
    }

    #[test]
    fn answers_stream_text_sources_and_final_status() {
        let mut view = model();
        view.apply(CopilotEvent::AnswerStarted {
            id: 7,
            title: "What should I say?".into(),
            used_screen: true,
            model: "gpt-6.1-sol".into(),
            automatic: false,
        });
        assert_eq!(
            view.latest_answer().unwrap().status_label().as_deref(),
            Some("Reading your screen and the conversation")
        );
        view.apply(CopilotEvent::AnswerStatus {
            id: 7,
            status: AnswerStatus::Searching("q3 revenue".into()),
        });
        assert_eq!(
            view.latest_answer().unwrap().status_label().as_deref(),
            Some("Searching \u{201c}q3 revenue\u{201d}")
        );
        view.apply(CopilotEvent::AnswerDelta {
            id: 7,
            text: "Say ".into(),
        });
        view.apply(CopilotEvent::AnswerDelta {
            id: 7,
            text: "this.".into(),
        });
        view.apply(CopilotEvent::AnswerStatus {
            id: 7,
            status: AnswerStatus::Searching("late".into()),
        });
        let source = AnswerSource {
            title: "Example".into(),
            url: "https://www.example.com/a".into(),
        };
        view.apply(CopilotEvent::AnswerSource {
            id: 7,
            source: source.clone(),
        });
        view.apply(CopilotEvent::AnswerSource { id: 7, source });
        view.apply(CopilotEvent::AnswerStatus {
            id: 7,
            status: AnswerStatus::Done,
        });
        let answer = view.latest_answer().unwrap();
        assert_eq!(answer.body, "Say this.");
        assert_eq!(answer.sources.len(), 1);
        assert_eq!(answer.status, AnswerStatus::Done);
        assert!(!view.answer_active());
    }

    #[test]
    fn instant_drafts_stream_beside_the_researched_answer() {
        let mut view = model();
        view.apply(CopilotEvent::AnswerStarted {
            id: 3,
            title: "Q".into(),
            used_screen: false,
            model: "gpt-6-sol".into(),
            automatic: true,
        });
        view.apply(CopilotEvent::InstantDelta {
            id: 3,
            text: "Quick ".into(),
        });
        view.apply(CopilotEvent::InstantDelta {
            id: 3,
            text: "line".into(),
        });
        let answer = view.latest_answer().unwrap();
        assert_eq!(answer.instant, "Quick line");
        assert!(answer.instant_active);
        assert_eq!(answer.status, AnswerStatus::Thinking);
        view.apply(CopilotEvent::InstantFinished { id: 3 });
        assert!(!view.latest_answer().unwrap().instant_active);
    }

    #[test]
    fn transcript_lines_insert_in_time_order() {
        let mut view = model();
        for (id, at_ms) in [(1, 5_000), (2, 1_000), (3, 3_000)] {
            assert_eq!(
                view.apply(CopilotEvent::Transcript(TranscriptLine {
                    id,
                    speaker: Speaker::Them,
                    text: format!("line {id}"),
                    at_ms,
                    voice: None,
                })),
                CopilotEffect::ScrollTranscript
            );
        }
        let order: Vec<u64> = view.transcript.iter().map(|line| line.id).collect();
        assert_eq!(order, vec![2, 3, 1]);
    }

    #[test]
    fn levels_keep_a_bounded_history_and_ignore_other_sources() {
        let mut view = model();
        for _ in 0..40 {
            view.apply(CopilotEvent::Levels {
                me: 0.5,
                them: -1.0,
            });
        }
        assert_eq!(view.levels_me.len(), LEVEL_HISTORY);
        assert!(view.levels_them.iter().all(|level| *level == 0.0));
    }

    #[test]
    fn visibility_and_exit_requests_become_effects() {
        let mut view = model();
        assert_eq!(
            view.apply(CopilotEvent::ToggleVisibility),
            CopilotEffect::ToggleVisibility
        );
        assert_eq!(view.apply(CopilotEvent::FocusAsk), CopilotEffect::FocusAsk);
        assert_eq!(view.apply(CopilotEvent::Exit), CopilotEffect::Exit);
    }

    #[test]
    fn provider_changes_reset_model_suggestions_until_the_catalog_loads() {
        let mut view = model();
        let settings = CopilotSettings {
            provider: agentdictate_core::CopilotProvider::AnthropicApi,
            ..CopilotSettings::default()
        };
        view.apply(CopilotEvent::Settings(Box::new(settings)));
        assert_eq!(view.model_label(), "Claude Opus 5.5");
        view.apply(CopilotEvent::Models(vec![ModelChoice::new(
            "claude-opus-5-5",
            "Opus",
        )]));
        assert_eq!(view.model_label(), "Opus");
    }

    #[test]
    fn source_hosts_drop_scheme_path_and_www() {
        assert_eq!(source_host("https://www.example.com/a?b"), "example.com");
        assert_eq!(source_host("blog.rust-lang.org/x"), "blog.rust-lang.org");
    }
}
