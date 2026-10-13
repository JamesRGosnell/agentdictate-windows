//! Meeting copilot settings, the engine/UI message contract, and the
//! platform-independent prompt and transcript logic behind live assistance.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Where copilot answers come from. Every provider streams text; web search
/// and screen context are honored where the provider supports them.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum CopilotProvider {
    /// The ChatGPT account already signed in for subscription dictation.
    #[default]
    ChatgptSubscription,
    /// Anthropic Messages API with an API key.
    AnthropicApi,
    /// The locally installed Claude Code CLI and its Claude subscription.
    ClaudeCode,
    /// OpenAI Responses API with an API key.
    OpenaiApi,
}

impl CopilotProvider {
    pub const ALL: [Self; 4] = [
        Self::ChatgptSubscription,
        Self::AnthropicApi,
        Self::ClaudeCode,
        Self::OpenaiApi,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ChatgptSubscription => "ChatGPT",
            Self::AnthropicApi => "Claude API",
            Self::ClaudeCode => "Claude Code",
            Self::OpenaiApi => "OpenAI API",
        }
    }

    #[must_use]
    pub const fn detail(self) -> &'static str {
        match self {
            Self::ChatgptSubscription => "Uses your ChatGPT sign-in. No API key needed.",
            Self::AnthropicApi => "Fastest Claude route. Needs an Anthropic API key.",
            Self::ClaudeCode => "Uses your Claude subscription through the claude CLI.",
            Self::OpenaiApi => "Uses an OpenAI API key.",
        }
    }

    /// Model used for answers when the user has not chosen one.
    #[must_use]
    pub const fn default_answer_model(self) -> &'static str {
        match self {
            Self::ChatgptSubscription => "gpt-6-sol",
            Self::AnthropicApi => "claude-opus-5-5",
            Self::ClaudeCode => "opus",
            Self::OpenaiApi => "gpt-5.5",
        }
    }

    /// Low-latency model for insights, question detection, and notes.
    #[must_use]
    pub const fn default_fast_model(self) -> &'static str {
        match self {
            Self::ChatgptSubscription => "gpt-6-luna",
            Self::AnthropicApi => "claude-haiku-5-5",
            Self::ClaudeCode => "haiku",
            Self::OpenaiApi => "gpt-5.4-mini",
        }
    }

    /// Built-in model suggestions; the ChatGPT route replaces these with the
    /// live account catalog when it is available.
    #[must_use]
    pub fn suggested_models(self) -> Vec<ModelChoice> {
        let pairs: &[(&str, &str)] = match self {
            Self::ChatgptSubscription => &[
                ("gpt-6-sol", "GPT-6 Sol"),
                ("gpt-6.1-sol", "GPT-6.1 Sol"),
                ("gpt-6-astra", "GPT-6 Astra"),
                ("gpt-6-luna", "GPT-6 Luna"),
                ("gpt-5.6-sol", "GPT-5.6 Sol"),
                ("gpt-5.6-terra", "GPT-5.6 Terra"),
                ("gpt-5.6-luna", "GPT-5.6 Luna"),
            ],
            Self::AnthropicApi => &[
                ("claude-opus-5-5", "Claude Opus 5.5"),
                ("claude-sonnet-5-5", "Claude Sonnet 5.5"),
                ("claude-haiku-5-5", "Claude Haiku 5.5"),
                ("claude-fable-5-1", "Claude Fable 5.1"),
            ],
            Self::ClaudeCode => &[
                ("opus", "Opus (latest)"),
                ("sonnet", "Sonnet (latest)"),
                ("haiku", "Haiku (latest)"),
                ("fable", "Fable (latest)"),
            ],
            Self::OpenaiApi => &[
                ("gpt-5.5", "GPT-5.5"),
                ("gpt-5.6-luna", "GPT-5.6 Luna"),
                ("gpt-5.4-mini", "GPT-5.4 mini"),
            ],
        };
        pairs
            .iter()
            .map(|(id, label)| ModelChoice::new(*id, *label))
            .collect()
    }

    #[must_use]
    pub const fn supports_screen(self) -> bool {
        true
    }
}

/// A selectable model with a human label.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelChoice {
    pub id: String,
    pub label: String,
}

impl ModelChoice {
    #[must_use]
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
        }
    }
}

/// How much reasoning an answer may spend before speaking.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CopilotEffort {
    #[default]
    Low,
    Medium,
    High,
}

impl CopilotEffort {
    pub const ALL: [Self; 3] = [Self::Low, Self::Medium, Self::High];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Low => "Fast",
            Self::Medium => "Balanced",
            Self::High => "Deep",
        }
    }
}

/// Conversation presets that tune how the copilot frames its help.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CopilotMode {
    #[default]
    General,
    Sales,
    Interview,
    Recruiting,
    Lecture,
    Negotiation,
}

impl CopilotMode {
    pub const ALL: [Self; 6] = [
        Self::General,
        Self::Sales,
        Self::Interview,
        Self::Recruiting,
        Self::Lecture,
        Self::Negotiation,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Sales => "Sales call",
            Self::Interview => "Job interview",
            Self::Recruiting => "Hiring",
            Self::Lecture => "Lecture",
            Self::Negotiation => "Negotiation",
        }
    }

    #[must_use]
    pub const fn guidance(self) -> &'static str {
        match self {
            Self::General => {
                "Help the user participate confidently: answer questions, supply facts, and suggest crisp next lines."
            }
            Self::Sales => {
                "The user is selling. Handle objections with empathy and evidence, quantify value, surface discovery questions, and always suggest a clear next step."
            }
            Self::Interview => {
                "The user is the candidate in a job interview. Give answers in the user's voice using the STAR pattern where useful, keep them specific and honest, and highlight relevant experience from the user's background."
            }
            Self::Recruiting => {
                "The user is interviewing a candidate. Suggest probing follow-up questions, flag vague or inconsistent answers, and assess against the role."
            }
            Self::Lecture => {
                "The user is in a lecture or class. Explain concepts simply, define terms, give examples, and note what is likely important."
            }
            Self::Negotiation => {
                "The user is negotiating. Identify interests and leverage, suggest anchors and concessions, and keep the user calm, specific, and firm."
            }
        }
    }
}

/// One-click requests offered under the copilot bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum QuickAction {
    WhatToSay,
    FollowUps,
    Recap,
    FactCheck,
    ExplainScreen,
}

impl QuickAction {
    pub const ALL: [Self; 5] = [
        Self::WhatToSay,
        Self::FollowUps,
        Self::Recap,
        Self::FactCheck,
        Self::ExplainScreen,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::WhatToSay => "What should I say?",
            Self::FollowUps => "Follow-up questions",
            Self::Recap => "Recap so far",
            Self::FactCheck => "Fact-check",
            Self::ExplainScreen => "Explain my screen",
        }
    }

    /// Instruction appended after the transcript for this action.
    #[must_use]
    pub const fn instruction(self) -> &'static str {
        match self {
            Self::WhatToSay => {
                "What should I say or do right now? If they just asked me something, answer it directly in words I can say out loud. Start with the line to say, then at most three supporting bullets."
            }
            Self::FollowUps => {
                "Suggest three sharp, specific follow-up questions I could ask next, each with a one-line reason."
            }
            Self::Recap => {
                "Recap the conversation so far in at most five bullets: key points, decisions, numbers, and open questions or action items."
            }
            Self::FactCheck => {
                "Fact-check the most recent factual claims in the conversation. For each claim, say whether it is correct and give the correction or source in one line. Use the web when it helps."
            }
            Self::ExplainScreen => {
                "Look at my screen and explain what matters for this conversation: the key information, what it means, and anything I should mention."
            }
        }
    }

    #[must_use]
    pub const fn wants_screen(self) -> bool {
        matches!(self, Self::ExplainScreen | Self::WhatToSay)
    }
}

/// Persisted copilot preferences, stored beside the dictation settings in
/// `copilot.json` so the meeting assistant evolves independently.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CopilotSettings {
    pub provider: CopilotProvider,
    /// Answer model per provider; missing entries use the provider default.
    pub answer_models: BTreeMap<CopilotProvider, String>,
    /// Fast model per provider for insights, detection, and notes.
    pub fast_models: BTreeMap<CopilotProvider, String>,
    pub effort: CopilotEffort,
    /// A fast model drafts a reply while the answer model researches.
    pub instant_answers: bool,
    pub web_search: bool,
    pub screen_context: bool,
    pub auto_answer: bool,
    pub live_insights: bool,
    /// The dictation shortcut opens the copilot when a meeting is detected.
    pub launch_from_dictation_hotkey: bool,
    pub capture_microphone: bool,
    pub capture_system_audio: bool,
    /// Excludes the copilot window from screen sharing, recording, and
    /// screenshots.
    pub hide_from_screen_share: bool,
    pub mode: CopilotMode,
    /// Background the copilot should know about the user.
    pub profile: String,
    /// Extra standing instructions for every answer.
    pub custom_instructions: String,
    pub anthropic_api_key: String,
    pub openai_api_key: String,
    pub save_notes: bool,
    /// Optional ISO-639-1 transcription language; empty means automatic.
    pub language: String,
}

impl Default for CopilotSettings {
    fn default() -> Self {
        Self {
            provider: CopilotProvider::default(),
            answer_models: BTreeMap::new(),
            fast_models: BTreeMap::new(),
            effort: CopilotEffort::default(),
            instant_answers: true,
            web_search: true,
            screen_context: true,
            auto_answer: true,
            live_insights: true,
            launch_from_dictation_hotkey: true,
            capture_microphone: true,
            capture_system_audio: true,
            hide_from_screen_share: true,
            mode: CopilotMode::default(),
            profile: String::new(),
            custom_instructions: String::new(),
            anthropic_api_key: String::new(),
            openai_api_key: String::new(),
            save_notes: true,
            language: String::new(),
        }
    }
}

impl CopilotSettings {
    #[must_use]
    pub fn answer_model(&self) -> String {
        self.answer_models
            .get(&self.provider)
            .map(|model| model.trim())
            .filter(|model| !model.is_empty())
            .unwrap_or(self.provider.default_answer_model())
            .to_owned()
    }

    #[must_use]
    pub fn fast_model(&self) -> String {
        self.fast_models
            .get(&self.provider)
            .map(|model| model.trim())
            .filter(|model| !model.is_empty())
            .unwrap_or(self.provider.default_fast_model())
            .to_owned()
    }

    pub fn set_answer_model(&mut self, model: impl Into<String>) {
        self.answer_models.insert(self.provider, model.into());
    }

    pub fn set_fast_model(&mut self, model: impl Into<String>) {
        self.fast_models.insert(self.provider, model.into());
    }

    /// A copy safe to log or render: API keys are reduced to presence.
    #[must_use]
    pub fn redacted(&self) -> Self {
        let mut copy = self.clone();
        if !copy.anthropic_api_key.is_empty() {
            copy.anthropic_api_key = "<set>".into();
        }
        if !copy.openai_api_key.is_empty() {
            copy.openai_api_key = "<set>".into();
        }
        copy
    }
}

/// Who spoke a transcript line. The microphone is the user; system audio is
/// everyone else on the call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Speaker {
    Me,
    Them,
}

impl Speaker {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Me => "Me",
            Self::Them => "Them",
        }
    }

    #[must_use]
    pub const fn display(self) -> &'static str {
        match self {
            Self::Me => "You",
            Self::Them => "Them",
        }
    }
}

/// A finalized utterance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptLine {
    pub id: u64,
    pub speaker: Speaker,
    pub text: String,
    /// Milliseconds since the session started, at speech onset.
    pub at_ms: u64,
}

/// Listening state shown in the bar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListenState {
    Starting,
    Listening,
    Paused,
    Unavailable(String),
}

/// Progress of one streamed answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnswerStatus {
    Thinking,
    Searching(String),
    Streaming,
    Done,
    Failed(String),
}

impl AnswerStatus {
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(self, Self::Thinking | Self::Searching(_) | Self::Streaming)
    }
}

/// A web citation attached to an answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnswerSource {
    pub title: String,
    pub url: String,
}

/// Rolling meeting understanding from the fast model.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CopilotInsights {
    pub topic: String,
    pub summary: Vec<String>,
    pub question: Option<String>,
    pub suggestions: Vec<String>,
    #[serde(deserialize_with = "lenient_situation")]
    pub situation: Situation,
}

/// What the copilot is listening to, judged from the transcript.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Situation {
    /// Not enough to tell yet.
    #[default]
    Unclear,
    /// A live conversation the user takes part in.
    Conversation,
    /// Someone presenting to an audience that includes the user.
    Presentation,
    /// A recording the user is watching or listening to, such as a video.
    Media,
}

impl Situation {
    /// Whether questions from the other side may be meant for the user, so
    /// they can be answered without being asked.
    #[must_use]
    pub const fn user_takes_part(self) -> bool {
        matches!(self, Self::Unclear | Self::Conversation)
    }

    const fn prompt_description(self) -> &'static str {
        match self {
            Self::Unclear => "not yet clear",
            Self::Conversation => "a live conversation Me takes part in",
            Self::Presentation => "a presentation to an audience that includes Me",
            Self::Media => "a recording Me is watching or listening to",
        }
    }
}

/// Reads a model-written situation, treating anything unexpected as unclear
/// so one odd value never discards the rest of the insights.
fn lenient_situation<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Situation, D::Error> {
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    let text = value
        .as_ref()
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    Ok(match text.as_str() {
        "conversation" => Situation::Conversation,
        "presentation" => Situation::Presentation,
        "media" => Situation::Media,
        _ => Situation::Unclear,
    })
}

/// Shortcut labels as actually registered (fallbacks differ from defaults).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopilotShortcuts {
    pub assist: Vec<String>,
    pub toggle: Vec<String>,
    pub ask: Vec<String>,
}

impl Default for CopilotShortcuts {
    fn default() -> Self {
        Self {
            assist: vec!["Ctrl".into(), "\u{21b5}".into()],
            toggle: vec!["Ctrl".into(), "\\".into()],
            ask: vec!["Ctrl".into(), "Shift".into(), "\u{21b5}".into()],
        }
    }
}

/// Engine -> UI updates.
#[derive(Clone, Debug, PartialEq)]
pub enum CopilotEvent {
    Listen(ListenState),
    Levels {
        me: f32,
        them: f32,
    },
    Speaking {
        speaker: Speaker,
        active: bool,
    },
    Transcript(TranscriptLine),
    AnswerStarted {
        id: u64,
        title: String,
        used_screen: bool,
        model: String,
        automatic: bool,
    },
    AnswerStatus {
        id: u64,
        status: AnswerStatus,
    },
    AnswerDelta {
        id: u64,
        text: String,
    },
    /// Text from the fast model's instant draft of the same answer.
    InstantDelta {
        id: u64,
        text: String,
    },
    InstantFinished {
        id: u64,
    },
    AnswerSource {
        id: u64,
        source: AnswerSource,
    },
    Insights(CopilotInsights),
    Settings(Box<CopilotSettings>),
    Models(Vec<ModelChoice>),
    Shortcuts(CopilotShortcuts),
    Notice(String),
    /// Toggle requested by the global shortcut or a second launch.
    ToggleVisibility,
    Show,
    /// Focus the ask box (global shortcut).
    FocusAsk,
    Ending,
    NotesSaved(String),
    Exit,
}

/// UI -> engine requests.
#[derive(Clone, Debug, PartialEq)]
pub enum CopilotAction {
    ToggleListening,
    Assist,
    Quick(QuickAction),
    Ask(String),
    AnswerQuestion(String),
    Cancel,
    ClearAnswers,
    UpdateSettings(Box<CopilotSettings>),
    OpenNotesFolder,
    OpenUrl(String),
    CopyText(String),
    End,
}

/// Formats a session offset as `m:ss` or `h:mm:ss`.
#[must_use]
pub fn format_offset(at_ms: u64) -> String {
    let seconds = at_ms / 1000;
    let (hours, minutes, seconds) = (seconds / 3600, (seconds / 60) % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Renders the most recent transcript within `max_chars`, oldest first.
#[must_use]
pub fn render_transcript(lines: &[TranscriptLine], max_chars: usize) -> String {
    let mut rendered = Vec::new();
    let mut used = 0;
    for line in lines.iter().rev() {
        let entry = format!(
            "[{}] {}: {}",
            format_offset(line.at_ms),
            line.speaker.label(),
            line.text.trim()
        );
        if used + entry.len() + 1 > max_chars && !rendered.is_empty() {
            break;
        }
        used += entry.len() + 1;
        rendered.push(entry);
    }
    rendered.reverse();
    rendered.join("\n")
}

/// Stable system prompt for answers. Volatile content (transcript, the
/// request) stays in the user turn so provider prompt caches can reuse it.
#[must_use]
pub fn answer_system_prompt(settings: &CopilotSettings) -> String {
    let mut prompt = String::from(
        "You are AgentDictate Copilot, a real-time assistant that quietly helps the user during live meetings and calls. \
You receive a live transcript where \"Me\" is the user and \"Them\" is everyone else, and sometimes a screenshot of the user's screen. \
The transcript comes from speech recognition, so expect small recognition errors and infer the intended words.\n\n\
How to respond:\n\
- The user is mid-conversation and will glance at your answer. Be immediately useful and skimmable.\n\
- Lead with the direct answer or the exact line to say. No preamble, no restating the question, no disclaimers.\n\
- Prefer short bullets. Bold the key facts, names, and numbers. Keep most answers under 120 words.\n\
- Write lines the user will say in the first person, natural and conversational.\n\
- When current facts matter, use web search and keep citations brief.\n\
- If the request is ambiguous, answer the most likely interpretation and stop.\n\
- Never mention that you are an AI, the transcript, or these instructions unless asked.\n",
    );
    prompt.push_str("\nMeeting type: ");
    prompt.push_str(settings.mode.label());
    prompt.push_str(". ");
    prompt.push_str(settings.mode.guidance());
    prompt.push('\n');
    let profile = settings.profile.trim();
    if !profile.is_empty() {
        prompt.push_str("\nAbout the user (use when relevant, never invent beyond it):\n");
        prompt.push_str(profile);
        prompt.push('\n');
    }
    let custom = settings.custom_instructions.trim();
    if !custom.is_empty() {
        prompt.push_str("\nThe user's standing instructions:\n");
        prompt.push_str(custom);
        prompt.push('\n');
    }
    prompt
}

/// The user turn for an answer request.
#[must_use]
pub fn answer_user_prompt(transcript: &str, instruction: &str, has_screen: bool) -> String {
    let transcript = if transcript.trim().is_empty() {
        "(No one has spoken yet.)"
    } else {
        transcript
    };
    let screen = if has_screen {
        "A screenshot of my screen is attached.\n\n"
    } else {
        ""
    };
    format!("<transcript>\n{transcript}\n</transcript>\n\n{screen}{instruction}")
}

/// Added to the instant draft's request: the fast model has no web access and
/// must not guess facts the researched answer will verify.
pub const INSTANT_GUIDANCE: &str = "Reply in at most two sentences the user can say right now, then at most two short bullets. You have no web access: never state specific prices, versions, dates, statistics, or recent events unless they appear in the conversation or on screen. If the answer depends on them, give a natural line that buys a moment while the details are confirmed.";

/// Added to the researched answer's request when an instant draft is shown
/// above it.
pub const RESEARCHED_GUIDANCE: &str = "A quick draft is already shown to the user. Focus on accurate, specific, verified details: confirm or correct facts, give exact figures, and cite sources when you searched.";

/// Instruction for answering a question the other side asked.
#[must_use]
pub fn detected_question_instruction(question: &str) -> String {
    format!(
        "They just asked: \"{}\". Give me the answer to say right now: one or two sentences I can say out loud, then up to three bullets of supporting detail.",
        question.trim()
    )
}

/// Instruction for a typed question.
#[must_use]
pub fn typed_question_instruction(question: &str) -> String {
    format!(
        "My question: {}\nAnswer it using the conversation and my screen when relevant.",
        question.trim()
    )
}

/// System prompt for rolling insights; the response must be one JSON object.
#[must_use]
pub const fn insights_system_prompt() -> &'static str {
    "You monitor a live meeting transcript for the user (\"Me\"). Other participants are \"Them\". \
Respond with ONLY one JSON object and no other text, using exactly these keys:\n\
{\"topic\": string of at most 8 words naming what is being discussed now,\n\
 \"summary\": array of at most 3 very short bullets about the latest discussion,\n\
 \"question\": string or null - the most recent genuine question Them asked Me and paused for Me to answer, which Me has not answered yet, rewritten to be self-contained; \
null for advice, opinions, instructions, statements, rhetorical questions, questions Them answered themselves, small talk, or when unsure,\n\
 \"suggestions\": array of at most 3 short clickable prompts (under 6 words) the user may want next, like \"Define <term>\" or \"Suggest a reply\",\n\
 \"situation\": \"conversation\" if Me takes part in a live conversation, \"presentation\" if someone presents or lectures to an audience that includes Me, \
\"media\" if Me is watching or listening to a recording such as a video or podcast (Them talk to each other or to a camera while Me stays silent), \
or \"unclear\"}"
}

/// User turn for rolling insights.
#[must_use]
pub fn insights_user_prompt(transcript: &str) -> String {
    format!("<transcript>\n{transcript}\n</transcript>")
}

/// Extracts insights JSON from a model response that may wrap it in prose
/// or a code fence.
#[must_use]
pub fn parse_insights(response: &str) -> Option<CopilotInsights> {
    let start = response.find('{')?;
    let end = response.rfind('}')?;
    if end <= start {
        return None;
    }
    let mut insights: CopilotInsights = serde_json::from_str(&response[start..=end]).ok()?;
    insights.topic = insights.topic.trim().to_owned();
    insights.summary = insights
        .summary
        .into_iter()
        .map(|item| item.trim().to_owned())
        .filter(|item| !item.is_empty())
        .take(3)
        .collect();
    insights.suggestions = insights
        .suggestions
        .into_iter()
        .map(|item| item.trim().to_owned())
        .filter(|item| !item.is_empty())
        .take(3)
        .collect();
    insights.question = insights
        .question
        .map(|question| question.trim().to_owned())
        .filter(|question| !question.is_empty() && !question.eq_ignore_ascii_case("null"));
    Some(insights)
}

const QUESTION_OPENERS: [&str; 24] = [
    "what", "why", "how", "when", "where", "who", "which", "can", "could", "would", "will",
    "should", "do", "does", "did", "is", "are", "was", "were", "have", "has", "tell", "explain",
    "walk",
];

const SMALL_TALK: [&str; 8] = [
    "how are you",
    "how's it going",
    "hows it going",
    "can you hear me",
    "can you see my screen",
    "you there",
    "how are things",
    "how have you been",
];

fn spoken_words(normalized: &str) -> Vec<&str> {
    normalized
        .split(|character: char| !character.is_alphanumeric() && character != '\'')
        .filter(|word| !word.is_empty())
        .collect()
}

/// True when a line from the other side says enough to judge whether the
/// user should respond: not a fragment, a greeting, or a connection check.
#[must_use]
pub fn worth_judging(text: &str) -> bool {
    let normalized = text.trim().to_lowercase();
    let words = spoken_words(&normalized);
    words.len() >= 4
        && !(SMALL_TALK.iter().any(|phrase| normalized.contains(phrase)) && words.len() < 9)
}

/// True when a line from the other side is very likely a substantive
/// question, worth drafting an answer to before the fast model confirms it.
#[must_use]
pub fn looks_like_question(text: &str) -> bool {
    if !worth_judging(text) {
        return false;
    }
    let normalized = text.trim().to_lowercase();
    let words = spoken_words(&normalized);
    let ends_with_question = normalized.trim_end().ends_with('?');
    let opener = words
        .first()
        .is_some_and(|first| QUESTION_OPENERS.contains(first));
    ends_with_question
        || (opener
            && words
                .first()
                .is_some_and(|first| matches!(*first, "tell" | "explain" | "walk")))
}

fn word_set(text: &str) -> std::collections::BTreeSet<String> {
    text.to_lowercase()
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| word.len() > 1)
        .map(str::to_owned)
        .collect()
}

/// Jaccard similarity of two utterances' word sets, used to drop microphone
/// lines that are speaker echo of the other side.
#[must_use]
pub fn utterance_similarity(left: &str, right: &str) -> f32 {
    let left = word_set(left);
    let right = word_set(right);
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let shared = left.intersection(&right).count();
    let union = left.union(&right).count();
    shared as f32 / union as f32
}

/// Microphone lines this similar to a concurrent system-audio line are echo.
pub const ECHO_SIMILARITY: f32 = 0.55;

/// Microphone lines whose words mostly appear in what the other side just
/// said are echo fragments, however short.
pub const ECHO_CONTAINMENT: f32 = 0.6;

/// Share of a microphone line's words that also appear in `theirs`.
///
/// Speaker echo usually reaches the transcript as a fragment of a longer
/// system-audio line, which word-set similarity scores low because the
/// fragment is a small part of the union.
#[must_use]
pub fn echo_containment(mine: &str, theirs: &str) -> f32 {
    let mine = word_set(mine);
    if mine.is_empty() {
        return 0.0;
    }
    let theirs = word_set(theirs);
    mine.intersection(&theirs).count() as f32 / mine.len() as f32
}

/// True when a microphone line repeats what the other side said nearby.
#[must_use]
pub fn is_echo_of(mine: &str, theirs: &str) -> bool {
    echo_containment(mine, theirs) >= ECHO_CONTAINMENT
        || utterance_similarity(mine, theirs) >= ECHO_SIMILARITY
}

/// System prompt deciding whether the other side's latest line calls for the
/// user to respond now, with help.
#[must_use]
pub const fn turn_judge_system_prompt() -> &'static str {
    "You decide, during a live call, whether the user (\"Me\") should respond now to the latest line from \"Them\". \
Answer true when that line asks Me a question, asks Me to do or explain something, or raises a concern or objection \
Me needs to address, even without a question mark. Answer false for rhetorical questions, questions the speaker answers \
or moves past, questions or remarks to someone else or to an audience, small talk, acknowledgments, and statements \
that need no reply. When Me has not been speaking, Them may be talking to each other or to a camera, as in a video, \
webinar, or podcast: answer true only if the line clearly addresses Me. \
Respond with ONLY {\"answer\": true} or {\"answer\": false}."
}

/// What the turn judge knows beyond the transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TurnContext {
    pub situation: Situation,
    /// Seconds since the user last spoke, if they have.
    pub me_last_spoke_secs: Option<u64>,
}

/// User turn for the turn judge.
#[must_use]
pub fn turn_judge_user_prompt(transcript: &str, latest: &str, context: TurnContext) -> String {
    let participation = match context.me_last_spoke_secs {
        None => "Me has not spoken yet.".to_owned(),
        Some(seconds) => format!("Me last spoke {seconds} seconds ago."),
    };
    format!(
        "<transcript>\n{transcript}\n</transcript>\n\nSituation: {}. {participation}\n\
Latest line from Them: \"{}\"",
        context.situation.prompt_description(),
        latest.trim()
    )
}

/// Reads the turn judge's verdict, tolerating prose around the JSON.
#[must_use]
pub fn parse_turn_verdict(response: &str) -> Option<bool> {
    let start = response.find('{')?;
    let end = response.rfind('}')?;
    let value: serde_json::Value = serde_json::from_str(response.get(start..=end)?).ok()?;
    value.get("answer").and_then(serde_json::Value::as_bool)
}

/// Builds the Markdown meeting notes file.
#[must_use]
pub fn meeting_notes_markdown(
    title: &str,
    started: &str,
    duration_ms: u64,
    summary: Option<&str>,
    transcript: &[TranscriptLine],
    answers: &[(String, String)],
) -> String {
    let mut notes = format!(
        "# {title}\n\n- **Started:** {started}\n- **Duration:** {}\n\n",
        format_offset(duration_ms)
    );
    if let Some(summary) = summary.map(str::trim).filter(|summary| !summary.is_empty()) {
        notes.push_str("## Summary\n\n");
        notes.push_str(summary);
        notes.push_str("\n\n");
    }
    if !answers.is_empty() {
        notes.push_str("## Copilot answers\n\n");
        for (question, answer) in answers {
            notes.push_str("### ");
            notes.push_str(question.trim());
            notes.push_str("\n\n");
            notes.push_str(answer.trim());
            notes.push_str("\n\n");
        }
    }
    notes.push_str("## Transcript\n\n");
    if transcript.is_empty() {
        notes.push_str("_No speech was transcribed._\n");
    }
    for line in transcript {
        notes.push_str(&format!(
            "**[{}] {}:** {}\n\n",
            format_offset(line.at_ms),
            line.speaker.display(),
            line.text.trim()
        ));
    }
    notes
}

/// System prompt for end-of-meeting notes.
#[must_use]
pub const fn notes_system_prompt() -> &'static str {
    "You write concise meeting notes from a transcript where \"Me\" is the user and \"Them\" is everyone else. \
Use Markdown with these sections: a one-sentence overview, **Key points** (bullets), **Decisions** (bullets, or \"None\"), \
and **Action items** (bullets with an owner when known, or \"None\"). Be factual and brief; do not invent details."
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(id: u64, speaker: Speaker, text: &str, at_ms: u64) -> TranscriptLine {
        TranscriptLine {
            id,
            speaker,
            text: text.into(),
            at_ms,
        }
    }

    #[test]
    fn settings_default_to_the_signed_in_chatgpt_route() {
        let settings = CopilotSettings::default();
        assert_eq!(settings.provider, CopilotProvider::ChatgptSubscription);
        assert_eq!(settings.answer_model(), "gpt-6-sol");
        assert!(settings.instant_answers);
        assert_eq!(settings.fast_model(), "gpt-6-luna");
        assert!(
            settings.web_search && settings.auto_answer && settings.launch_from_dictation_hotkey
        );
    }

    #[test]
    fn per_provider_models_round_trip_through_json() {
        let mut settings = CopilotSettings {
            provider: CopilotProvider::AnthropicApi,
            ..CopilotSettings::default()
        };
        settings.set_answer_model("claude-sonnet-5-5");
        let json = serde_json::to_string(&settings).unwrap();
        assert!(json.contains("\"anthropic_api\":\"claude-sonnet-5-5\""));
        let restored: CopilotSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.answer_model(), "claude-sonnet-5-5");
        assert_eq!(restored.fast_model(), "claude-haiku-5-5");
    }

    #[test]
    fn partial_settings_files_fill_defaults() {
        let restored: CopilotSettings =
            serde_json::from_str(r#"{"provider":"claude_code","auto_answer":false}"#).unwrap();
        assert_eq!(restored.provider, CopilotProvider::ClaudeCode);
        assert!(!restored.auto_answer);
        assert!(restored.web_search);
        assert_eq!(restored.answer_model(), "opus");
    }

    #[test]
    fn redaction_hides_keys() {
        let settings = CopilotSettings {
            anthropic_api_key: "sk-ant-secret".into(),
            ..CopilotSettings::default()
        };
        let redacted = settings.redacted();
        assert_eq!(redacted.anthropic_api_key, "<set>");
        assert!(redacted.openai_api_key.is_empty());
    }

    #[test]
    fn transcript_keeps_the_most_recent_lines_within_budget() {
        let lines = vec![
            line(1, Speaker::Them, "first line that is fairly long", 1_000),
            line(2, Speaker::Me, "second", 65_000),
            line(3, Speaker::Them, "third?", 3_725_000),
        ];
        let all = render_transcript(&lines, 10_000);
        assert_eq!(
            all,
            "[0:01] Them: first line that is fairly long\n[1:05] Me: second\n[1:02:05] Them: third?"
        );
        let recent = render_transcript(&lines, 45);
        assert_eq!(recent, "[1:05] Me: second\n[1:02:05] Them: third?");
    }

    #[test]
    fn system_prompt_includes_mode_profile_and_instructions() {
        let settings = CopilotSettings {
            mode: CopilotMode::Sales,
            profile: "I sell AgentDictate.".into(),
            custom_instructions: "Use metric units.".into(),
            ..CopilotSettings::default()
        };
        let prompt = answer_system_prompt(&settings);
        assert!(prompt.contains("Sales call"));
        assert!(prompt.contains("I sell AgentDictate."));
        assert!(prompt.contains("Use metric units."));
    }

    #[test]
    fn user_prompt_marks_screens_and_empty_transcripts() {
        let prompt = answer_user_prompt("", QuickAction::Recap.instruction(), true);
        assert!(prompt.contains("(No one has spoken yet.)"));
        assert!(prompt.contains("screenshot of my screen is attached"));
    }

    #[test]
    fn insights_parse_from_fenced_responses() {
        let response = "```json\n{\"topic\":\" Pricing \",\"summary\":[\"a\",\"\",\"b\",\"c\",\"d\"],\"question\":\"What is the annual price?\",\"suggestions\":[\"Suggest a reply\"]}\n```";
        let insights = parse_insights(response).unwrap();
        assert_eq!(insights.topic, "Pricing");
        assert_eq!(insights.summary, vec!["a", "b", "c"]);
        assert_eq!(
            insights.question.as_deref(),
            Some("What is the annual price?")
        );
        let none = parse_insights("{\"topic\":\"x\",\"question\":null}").unwrap();
        assert_eq!(none.question, None);
        assert!(parse_insights("no json").is_none());
    }

    #[test]
    fn question_heuristic_ignores_small_talk_and_fragments() {
        assert!(looks_like_question(
            "What does your pricing look like for teams?"
        ));
        assert!(looks_like_question(
            "Tell me about a time you led a project"
        ));
        assert!(!looks_like_question("How are you?"));
        assert!(!looks_like_question("Really?"));
        assert!(!looks_like_question("We shipped the release yesterday."));
    }

    #[test]
    fn echo_similarity_detects_repeated_speech() {
        let them = "We should ship the pricing update on Monday";
        assert!(
            utterance_similarity("we should ship the pricing update monday", them)
                > ECHO_SIMILARITY
        );
        assert!(utterance_similarity("I disagree, Friday works better", them) < ECHO_SIMILARITY);
    }

    #[test]
    fn echo_fragments_of_longer_lines_are_recognized() {
        // Real echo fragments from a laptop-speaker session.
        let them = "trip with us. It's like the entire wedding party. What's one thing in your life that you can never have enough of? And I thought I'd get some silly answers";
        assert!(is_echo_of(
            "in your life that you can never have enough of. And I thought",
            them
        ));
        let them = "of connection, community, tribe, family, belonging. And that's the industry that we're in. So our industry, what we're selling.";
        assert!(is_echo_of("Tribe. Family. Belonging.", them));
        assert!(is_echo_of(
            "And that's the industry that we're in. So our",
            them
        ));
        let them =
            "or money. And I got some of that, but mostly what people told me was some version.";
        assert!(is_echo_of("People told me with some version.", them));
        let them = "All right. I have very few rules of law, but a rule of sales is never present an invoice via email.";
        assert!(is_echo_of("Role of sales is never presented.", them));
        // The user's own words are kept.
        assert!(!is_echo_of(
            "Sorry, can you repeat the pricing for the annual plan?",
            them
        ));
        assert!(!is_echo_of("", them));
    }

    #[test]
    fn turn_verdicts_parse_and_prompts_carry_the_context() {
        assert_eq!(parse_turn_verdict("{\"answer\": true}"), Some(true));
        assert_eq!(
            parse_turn_verdict("Sure: {\"answer\": false} done"),
            Some(false)
        );
        assert_eq!(parse_turn_verdict("yes"), None);
        let silent = turn_judge_user_prompt(
            "[0:01] Them: Hi",
            "How much?",
            TurnContext {
                situation: Situation::Unclear,
                me_last_spoke_secs: None,
            },
        );
        assert!(silent.contains("Latest line from Them: \"How much?\""));
        assert!(silent.contains("Me has not spoken yet."));
        let talking = turn_judge_user_prompt(
            "",
            "Walk me through your pricing",
            TurnContext {
                situation: Situation::Conversation,
                me_last_spoke_secs: Some(12),
            },
        );
        assert!(talking.contains("a live conversation Me takes part in"));
        assert!(talking.contains("Me last spoke 12 seconds ago."));
    }

    #[test]
    fn situations_parse_leniently_and_gate_automatic_answers() {
        let media = parse_insights("{\"topic\":\"Podcast\",\"situation\":\"Media\"}").unwrap();
        assert_eq!(media.situation, Situation::Media);
        assert!(!media.situation.user_takes_part());
        // Unknown or missing values keep the rest of the insights.
        let odd = parse_insights("{\"topic\":\"Sync\",\"situation\":\"standup\"}").unwrap();
        assert_eq!(
            (odd.topic.as_str(), odd.situation),
            ("Sync", Situation::Unclear)
        );
        let missing = parse_insights("{\"topic\":\"Sync\",\"situation\":null}").unwrap();
        assert_eq!(missing.situation, Situation::Unclear);
        assert!(Situation::Unclear.user_takes_part());
        assert!(Situation::Conversation.user_takes_part());
        assert!(!Situation::Presentation.user_takes_part());
    }

    #[test]
    fn substantial_lines_are_judged_even_without_a_question_mark() {
        assert!(worth_judging(
            "I'd love to understand how you handle security."
        ));
        assert!(!worth_judging("Can you hear me?"));
        assert!(!worth_judging("Sounds good."));
    }

    #[test]
    fn notes_include_summary_answers_and_transcript() {
        let notes = meeting_notes_markdown(
            "Meeting notes",
            "2026-10-13 09:00",
            125_000,
            Some("Overview"),
            &[line(1, Speaker::Them, "Hi", 0)],
            &[("What should I say?".into(), "Say hello.".into())],
        );
        assert!(notes.contains("**Duration:** 2:05"));
        assert!(notes.contains("## Summary\n\nOverview"));
        assert!(notes.contains("### What should I say?"));
        assert!(notes.contains("**[0:00] Them:** Hi"));
    }
}
