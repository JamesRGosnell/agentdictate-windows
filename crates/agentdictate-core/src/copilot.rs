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

    /// The provider's quickest model. It always judges turns and writes
    /// insights, whatever model the user picks for drafts, because those must
    /// keep pace with the call.
    #[must_use]
    pub const fn default_fast_model(self) -> &'static str {
        match self {
            Self::ChatgptSubscription => "gpt-6-luna",
            Self::AnthropicApi => "claude-haiku-5-5",
            Self::ClaudeCode => "haiku",
            Self::OpenaiApi => "gpt-5.4-mini",
        }
    }

    /// Default model for instant drafts: the most knowledgeable model that
    /// still answers in about a second. Measured on ChatGPT, GPT-6 Sol starts
    /// as fast as GPT-6 Luna (about 1.2 s without web search) and knows more.
    #[must_use]
    pub const fn default_instant_model(self) -> &'static str {
        match self {
            Self::ChatgptSubscription => "gpt-6-sol",
            _ => self.default_fast_model(),
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

/// How quickly a model starts answering in the copilot, measured as the
/// first words of an instant draft (no web search).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelPace {
    /// Under a second; smaller models with less world knowledge.
    Fastest,
    /// About a second; larger models, the best instant drafts.
    Fast,
    /// Several seconds.
    Slower,
}

impl ModelPace {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fastest => "Fastest",
            Self::Fast => "Fast",
            Self::Slower => "Slower",
        }
    }
}

/// The pace of a known model; `None` for models not yet measured.
#[must_use]
pub fn model_pace(id: &str) -> Option<ModelPace> {
    let id = id.trim().to_ascii_lowercase();
    match id.as_str() {
        "gpt-6-luna" | "gpt-5.6-luna" | "gpt-5.4-mini" | "haiku" | "claude-haiku-5-5" => {
            Some(ModelPace::Fastest)
        }
        "gpt-6-sol" | "gpt-5.6-sol" | "gpt-5.6-terra" | "sonnet" | "claude-sonnet-5-5" => {
            Some(ModelPace::Fast)
        }
        "gpt-6.1-sol" | "gpt-6-astra" | "opus" | "claude-opus-5-5" => Some(ModelPace::Slower),
        _ => None,
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

/// Ways to rewrite the line to say with one click.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewriteStyle {
    Shorter,
    Casual,
    Specific,
    Another,
}

impl RewriteStyle {
    pub const ALL: [Self; 4] = [Self::Shorter, Self::Casual, Self::Specific, Self::Another];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Shorter => "Shorter",
            Self::Casual => "More casual",
            Self::Specific => "More specific",
            Self::Another => "Another way",
        }
    }

    const fn guidance(self) -> &'static str {
        match self {
            Self::Shorter => {
                "Make it shorter: one sentence of at most 20 words that keeps the key fact."
            }
            Self::Casual => {
                "Make it more casual and conversational, as I would say it to a colleague, with the same facts."
            }
            Self::Specific => {
                "Make it more specific with one concrete detail, such as a number, name, date, or example, \
taken from my knowledge excerpts or the conversation. Never invent one; if none is available, make the wording more precise."
            }
            Self::Another => {
                "Say it another way: the same meaning in different words and structure."
            }
        }
    }
}

/// Instruction for rewriting a line the user is about to say. `details` is
/// the full answer the line came from, including what research added.
#[must_use]
pub fn rewrite_instruction(style: RewriteStyle, line: &str, details: &str) -> String {
    let details = details.trim();
    let details = if details.is_empty() || details == line.trim() {
        String::new()
    } else {
        format!(
            "The full answer it came from, with checked details you may use:\n<answer>\n{details}\n</answer>\n"
        )
    };
    format!(
        "I'm about to say this:\n<line>\n{}\n</line>\n{details}{} Reply with only the new line to say, in the first person, \
with no preamble, quotes, bullets, or mention of my documents.",
        line.trim(),
        style.guidance()
    )
}

/// Persisted copilot preferences, stored beside the dictation settings in
/// `copilot.json` so the meeting assistant evolves independently.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CopilotSettings {
    pub provider: CopilotProvider,
    /// Answer model per provider; missing entries use the provider default.
    pub answer_models: BTreeMap<CopilotProvider, String>,
    /// Fast model per provider for instant drafts and meeting notes.
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
    /// Answers may draw on notes from earlier meetings.
    pub use_past_meetings: bool,
    /// The user has dismissed the first-run welcome.
    pub onboarded: bool,
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
            use_past_meetings: true,
            onboarded: false,
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
            .unwrap_or(self.provider.default_instant_model())
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
    /// Which of the other side's voices spoke, when they are told apart.
    #[serde(default)]
    pub voice: Option<u8>,
}

/// Names of the other side's people, by voice.
pub type VoiceNames = BTreeMap<u8, String>;

/// How transcript lines name who spoke: "You" and "Them", "Them A" and
/// "Them B" once the other side's voices are told apart, and people's names
/// once they are known.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SpeakerLabels {
    several_voices: bool,
    /// The other side's voice, when exactly one was told apart.
    only_voice: Option<u8>,
    names: VoiceNames,
}

impl SpeakerLabels {
    #[must_use]
    pub fn new(lines: &[TranscriptLine], names: &VoiceNames) -> Self {
        let mut voices = lines
            .iter()
            .filter(|line| line.speaker == Speaker::Them)
            .filter_map(|line| line.voice);
        let only_voice = voices.next();
        let several_voices = only_voice.is_some_and(|first| voices.any(|voice| voice != first));
        Self {
            several_voices,
            only_voice: only_voice.filter(|_| !several_voices),
            names: names.clone(),
        }
    }

    /// Whether lines tell the other side's voices apart.
    #[must_use]
    pub const fn several_voices(&self) -> bool {
        self.several_voices
    }

    /// The voice a line is named by: its own, or the only voice when the
    /// other side is one person. `None` for the user's lines and for
    /// unassigned lines among several voices.
    #[must_use]
    pub fn voice_of(&self, line: &TranscriptLine) -> Option<u8> {
        match (line.speaker, line.voice) {
            (Speaker::Me, _) => None,
            (Speaker::Them, _) if !self.several_voices => Some(self.only_voice.unwrap_or(0)),
            (Speaker::Them, voice) => voice,
        }
    }

    /// The other side's voices, in the order they first spoke.
    #[must_use]
    pub fn voices(&self, lines: &[TranscriptLine]) -> Vec<u8> {
        let mut voices = Vec::new();
        for voice in lines.iter().filter_map(|line| self.voice_of(line)) {
            if !voices.contains(&voice) {
                voices.push(voice);
            }
        }
        voices
    }

    /// The person who spoke a line, when known.
    #[must_use]
    pub fn name(&self, line: &TranscriptLine) -> Option<&str> {
        self.voice_of(line).and_then(|voice| self.voice_name(voice))
    }

    /// A voice's person, when known.
    #[must_use]
    pub fn voice_name(&self, voice: u8) -> Option<&str> {
        self.names.get(&voice).map(String::as_str)
    }

    /// The speaker for prompts: "Me", "Them", "Them A", or with the
    /// person's name, "Them (Sam)" and "Them A (Sam)".
    #[must_use]
    pub fn label(&self, line: &TranscriptLine) -> String {
        match line.speaker {
            Speaker::Me => Speaker::Me.label().to_owned(),
            Speaker::Them => {
                let base = self.base(line);
                match self.name(line) {
                    Some(name) => format!("{base} ({name})"),
                    None => base,
                }
            }
        }
    }

    /// The speaker for people: "You", "Them", "Them A", or the person's name.
    #[must_use]
    pub fn display(&self, line: &TranscriptLine) -> String {
        match (line.speaker, self.name(line)) {
            (Speaker::Me, _) => Speaker::Me.display().to_owned(),
            (Speaker::Them, Some(name)) => name.to_owned(),
            (Speaker::Them, None) => self.base(line),
        }
    }

    /// How a voice is labeled before it has a name: "Them", or "Them A"
    /// among several voices.
    #[must_use]
    pub fn voice_label(&self, voice: u8) -> String {
        if self.several_voices {
            format!("Them {}", voice_letter(voice))
        } else {
            Speaker::Them.label().to_owned()
        }
    }

    /// The voice a model-written label refers to, such as "Them B" or
    /// "Them B (Sam)".
    #[must_use]
    pub fn voice_for_label(&self, label: &str) -> Option<u8> {
        let label = label.split('(').next()?.trim();
        let rest = label
            .get(..4)
            .filter(|head| head.eq_ignore_ascii_case("them"))
            .and_then(|_| label.get(4..))?
            .trim();
        let mut letters = rest.chars();
        match (letters.next(), letters.next()) {
            (None, _) if !self.several_voices => Some(self.only_voice.unwrap_or(0)),
            (Some(letter), None) if letter.is_ascii_alphabetic() => {
                let voice = letter.to_ascii_uppercase() as u8 - b'A';
                if self.several_voices {
                    Some(voice)
                } else {
                    // A lone voice is plain "Them"; "Them A" can only mean it.
                    (voice == 0).then(|| self.only_voice.unwrap_or(0))
                }
            }
            _ => None,
        }
    }

    fn base(&self, line: &TranscriptLine) -> String {
        match line.voice {
            Some(voice) if self.several_voices => self.voice_label(voice),
            _ => Speaker::Them.label().to_owned(),
        }
    }
}

/// A person's given name, for tight labels.
#[must_use]
pub fn given_name(name: &str) -> &str {
    name.split_whitespace().next().unwrap_or(name)
}

/// Words a model may write where it does not know a name.
const NOT_NAMES: [&str; 26] = [
    "me",
    "you",
    "them",
    "us",
    "unknown",
    "null",
    "none",
    "n/a",
    "user",
    "host",
    "co-host",
    "guest",
    "speaker",
    "interviewer",
    "interviewee",
    "candidate",
    "presenter",
    "narrator",
    "moderator",
    "everyone",
    "participant",
    "customer",
    "client",
    "caller",
    "other",
    "someone",
];

/// A person's name written by a model, trimmed of decoration, or `None`
/// when it is not plausibly a name: placeholders, roles, or long phrases.
#[must_use]
pub fn clean_person_name(raw: &str) -> Option<String> {
    // Meeting apps decorate names: "Sam Lee (Guest)", "Sam Lee | Acme".
    let raw = raw.split(['(', '|', '[']).next().unwrap_or_default();
    let name = raw
        .trim_matches(|character: char| {
            character.is_whitespace() || matches!(character, '"' | '\'' | '*' | '.' | ',' | ':')
        })
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let first = name.chars().next()?;
    let plausible = first.is_alphabetic()
        && !first.is_lowercase()
        && name.chars().count() <= 40
        && name.split(' ').count() <= 4
        && !name.chars().any(|character| {
            character.is_ascii_digit()
                || matches!(
                    character,
                    '@' | '/' | '\\' | '<' | '>' | '{' | '}' | ']' | ')'
                )
        });
    let lower = name.to_lowercase();
    let placeholder = NOT_NAMES.contains(&lower.as_str())
        || lower.starts_with("them ")
        || lower.starts_with("speaker ");
    (plausible && !placeholder).then_some(name)
}

/// The letter naming one of the other side's voices: A, B, C...
#[must_use]
pub fn voice_letter(voice: u8) -> char {
    char::from(b'A' + voice.min(25))
}

/// Whether more than one of the other side's voices spoke, so lines name
/// them; with a single voice, "Them" stays plain.
#[must_use]
pub fn has_several_voices(lines: &[TranscriptLine]) -> bool {
    let mut first = None;
    lines
        .iter()
        .filter(|line| line.speaker == Speaker::Them)
        .filter_map(|line| line.voice)
        .any(|voice| *first.get_or_insert(voice) != voice)
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

/// What the copilot can draw on beyond the conversation: the user's own
/// documents and notes from earlier meetings.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnowledgeSummary {
    pub documents: usize,
    pub meetings: usize,
    pub words: usize,
    /// Files in the knowledge folder that cannot be read, such as PDFs.
    pub unreadable: Vec<String>,
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
    /// People's names the conversation makes certain, by transcript label
    /// such as "Them B".
    #[serde(deserialize_with = "lenient_speakers")]
    pub speakers: BTreeMap<String, String>,
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

/// Reads model-written speaker names, skipping entries that are not text.
fn lenient_speakers<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, String>, D::Error> {
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    let Some(serde_json::Value::Object(entries)) = value else {
        return Ok(BTreeMap::new());
    };
    Ok(entries
        .into_iter()
        .filter_map(|(label, name)| Some((label.trim().to_owned(), name.as_str()?.to_owned())))
        .collect())
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
    /// Text of a rewrite of an answer's line to say.
    RewriteDelta {
        id: u64,
        text: String,
    },
    RewriteFinished {
        id: u64,
        error: Option<String>,
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
    Knowledge(KnowledgeSummary),
    /// People's names by voice, and the names seen in the meeting window,
    /// offered when the user names a voice.
    Speakers {
        names: VoiceNames,
        seen: Vec<String>,
    },
    Ending,
    NotesSaved {
        path: String,
        /// Overview, key points, decisions, and action items, in Markdown.
        summary: Option<String>,
        follow_up: Option<String>,
    },
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
    /// Opens the notes file written when the session ended.
    OpenNotes,
    OpenKnowledgeFolder,
    OpenUrl(String),
    CopyText(String),
    /// Rewrites an answer's line to say, `text`, in a style.
    Rewrite {
        id: u64,
        style: RewriteStyle,
        text: String,
    },
    /// The version of an answer's line the user settled on; `None` for the
    /// original.
    KeepVersion {
        id: u64,
        text: Option<String>,
    },
    /// Names one of the other side's voices; an empty name forgets it.
    NameSpeaker {
        voice: u8,
        name: String,
    },
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
pub fn render_transcript(lines: &[TranscriptLine], names: &VoiceNames, max_chars: usize) -> String {
    let labels = SpeakerLabels::new(lines, names);
    let mut rendered = Vec::new();
    let mut used = 0;
    for line in lines.iter().rev() {
        let entry = format!(
            "[{}] {}: {}",
            format_offset(line.at_ms),
            labels.label(line),
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
When the other side has several people, their lines are labeled Them A, Them B, and so on, told apart by voice; the labels can be imperfect. \
A person's name follows the label once known, as in \"Them A (Sam)\"; use names where it is natural. \
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
///
/// `knowledge` holds excerpts from the user's documents and earlier meetings,
/// which take precedence over general knowledge about the user's business.
pub fn answer_user_prompt(
    transcript: &str,
    instruction: &str,
    has_screen: bool,
    knowledge: &str,
) -> String {
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
    let knowledge = knowledge.trim();
    let knowledge = if knowledge.is_empty() {
        String::new()
    } else {
        format!(
            "<my_knowledge>\n{knowledge}\n</my_knowledge>\n\
These are excerpts from my own documents and from notes of my earlier meetings. When they cover what is asked, \
answer from them: they override web results and general knowledge about me and my business. Mention the document \
briefly when you rely on it. Never invent facts about me or my business that are not in them or my profile.\n\n"
        )
    };
    format!("{knowledge}<transcript>\n{transcript}\n</transcript>\n\n{screen}{instruction}")
}

/// Added to the instant draft's request: the fast model has no web access and
/// must not guess facts the researched answer will verify.
pub const INSTANT_GUIDANCE: &str = "Reply in at most two sentences the user can say right now, then at most two short bullets. You have no web access: answer from what you reliably know and from my knowledge excerpts. For anything that changes or that you are unsure of, such as current prices, recent events, the latest versions, or exact figures, never guess: give a natural line that buys a moment while the details are confirmed.";

/// What the researched answer says when the instant draft needs nothing more.
pub const NOTHING_TO_ADD: &str = "Nothing to add.";

/// Added to the researched answer's request once the instant draft is on
/// screen: the researched answer continues the draft instead of repeating it,
/// so the user reads one answer that grows, and corrects it only when wrong.
#[must_use]
pub fn researched_guidance(draft: &str) -> String {
    format!(
        "I'm already saying this quick draft to them:\n<draft>\n{}\n</draft>\n\
Continue from it without repeating it. Add only what makes it stronger and that I can say next: verified specifics \
such as exact figures, names, and dates, as one or two sentences or a few short bullets, citing sources briefly when you \
searched. If the draft is wrong or misleading, begin with \"Correction:\" and give the line I should say instead. \
If the draft is already complete and accurate, reply with exactly \"{NOTHING_TO_ADD}\"",
        draft.trim()
    )
}

/// How the researched answer relates to the instant draft above it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FollowUp {
    /// Nothing usable yet, possibly the start of a marker still streaming.
    Pending,
    /// Details that continue the draft.
    Adds(String),
    /// The draft was wrong; this is what to say instead.
    Correction(String),
    /// The draft was checked and needs nothing more.
    Confirmed,
}

/// Reads a researched answer written with [`researched_guidance`], holding
/// back text that may still become a marker while it streams.
#[must_use]
pub fn researched_follow_up(text: &str) -> FollowUp {
    let unmarked = text.trim_start().trim_start_matches(['*', '_']);
    let lower = unmarked.to_lowercase();
    if lower.is_empty() {
        return FollowUp::Pending;
    }
    let nothing = "nothing to add";
    if lower.starts_with(nothing) {
        return FollowUp::Confirmed;
    }
    let correction = "correction:";
    if let Some(rest) = lower.strip_prefix(correction) {
        let offset = unmarked.len() - rest.len();
        let rest = unmarked
            .get(offset..)
            .unwrap_or_default()
            .trim_start_matches(['*', '_', ' ']);
        return FollowUp::Correction(rest.to_owned());
    }
    if nothing.starts_with(lower.trim_end()) || correction.starts_with(lower.trim_end()) {
        return FollowUp::Pending;
    }
    FollowUp::Adds(text.trim().to_owned())
}

/// The answer as one text, for notes and copying: the draft with what the
/// researched answer added, or the correction that replaced it.
#[must_use]
pub fn combined_answer(instant: &str, researched: &str) -> String {
    let instant = instant.trim();
    if instant.is_empty() {
        return researched.trim().to_owned();
    }
    match researched_follow_up(researched) {
        FollowUp::Pending | FollowUp::Confirmed => instant.to_owned(),
        FollowUp::Adds(more) => format!("{instant}\n\n{more}"),
        FollowUp::Correction(line) if line.is_empty() => instant.to_owned(),
        FollowUp::Correction(line) => format!("Correction: {line}"),
    }
}

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
or \"unclear\",\n\
 \"speakers\": object mapping the other side's transcript labels, such as \"Them\" or \"Them B\", to that person's name, only when the transcript makes it certain: \
they introduce themselves (\"Hi, I'm Sam\"), they answer right after being addressed by name (\"Sam, what do you think?\"), or Me thanks or addresses them by name right after they spoke. \
Use {} when unsure, and never guess a name from a role or company. Labels with a name in parentheses are already named; include one only to correct a name the transcript clearly contradicts.}"
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
    insights.speakers = insights
        .speakers
        .into_iter()
        .filter_map(|(label, name)| Some((label, clean_person_name(&name)?)))
        .take(6)
        .collect();
    Some(insights)
}

/// What a screenshot of the meeting app shows about who is on the call.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct CallView {
    /// A call or meeting window with participants is visible.
    pub call: bool,
    /// The participant the app shows as speaking.
    pub speaking: Option<String>,
    /// The other participants' names shown, without the user.
    pub participants: Vec<String>,
    /// The user's own name, when shown.
    pub me: Option<String>,
}

/// System prompt for reading who is speaking from a meeting app's window.
#[must_use]
pub const fn call_view_system_prompt() -> &'static str {
    "You read screenshots of video calls (Teams, Zoom, Google Meet, Webex, Slack, and others) for the user, who is in the call. \
Respond with ONLY one JSON object and no other text, using exactly these keys:\n\
{\"call\": true if a call or meeting window with participants is visible, otherwise false,\n\
 \"speaking\": the name of the participant the app shows as speaking right now (a highlighted tile border, a speaking indicator or sound bars by the name, \
or the active speaker shown large), exactly as written on screen; null when no one or several people are shown speaking, or when unsure,\n\
 \"participants\": array of the other participants' names shown on screen, exactly as written, without the user,\n\
 \"me\": the user's own name when the screen shows it (their self view, or a name marked \"(You)\" or \"Me\"), otherwise null}\n\
Report only names written on screen; never guess one from a face. Ignore names in chat messages, documents, and shared slides."
}

/// User turn for reading a meeting app's window.
#[must_use]
pub fn call_view_user_prompt(window_title: Option<&str>, profile: &str) -> String {
    let mut prompt = String::new();
    if let Some(title) = window_title
        .map(str::trim)
        .filter(|title| !title.is_empty())
    {
        prompt.push_str(&format!("Window title: {title}\n"));
    }
    let profile: String = profile.trim().chars().take(400).collect();
    if !profile.is_empty() {
        prompt.push_str(&format!("About the user: {profile}\n"));
    }
    prompt.push_str("Someone on the other side is talking right now. Who is shown speaking?");
    prompt
}

/// Reads a meeting-window description, keeping only plausible names and
/// leaving the user out of the other participants.
#[must_use]
pub fn parse_call_view(response: &str) -> Option<CallView> {
    let start = response.find('{')?;
    let end = response.rfind('}')?;
    let mut view: CallView = serde_json::from_str(response.get(start..=end)?).ok()?;
    view.me = view.me.as_deref().and_then(clean_person_name);
    let is_me = |name: &str| {
        view.me
            .as_deref()
            .is_some_and(|me| me.eq_ignore_ascii_case(name))
            || name.eq_ignore_ascii_case("you")
    };
    view.speaking = view
        .speaking
        .as_deref()
        .and_then(clean_person_name)
        .filter(|name| !is_me(name));
    let mut participants: Vec<String> = Vec::new();
    for name in view
        .participants
        .iter()
        .filter_map(|name| clean_person_name(name))
    {
        if !is_me(&name)
            && !participants
                .iter()
                .any(|known| known.eq_ignore_ascii_case(&name))
        {
            participants.push(name);
        }
    }
    participants.truncate(16);
    view.participants = participants;
    Some(view)
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
    names: &VoiceNames,
    answers: &[(String, String)],
) -> String {
    let labels = SpeakerLabels::new(transcript, names);
    let mut notes = format!(
        "# {title}\n\n- **Started:** {started}\n- **Duration:** {}\n",
        format_offset(duration_ms)
    );
    let mut people: Vec<&str> = Vec::new();
    for voice in labels.voices(transcript) {
        if let Some(name) = labels
            .voice_name(voice)
            .filter(|name| !people.contains(name))
        {
            people.push(name);
        }
    }
    if !people.is_empty() {
        notes.push_str(&format!("- **With:** {}\n", people.join(", ")));
    }
    notes.push('\n');
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
            labels.display(line),
            line.text.trim()
        ));
    }
    notes
}

/// A saved meeting-notes file read back into its parts, as written by
/// [`meeting_notes_markdown`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MeetingNotes {
    pub title: String,
    pub started: String,
    pub duration: String,
    /// The other side's people, when they were named.
    pub with: String,
    /// Overview, key points, decisions, and action items, in Markdown.
    pub summary: String,
    pub follow_up: Option<String>,
    /// The copilot's answers during the meeting, in Markdown.
    pub answers: String,
    /// The transcript, in Markdown.
    pub transcript: String,
}

impl MeetingNotes {
    /// The summary's opening sentence, for lists of meetings.
    #[must_use]
    pub fn overview(&self) -> String {
        let plain = |line: &str| line.replace("**", "").trim().to_owned();
        let lines = self
            .summary
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty());
        let mut bullets = None;
        for line in lines {
            if line.starts_with('#') || (line.starts_with("**") && line.ends_with("**")) {
                continue;
            }
            if let Some(item) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
                bullets.get_or_insert_with(|| plain(item));
                continue;
            }
            return plain(line);
        }
        bullets.unwrap_or_default()
    }
}

/// Reads a meeting-notes file. Sections are found by the headings the
/// writer uses, so headings inside the model-written summary stay in it.
#[must_use]
pub fn parse_meeting_notes(markdown: &str) -> MeetingNotes {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut notes = MeetingNotes::default();
    for line in &lines {
        let line = line.trim();
        if notes.title.is_empty()
            && let Some(title) = line.strip_prefix("# ")
        {
            notes.title = title.trim().to_owned();
        } else if let Some(started) = line.strip_prefix("- **Started:**") {
            notes.started = started.trim().to_owned();
        } else if let Some(duration) = line.strip_prefix("- **Duration:**") {
            notes.duration = duration.trim().to_owned();
        } else if let Some(with) = line.strip_prefix("- **With:**") {
            notes.with = with.trim().to_owned();
        }
    }
    let heading = |name: &str| lines.iter().position(|line| line.trim() == name);
    let summary = heading("## Summary");
    let answers = heading("## Copilot answers");
    let transcript = heading("## Transcript");
    let section = |start: Option<usize>, ends: &[Option<usize>]| -> String {
        let Some(start) = start else {
            return String::new();
        };
        let end = ends
            .iter()
            .flatten()
            .copied()
            .filter(|&end| end > start)
            .min()
            .unwrap_or(lines.len());
        lines[start + 1..end].join("\n").trim().to_owned()
    };
    let (summary, follow_up) = split_follow_up(&section(summary, &[answers, transcript]));
    notes.summary = summary;
    notes.follow_up = follow_up;
    notes.answers = section(answers, &[transcript]);
    notes.transcript = section(transcript, &[]);
    notes
}

/// System prompt for end-of-meeting notes.
#[must_use]
pub const fn notes_system_prompt() -> &'static str {
    "You write concise meeting notes from a transcript where \"Me\" is the user and \"Them\" is everyone else. \
Use Markdown with these sections: a one-sentence overview, **Key points** (bullets), **Decisions** (bullets, or \"None\"), \
and **Action items** (bullets with an owner when known, or \"None\"). If Me took part in a conversation with Them, \
finish with **Follow-up email**: a short, ready-to-send email from Me to Them, in Me's voice, that thanks them, recaps \
what was agreed, and lists next steps; omit it for presentations or recordings Me only listened to. \
Be factual and brief; do not invent details."
}

/// Separates the follow-up email from the rest of a notes summary.
#[must_use]
pub fn split_follow_up(summary: &str) -> (String, Option<String>) {
    let lines: Vec<&str> = summary.lines().collect();
    let heading = lines.iter().position(|line| {
        let label: String = line
            .chars()
            .filter(|character| character.is_alphanumeric() || character.is_whitespace())
            .collect::<String>()
            .to_lowercase();
        let label = label.split_whitespace().collect::<Vec<_>>().join(" ");
        matches!(label.as_str(), "followup email" | "follow up email")
    });
    let Some(heading) = heading else {
        return (summary.trim().to_owned(), None);
    };
    let before = lines[..heading].join("\n").trim().to_owned();
    let email = lines[heading + 1..].join("\n").trim().to_owned();
    (before, (!email.is_empty()).then_some(email))
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
            voice: None,
        }
    }

    #[test]
    fn settings_default_to_the_signed_in_chatgpt_route() {
        let settings = CopilotSettings::default();
        assert_eq!(settings.provider, CopilotProvider::ChatgptSubscription);
        assert_eq!(settings.answer_model(), "gpt-6-sol");
        assert!(settings.instant_answers);
        assert_eq!(settings.fast_model(), "gpt-6-sol");
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
        let all = render_transcript(&lines, &VoiceNames::new(), 10_000);
        assert_eq!(
            all,
            "[0:01] Them: first line that is fairly long\n[1:05] Me: second\n[1:02:05] Them: third?"
        );
        let recent = render_transcript(&lines, &VoiceNames::new(), 45);
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
        let prompt = answer_user_prompt("", QuickAction::Recap.instruction(), true, "");
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
    fn answers_draw_on_the_users_knowledge_first() {
        let without = answer_user_prompt("[0:01] Them: Price?", "Answer.", false, "  ");
        assert!(!without.contains("<my_knowledge>"));
        let with = answer_user_prompt(
            "[0:01] Them: Price?",
            "Answer.",
            false,
            "[pricing.md] Growth plan: $49 per seat.",
        );
        assert!(with.starts_with("<my_knowledge>\n[pricing.md] Growth plan: $49 per seat."));
        assert!(with.contains("override web results"));
        assert!(with.find("<my_knowledge>") < with.find("<transcript>"));
    }

    #[test]
    fn researched_answers_continue_correct_or_confirm_the_draft() {
        assert!(
            researched_guidance(" Our plan is $49. ")
                .contains("<draft>\nOur plan is $49.\n</draft>")
        );
        assert_eq!(researched_follow_up(""), FollowUp::Pending);
        // Streaming text that may still become a marker is held back.
        assert_eq!(researched_follow_up("Noth"), FollowUp::Pending);
        assert_eq!(researched_follow_up("**Correc"), FollowUp::Pending);
        assert_eq!(researched_follow_up("Nothing to add."), FollowUp::Confirmed);
        assert_eq!(
            researched_follow_up("**Correction:** It's $59 per seat."),
            FollowUp::Correction("It's $59 per seat.".into())
        );
        assert_eq!(
            researched_follow_up("- Annual billing saves 20%"),
            FollowUp::Adds("- Annual billing saves 20%".into())
        );
        assert_eq!(
            researched_follow_up("No, wait"),
            FollowUp::Adds("No, wait".into())
        );
        assert_eq!(
            combined_answer("Say: $49.", "- Saves 20% yearly"),
            "Say: $49.\n\n- Saves 20% yearly"
        );
        assert_eq!(combined_answer("Say: $49.", "Nothing to add."), "Say: $49.");
        assert_eq!(
            combined_answer("Say: $49.", "Correction: It's $59."),
            "Correction: It's $59."
        );
        assert_eq!(combined_answer("", "Full answer."), "Full answer.");
    }

    #[test]
    fn saved_notes_read_back_into_their_parts() {
        let markdown = meeting_notes_markdown(
            "Pilot planning",
            "2026-10-13 09:00",
            693_000,
            Some(
                "Agreed on a pilot.\n\n## Key points\n- 25 seats\n\n**Follow-up email**\nHi Sam,\n\nThanks.",
            ),
            &[TranscriptLine {
                id: 1,
                speaker: Speaker::Them,
                text: "Can we start small?".into(),
                at_ms: 2_000,
                voice: None,
            }],
            &VoiceNames::new(),
            &[(
                "Can we start small?".into(),
                "Offer a 25-seat pilot.".into(),
            )],
        );
        let notes = parse_meeting_notes(&markdown);
        assert_eq!(notes.title, "Pilot planning");
        assert_eq!(notes.started, "2026-10-13 09:00");
        assert_eq!(notes.duration, "11:33");
        // A heading inside the summary stays in the summary.
        assert_eq!(
            notes.summary,
            "Agreed on a pilot.\n\n## Key points\n- 25 seats"
        );
        assert_eq!(notes.follow_up.as_deref(), Some("Hi Sam,\n\nThanks."));
        assert!(notes.answers.starts_with("### Can we start small?"));
        assert!(notes.transcript.contains("Can we start small?"));
        assert_eq!(notes.overview(), "Agreed on a pilot.");
        let bare =
            parse_meeting_notes("# Untitled\n\n## Transcript\n\n_No speech was transcribed._\n");
        assert_eq!((bare.summary.as_str(), bare.overview().as_str()), ("", ""));
        let bullets = MeetingNotes {
            summary: "**Key points**\n- **Budget** approved".into(),
            ..MeetingNotes::default()
        };
        assert_eq!(bullets.overview(), "Budget approved");
    }

    #[test]
    fn the_other_sides_voices_are_named_only_when_there_are_several() {
        let mut lines = vec![
            line(1, Speaker::Them, "Welcome to the show.", 0),
            line(2, Speaker::Me, "Thanks for having me.", 2_000),
        ];
        lines[0].voice = Some(0);
        assert!(!has_several_voices(&lines));
        assert_eq!(
            render_transcript(&lines, &VoiceNames::new(), 1_000),
            "[0:00] Them: Welcome to the show.\n[0:02] Me: Thanks for having me."
        );
        let mut guest = line(3, Speaker::Them, "Happy to be here.", 4_000);
        guest.voice = Some(1);
        lines.push(guest);
        assert!(has_several_voices(&lines));
        let rendered = render_transcript(&lines, &VoiceNames::new(), 1_000);
        assert!(rendered.contains("[0:00] Them A: Welcome"));
        assert!(rendered.contains("[0:04] Them B: Happy"));
        assert!(rendered.contains("[0:02] Me: Thanks"));
        let notes = meeting_notes_markdown(
            "Show",
            "2026-10-13 09:00",
            5_000,
            None,
            &lines,
            &VoiceNames::new(),
            &[],
        );
        assert!(notes.contains("**[0:04] Them B:** Happy to be here."));
        assert!(notes.contains("**[0:02] You:** Thanks"));
        assert!(!notes.contains("**With:**"));
    }

    #[test]
    fn named_voices_are_labeled_by_name_everywhere() {
        let mut lines = vec![
            line(1, Speaker::Them, "I'm Sam, I run the show.", 0),
            line(2, Speaker::Me, "Thanks, Sam.", 2_000),
            line(3, Speaker::Them, "And I'm Joey.", 4_000),
            line(4, Speaker::Them, "Mm.", 5_000),
        ];
        lines[0].voice = Some(0);
        lines[2].voice = Some(1);
        let names = VoiceNames::from([(0, "Sam Lee".to_owned())]);
        let labels = SpeakerLabels::new(&lines, &names);
        assert_eq!(labels.display(&lines[0]), "Sam Lee");
        assert_eq!(labels.display(&lines[2]), "Them B");
        assert_eq!(labels.display(&lines[1]), "You");
        // A line without a voice among several voices stays unnamed.
        assert_eq!(labels.display(&lines[3]), "Them");
        assert_eq!(labels.voice_of(&lines[3]), None);
        assert_eq!(labels.label(&lines[0]), "Them A (Sam Lee)");
        assert_eq!(labels.voices(&lines), vec![0, 1]);
        let rendered = render_transcript(&lines, &names, 1_000);
        assert!(rendered.contains("[0:00] Them A (Sam Lee): I'm Sam"));
        assert!(rendered.contains("[0:04] Them B: And I'm Joey."));
        let notes =
            meeting_notes_markdown("Show", "2026-10-13 09:00", 6_000, None, &lines, &names, &[]);
        assert!(notes.contains("- **With:** Sam Lee\n"));
        assert!(notes.contains("**[0:00] Sam Lee:** I'm Sam"));
        assert_eq!(parse_meeting_notes(&notes).with, "Sam Lee");
        assert_eq!(given_name("Sam Lee"), "Sam");

        // Model-written labels map back to voices.
        assert_eq!(labels.voice_for_label("Them B"), Some(1));
        assert_eq!(labels.voice_for_label("them a (Sam Lee)"), Some(0));
        assert_eq!(labels.voice_for_label("Them"), None);
        assert_eq!(labels.voice_for_label("Me"), None);
        assert_eq!(labels.voice_for_label("Them AB"), None);

        // With one voice, every line from the other side is that person.
        let mut one = vec![
            line(1, Speaker::Them, "Hi there.", 0),
            line(2, Speaker::Them, "Yes.", 1_000),
        ];
        one[0].voice = Some(2);
        let names = VoiceNames::from([(2, "Ana".to_owned())]);
        let labels = SpeakerLabels::new(&one, &names);
        assert!(!labels.several_voices());
        assert_eq!(labels.display(&one[1]), "Ana");
        assert_eq!(labels.label(&one[1]), "Them (Ana)");
        assert_eq!(labels.voice_for_label("Them"), Some(2));
        assert_eq!(labels.voice_for_label("Them A"), Some(2));
        assert_eq!(labels.voice_label(2), "Them");
    }

    #[test]
    fn only_plausible_names_are_taken_from_models() {
        assert_eq!(clean_person_name(" Sam Lee "), Some("Sam Lee".into()));
        assert_eq!(clean_person_name("\"Sam\"."), Some("Sam".into()));
        assert_eq!(clean_person_name("Sam Lee (Guest)"), Some("Sam Lee".into()));
        assert_eq!(clean_person_name("Sam Lee | Acme"), Some("Sam Lee".into()));
        assert_eq!(
            clean_person_name("José Álvarez"),
            Some("José Álvarez".into())
        );
        for bad in [
            "",
            "null",
            "Unknown",
            "Host",
            "Them A",
            "Speaker 2",
            "the interviewer",
            "sam@acme.com",
            "User 1234",
            "A very long phrase that is not a name",
        ] {
            assert_eq!(clean_person_name(bad), None, "{bad}");
        }
        let insights = parse_insights(
            "{\"topic\":\"Intro\",\"speakers\":{\"Them A\":\"Sam\",\"Them B\":null,\"Them C\":\"Unknown\"}}",
        )
        .unwrap();
        assert_eq!(
            insights.speakers,
            BTreeMap::from([("Them A".to_owned(), "Sam".to_owned())])
        );
        let odd = parse_insights("{\"topic\":\"Intro\",\"speakers\":[\"Sam\"]}").unwrap();
        assert!(odd.speakers.is_empty());
        assert!(insights_system_prompt().contains("\"speakers\""));
    }

    #[test]
    fn meeting_windows_name_who_is_speaking() {
        let view = parse_call_view(
            "```json\n{\"call\":true,\"speaking\":\"Sam Lee (Guest)\",\"participants\":[\"Sam Lee\",\"James Gosnell\",\"You\",\"Joey\",\"joey\"],\"me\":\"James Gosnell\"}\n```",
        )
        .unwrap();
        assert!(view.call);
        assert_eq!(view.speaking.as_deref(), Some("Sam Lee"));
        assert_eq!(view.participants, vec!["Sam Lee", "Joey"]);
        // The user's own tile is never the other side.
        let mine = parse_call_view(
            "{\"call\":true,\"speaking\":\"James Gosnell\",\"participants\":[],\"me\":\"James Gosnell\"}",
        )
        .unwrap();
        assert_eq!(mine.speaking, None);
        let none = parse_call_view("{\"call\":false,\"speaking\":null}").unwrap();
        assert!(!none.call);
        assert!(parse_call_view("no json").is_none());
        let prompt = call_view_user_prompt(Some("Weekly sync | Microsoft Teams"), "I'm James.");
        assert!(prompt.contains("Window title: Weekly sync"));
        assert!(prompt.contains("About the user: I'm James."));
    }

    #[test]
    fn rewrites_quote_the_line_and_ask_for_only_the_new_one() {
        let instruction = rewrite_instruction(RewriteStyle::Shorter, " It's $20 per seat. ", "");
        assert!(instruction.contains("<line>\nIt's $20 per seat.\n</line>"));
        assert!(instruction.contains("at most 20 words"));
        assert!(instruction.contains("only the new line"));
        assert!(!instruction.contains("<answer>"));
        let specific = rewrite_instruction(
            RewriteStyle::Specific,
            "Let me get you the number.",
            "Let me get you the number.\n\nIt's $18 per seat.",
        );
        assert!(specific.contains("Never invent"));
        assert!(
            specific
                .contains("<answer>\nLet me get you the number.\n\nIt's $18 per seat.\n</answer>")
        );
        assert_eq!(
            RewriteStyle::ALL.map(RewriteStyle::label),
            ["Shorter", "More casual", "More specific", "Another way"]
        );
    }

    #[test]
    fn follow_up_emails_are_split_from_the_notes() {
        let summary = "Overview.\n\n**Action items**\n- Send deck\n\n**Follow-up email**\nHi Sam,\n\nThanks for today.";
        let (notes, email) = split_follow_up(summary);
        assert_eq!(notes, "Overview.\n\n**Action items**\n- Send deck");
        assert_eq!(email.as_deref(), Some("Hi Sam,\n\nThanks for today."));
        let (notes, email) = split_follow_up("## Follow up email:\n");
        assert_eq!((notes.as_str(), email), ("", None));
        assert_eq!(split_follow_up("Just notes.").1, None);
    }

    #[test]
    fn known_models_are_labeled_by_pace() {
        assert_eq!(model_pace("gpt-6-luna"), Some(ModelPace::Fastest));
        assert_eq!(model_pace("gpt-6-sol"), Some(ModelPace::Fast));
        assert_eq!(model_pace("gpt-6.1-sol"), Some(ModelPace::Slower));
        assert_eq!(model_pace("gpt-6-astra"), Some(ModelPace::Slower));
        assert_eq!(model_pace("some-new-model"), None);
        assert_eq!(
            model_pace(CopilotProvider::ChatgptSubscription.default_fast_model()),
            Some(ModelPace::Fastest)
        );
        // Instant drafts default to the larger model that is just as quick.
        assert_eq!(CopilotSettings::default().fast_model(), "gpt-6-sol");
        assert_eq!(
            model_pace(&CopilotSettings::default().fast_model()),
            Some(ModelPace::Fast)
        );
    }

    #[test]
    fn past_meetings_are_used_unless_turned_off() {
        assert!(CopilotSettings::default().use_past_meetings);
        let saved: CopilotSettings = serde_json::from_str("{\"save_notes\": false}").unwrap();
        assert!(saved.use_past_meetings);
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
            &VoiceNames::new(),
            &[("What should I say?".into(), "Say hello.".into())],
        );
        assert!(notes.contains("**Duration:** 2:05"));
        assert!(notes.contains("## Summary\n\nOverview"));
        assert!(notes.contains("### What should I say?"));
        assert!(notes.contains("**[0:00] Them:** Hi"));
    }
}
