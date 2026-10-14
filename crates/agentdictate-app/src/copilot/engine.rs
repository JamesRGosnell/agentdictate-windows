//! The meeting copilot session: audio in, transcript and answers out.
//!
//! One engine thread owns all session state. Audio processing, transcription
//! workers, and answer streams run on their own threads and report back
//! through [`EngineMsg`]; streamed answer text goes straight to the UI so a
//! busy engine never delays the words the user is waiting for.

use std::{
    collections::{HashSet, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    time::{Duration, Instant},
};

use agentdictate_core::{
    AnswerStatus, CopilotAction, CopilotEffort, CopilotEvent, CopilotInsights, CopilotProvider,
    CopilotSettings, INSTANT_GUIDANCE, ListenState, ModelChoice, QuickAction, Speaker,
    TranscriptLine, TurnContext, answer_system_prompt, answer_user_prompt, combined_answer,
    detected_question_instruction, insights_system_prompt, insights_user_prompt, is_echo_of,
    looks_like_question, meeting_notes_markdown, notes_system_prompt, parse_insights,
    parse_turn_verdict, render_transcript, researched_guidance, split_follow_up,
    turn_judge_system_prompt, turn_judge_user_prompt, typed_question_instruction,
    utterance_similarity, worth_judging,
};

use super::{
    knowledge::{Knowledge, fingerprint},
    providers::{AnswerProvider, AnswerRequest, complete},
    reveal::RevealGate,
    stream::StreamEvent,
    vad::{SegmentEvent, Utterance, wav_bytes},
    voices::{VoiceTracker, voice_print},
};

const TRANSCRIPTION_WORKERS: usize = 3;
const ANSWER_TRANSCRIPT_CHARS: usize = 24_000;
const INSIGHTS_TRANSCRIPT_CHARS: usize = 8_000;
const INSIGHTS_INTERVAL: Duration = Duration::from_secs(20);
/// New transcript lines required before insights are refreshed.
const INSIGHTS_MIN_NEW_LINES: usize = 2;
const AUTO_ANSWER_COOLDOWN: Duration = Duration::from_secs(6);
/// How often the knowledge folders are checked for added or edited files.
const KNOWLEDGE_CHECK_INTERVAL: Duration = Duration::from_secs(15);
/// Connections opened when a session starts: the judge, the instant draft,
/// and the researched answer run at the same time.
const WARM_CONNECTIONS: usize = 3;
/// Longest the researched answer waits for the instant draft it continues.
const INSTANT_WAIT: Duration = Duration::from_secs(8);
/// How long the wrap-up stays open when the user leaves it alone.
const WRAP_UP_TIMEOUT: Duration = Duration::from_secs(180);
const ECHO_WAIT: Duration = Duration::from_secs(4);
/// Longest a microphone line waits for the other side to finish speaking;
/// covers the longest system-audio utterance (18 s) plus its transcription.
const ECHO_WAIT_WHILE_SPEAKING: Duration = Duration::from_secs(30);
/// System-audio speech within this much of a microphone line can be its
/// source. Echo is heard while they speak, so this only absorbs timing drift
/// between the two captures; a reply that starts after they finish is kept.
const ECHO_WINDOW_MS: u64 = 500;
/// Silence after the other side's question before answering it: someone
/// asking you something waits; a rhetorical question runs straight on.
const QUESTION_PAUSE_MS: u64 = 1_000;
/// Turns still waiting this long, for example while sound keeps playing, are
/// dropped as stale.
const TURN_EXPIRY: Duration = Duration::from_secs(8);
/// Longest the fast model's judgment is awaited before deciding from the
/// wording alone.
const TURN_VERDICT_WAIT: Duration = Duration::from_secs(3);
/// System-audio speech starting this long after a question ended is new
/// speech. The segmenter needs over half a second of silence to end an
/// utterance, while another device's copy of the question starts with it.
const RESUMED_SPEECH_MS: u64 = 300;

/// Audio pipeline output, independent of the capture backend.
#[derive(Clone, Debug)]
pub(crate) enum AudioMsg {
    Level {
        speaker: Speaker,
        level: f32,
    },
    Speaking {
        speaker: Speaker,
        active: bool,
    },
    Utterance {
        speaker: Speaker,
        utterance: Utterance,
    },
    Problem(String),
    /// Informational capture state, such as active echo cancellation.
    Status(String),
}

impl AudioMsg {
    /// Messages for a segmenter event. A finished utterance is sent before the
    /// end of speech, so the engine never sees a speaker go quiet without the
    /// speech it judges echo and questions against.
    pub(crate) fn from_segment(speaker: Speaker, event: SegmentEvent) -> Vec<Self> {
        match event {
            SegmentEvent::SpeechStarted => vec![Self::Speaking {
                speaker,
                active: true,
            }],
            SegmentEvent::SpeechDiscarded => vec![Self::Speaking {
                speaker,
                active: false,
            }],
            SegmentEvent::Utterance(utterance) => vec![
                Self::Utterance { speaker, utterance },
                Self::Speaking {
                    speaker,
                    active: false,
                },
            ],
        }
    }
}

pub(crate) trait AudioControl: Send {
    fn set_paused(&self, paused: bool);
    /// Stops capture, flushing any utterance in progress through the sink.
    fn stop(self: Box<Self>);
}

pub(crate) type AudioSink = Arc<dyn Fn(AudioMsg) + Send + Sync>;

/// Desktop services the engine needs from the host platform.
pub(crate) trait Platform: Send + Sync {
    fn start_audio(
        &self,
        microphone: bool,
        system_audio: bool,
        sink: AudioSink,
    ) -> Result<Box<dyn AudioControl>, String>;
    fn capture_screen(&self) -> Result<Vec<u8>, String>;
    fn copy_text(&self, text: &str);
    fn open(&self, target: &str);
}

pub(crate) type ProviderFactory =
    Arc<dyn Fn(&CopilotSettings) -> Result<Box<dyn AnswerProvider>, String> + Send + Sync>;
pub(crate) type Transcriber = Arc<dyn Fn(Vec<u8>, &str) -> Result<String, String> + Send + Sync>;
pub(crate) type ModelLoader = Arc<dyn Fn(CopilotProvider) -> Vec<ModelChoice> + Send + Sync>;
pub(crate) type UiSink = Arc<dyn Fn(CopilotEvent) + Send + Sync>;

pub(crate) struct Services {
    pub platform: Arc<dyn Platform>,
    pub providers: ProviderFactory,
    pub transcriber: Transcriber,
    pub models: ModelLoader,
    pub settings_path: PathBuf,
    pub notes_dir: PathBuf,
    /// Documents the user wants answers to draw on.
    pub knowledge_dir: PathBuf,
    pub language: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HotkeyCommand {
    Assist,
    Toggle,
    Ask,
}

pub(crate) enum EngineMsg {
    Action(CopilotAction),
    Audio(AudioMsg),
    Transcribed {
        speaker: Speaker,
        start_ms: u64,
        end_ms: u64,
        text: Result<String, String>,
    },
    AnswerFinished {
        id: u64,
        title: String,
        text: String,
    },
    InsightsFinished(Result<Option<CopilotInsights>, String>),
    /// Whether the user should respond to a turn; `None` when the judge failed.
    TurnVerdict {
        id: u64,
        respond: Option<bool>,
    },
    Hotkey(HotkeyCommand),
    ShowRequested,
    ModelsLoaded(CopilotProvider, Vec<ModelChoice>),
    KnowledgeLoaded {
        knowledge: Arc<Knowledge>,
        fingerprint: u64,
    },
}

struct TranscriptionJob {
    speaker: Speaker,
    start_ms: u64,
    end_ms: u64,
    wav: Vec<u8>,
}

struct ThemSpan {
    start_ms: u64,
    end_ms: u64,
    text: Option<String>,
    /// The other side's voice that spoke, when told apart.
    voice: Option<u8>,
}

/// A line from the other side that may call for the user to respond.
struct PendingTurn {
    id: u64,
    text: String,
    end_ms: u64,
    verdict: Option<bool>,
    /// Worded like a question, so an answer is drafted before the verdict.
    likely: bool,
    draft: Option<Draft>,
    heard: Instant,
}

/// An answer drafted ahead of the verdict, hidden until confirmed.
struct Draft {
    id: u64,
    gate: Arc<RevealGate>,
    /// Title and text, when the draft finished while still hidden.
    finished: Option<(String, String)>,
}

struct PendingMine {
    line: TranscriptLine,
    end_ms: u64,
    deadline: Instant,
}

struct ActiveAnswer {
    id: u64,
    automatic: bool,
    cancel: Arc<AtomicBool>,
    gate: Arc<RevealGate>,
}

/// Engine state, owned by the engine thread.
pub(crate) struct Engine {
    services: Services,
    settings: CopilotSettings,
    ui: UiSink,
    messages: Sender<EngineMsg>,
    started: Instant,
    session_key: String,
    audio: Option<Box<dyn AudioControl>>,
    paused: bool,
    jobs: Option<Sender<TranscriptionJob>>,
    in_flight: Arc<AtomicUsize>,
    transcript: Vec<TranscriptLine>,
    next_line: u64,
    them_spans: VecDeque<ThemSpan>,
    voices: VoiceTracker,
    pending_mine: Vec<PendingMine>,
    them_speaking: bool,
    pending_turn: Option<PendingTurn>,
    next_turn: u64,
    /// Drafts discarded unseen, kept out of the meeting notes.
    discarded: HashSet<u64>,
    knowledge: Arc<Knowledge>,
    knowledge_fingerprint: Option<u64>,
    knowledge_loading: bool,
    knowledge_checked_at: Instant,
    notes_path: Option<PathBuf>,
    next_answer: u64,
    active_answer: Option<ActiveAnswer>,
    answers: Vec<(String, String)>,
    last_auto_question: Option<(String, Instant)>,
    insights_in_flight: bool,
    last_insights_at: Option<Instant>,
    lines_at_last_insights: usize,
    insights: CopilotInsights,
    chatgpt_models: Vec<String>,
    ending: bool,
}

impl Engine {
    pub(crate) fn new(
        services: Services,
        settings: CopilotSettings,
        ui: UiSink,
        messages: Sender<EngineMsg>,
    ) -> Self {
        Self {
            services,
            settings,
            ui,
            messages,
            started: Instant::now(),
            session_key: uuid::Uuid::new_v4().to_string(),
            audio: None,
            paused: false,
            jobs: None,
            in_flight: Arc::new(AtomicUsize::new(0)),
            transcript: Vec::new(),
            next_line: 1,
            them_spans: VecDeque::new(),
            voices: VoiceTracker::default(),
            pending_mine: Vec::new(),
            them_speaking: false,
            pending_turn: None,
            next_turn: 1,
            discarded: HashSet::new(),
            knowledge: Arc::new(Knowledge::default()),
            knowledge_fingerprint: None,
            knowledge_loading: false,
            knowledge_checked_at: Instant::now(),
            notes_path: None,
            next_answer: 1,
            active_answer: None,
            answers: Vec::new(),
            last_auto_question: None,
            insights_in_flight: false,
            last_insights_at: None,
            lines_at_last_insights: 0,
            insights: CopilotInsights::default(),
            chatgpt_models: Vec::new(),
            ending: false,
        }
    }

    fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Runs the session until the user ends it.
    pub(crate) fn run(mut self, incoming: Receiver<EngineMsg>) {
        self.start_transcription_workers();
        self.load_models(self.settings.provider);
        self.load_knowledge();
        self.warm_up();
        self.start_audio();
        loop {
            match incoming.recv_timeout(Duration::from_millis(250)) {
                Ok(message) => {
                    if self.handle(message) {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.tick();
        }
        self.finish(&incoming);
    }

    fn start_transcription_workers(&mut self) {
        let (jobs, queue) = mpsc::channel::<TranscriptionJob>();
        let queue = Arc::new(Mutex::new(queue));
        for index in 0..TRANSCRIPTION_WORKERS {
            let queue = Arc::clone(&queue);
            let transcriber = Arc::clone(&self.services.transcriber);
            let messages = self.messages.clone();
            let in_flight = Arc::clone(&self.in_flight);
            let language = self.services.language.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("copilot-transcribe-{index}"))
                .spawn(move || {
                    loop {
                        let job = match queue.lock() {
                            Ok(queue) => queue.recv(),
                            Err(_) => return,
                        };
                        let Ok(job) = job else { return };
                        let started = Instant::now();
                        let text = transcriber(job.wav, &language);
                        tracing::info!(
                            speaker = ?job.speaker,
                            audio_ms = job.end_ms.saturating_sub(job.start_ms),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            ok = text.is_ok(),
                            "copilot utterance transcribed"
                        );
                        in_flight.fetch_sub(1, Ordering::AcqRel);
                        if messages
                            .send(EngineMsg::Transcribed {
                                speaker: job.speaker,
                                start_ms: job.start_ms,
                                end_ms: job.end_ms,
                                text,
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                });
            if let Err(error) = spawned {
                tracing::error!(%error, "could not start copilot transcription worker");
            }
        }
        self.jobs = Some(jobs);
    }

    fn load_models(&self, provider: CopilotProvider) {
        let loader = Arc::clone(&self.services.models);
        let messages = self.messages.clone();
        let _ = std::thread::Builder::new()
            .name("copilot-models".into())
            .spawn(move || {
                let models = loader(provider);
                let _ = messages.send(EngineMsg::ModelsLoaded(provider, models));
            });
    }

    /// Signs in and opens connections before the first question, one per
    /// lane that runs at once (judge, instant draft, researched answer), so
    /// the first answer is as quick as later ones.
    fn warm_up(&self) {
        if self.settings.provider == CopilotProvider::ClaudeCode {
            // Each request starts a CLI process; there is nothing to keep warm.
            return;
        }
        let mut settings = self.settings.clone();
        settings.set_answer_model(self.settings.provider.default_fast_model());
        let request = AnswerRequest {
            system: "Reply with OK.".to_owned(),
            user: "OK?".to_owned(),
            image_jpeg: None,
            model: settings.answer_model(),
            effort: CopilotEffort::Low,
            web_search: false,
            max_output_tokens: 16,
            cache_key: format!("{}-warm-up", self.session_key),
        };
        for lane in 0..WARM_CONNECTIONS {
            let providers = Arc::clone(&self.services.providers);
            let settings = settings.clone();
            let request = request.clone();
            let _ = std::thread::Builder::new()
                .name(format!("copilot-warm-up-{lane}"))
                .spawn(move || {
                    let started = Instant::now();
                    let cancel = AtomicBool::new(false);
                    let result = providers(&settings)
                        .and_then(|provider| complete(provider.as_ref(), &request, &cancel));
                    match result {
                        Ok(_) => tracing::info!(
                            lane,
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "copilot connection warmed"
                        ),
                        Err(error) => tracing::warn!(lane, %error, "copilot warm-up failed"),
                    }
                });
        }
    }

    /// The folders answers draw on: the knowledge folder, plus meeting notes
    /// when the user allows it.
    fn knowledge_folders(&self) -> (PathBuf, Option<PathBuf>) {
        (
            self.services.knowledge_dir.clone(),
            self.settings
                .use_past_meetings
                .then(|| self.services.notes_dir.clone()),
        )
    }

    fn load_knowledge(&mut self) {
        if self.knowledge_loading {
            return;
        }
        self.knowledge_loading = true;
        self.knowledge_checked_at = Instant::now();
        let (documents, meetings) = self.knowledge_folders();
        let messages = self.messages.clone();
        let _ = std::thread::Builder::new()
            .name("copilot-knowledge".into())
            .spawn(move || {
                let mut folders = vec![documents.as_path()];
                folders.extend(meetings.as_deref());
                let fingerprint = fingerprint(&folders);
                let knowledge = Arc::new(Knowledge::load(&documents, meetings.as_deref()));
                let _ = messages.send(EngineMsg::KnowledgeLoaded {
                    knowledge,
                    fingerprint,
                });
            });
    }

    /// Reloads the knowledge when its files have changed.
    fn check_knowledge(&mut self) {
        if self.knowledge_loading || self.knowledge_checked_at.elapsed() < KNOWLEDGE_CHECK_INTERVAL
        {
            return;
        }
        self.knowledge_checked_at = Instant::now();
        let (documents, meetings) = self.knowledge_folders();
        let mut folders = vec![documents.as_path()];
        folders.extend(meetings.as_deref());
        if self.knowledge_fingerprint != Some(fingerprint(&folders)) {
            self.load_knowledge();
        }
    }

    fn start_audio(&mut self) {
        if self.audio.is_some() {
            return;
        }
        if !self.settings.capture_microphone && !self.settings.capture_system_audio {
            (self.ui)(CopilotEvent::Listen(ListenState::Unavailable(
                "Turn on the microphone or system audio in settings.".into(),
            )));
            return;
        }
        (self.ui)(CopilotEvent::Listen(ListenState::Starting));
        let messages = self.messages.clone();
        let sink: AudioSink = Arc::new(move |message| {
            let _ = messages.send(EngineMsg::Audio(message));
        });
        match self.services.platform.start_audio(
            self.settings.capture_microphone,
            self.settings.capture_system_audio,
            sink,
        ) {
            Ok(audio) => {
                self.audio = Some(audio);
                self.paused = false;
                (self.ui)(CopilotEvent::Listen(ListenState::Listening));
            }
            Err(error) => {
                tracing::error!(%error, "copilot audio capture failed");
                (self.ui)(CopilotEvent::Listen(ListenState::Unavailable(error)));
            }
        }
    }

    fn stop_audio(&mut self) {
        if let Some(audio) = self.audio.take() {
            audio.stop();
        }
    }

    /// Returns true when the session should end.
    fn handle(&mut self, message: EngineMsg) -> bool {
        match message {
            EngineMsg::Action(action) => return self.handle_action(action),
            EngineMsg::Audio(message) => self.handle_audio(message),
            EngineMsg::Transcribed {
                speaker,
                start_ms,
                end_ms,
                text,
            } => self.handle_transcribed(speaker, start_ms, end_ms, text),
            EngineMsg::AnswerFinished { id, title, text } => {
                self.handle_answer_finished(id, title, text);
            }
            EngineMsg::InsightsFinished(result) => self.handle_insights(result),
            EngineMsg::TurnVerdict { id, respond } => {
                if let Some(turn) = self.pending_turn.as_mut().filter(|turn| turn.id == id) {
                    // Without a verdict, the wording decides.
                    turn.verdict = Some(respond.unwrap_or(turn.likely));
                }
                self.evaluate_turn();
            }
            EngineMsg::Hotkey(HotkeyCommand::Assist) => {
                // The shortcut works while the copilot is hidden.
                (self.ui)(CopilotEvent::Show);
                self.answer(AnswerKind::Quick(QuickAction::WhatToSay))
            }
            EngineMsg::Hotkey(HotkeyCommand::Toggle) => (self.ui)(CopilotEvent::ToggleVisibility),
            EngineMsg::Hotkey(HotkeyCommand::Ask) => {
                (self.ui)(CopilotEvent::Show);
                (self.ui)(CopilotEvent::FocusAsk);
            }
            EngineMsg::ShowRequested => (self.ui)(CopilotEvent::Show),
            EngineMsg::KnowledgeLoaded {
                knowledge,
                fingerprint,
            } => {
                let summary = knowledge.summary();
                tracing::info!(
                    documents = summary.documents,
                    meetings = summary.meetings,
                    words = summary.words,
                    unreadable = summary.unreadable.len(),
                    "copilot knowledge loaded"
                );
                self.knowledge = knowledge;
                self.knowledge_fingerprint = Some(fingerprint);
                self.knowledge_loading = false;
                (self.ui)(CopilotEvent::Knowledge(summary));
            }
            EngineMsg::ModelsLoaded(provider, models) => {
                if provider == CopilotProvider::ChatgptSubscription {
                    self.chatgpt_models = models.iter().map(|model| model.id.clone()).collect();
                }
                if provider == self.settings.provider {
                    (self.ui)(CopilotEvent::Models(models));
                }
            }
        }
        false
    }

    fn handle_action(&mut self, action: CopilotAction) -> bool {
        match action {
            CopilotAction::ToggleListening => {
                if self.audio.is_none() {
                    self.start_audio();
                } else {
                    self.paused = !self.paused;
                    if let Some(audio) = &self.audio {
                        audio.set_paused(self.paused);
                    }
                    (self.ui)(CopilotEvent::Listen(if self.paused {
                        ListenState::Paused
                    } else {
                        ListenState::Listening
                    }));
                }
            }
            CopilotAction::Assist => self.answer(AnswerKind::Quick(QuickAction::WhatToSay)),
            CopilotAction::Quick(action) => self.answer(AnswerKind::Quick(action)),
            CopilotAction::Ask(question) => {
                if !question.trim().is_empty() {
                    self.answer(AnswerKind::Typed(question));
                }
            }
            CopilotAction::AnswerQuestion(question) => {
                self.answer(AnswerKind::Detected {
                    question,
                    automatic: false,
                });
            }
            CopilotAction::Cancel => self.cancel_answer(),
            CopilotAction::ClearAnswers => self.cancel_answer(),
            CopilotAction::UpdateSettings(settings) => self.update_settings(*settings),
            CopilotAction::OpenNotesFolder => {
                let _ = std::fs::create_dir_all(&self.services.notes_dir);
                self.services
                    .platform
                    .open(&self.services.notes_dir.to_string_lossy());
            }
            CopilotAction::OpenNotes => {
                if let Some(path) = &self.notes_path {
                    self.services.platform.open(&path.to_string_lossy());
                }
            }
            CopilotAction::OpenKnowledgeFolder => {
                let _ = std::fs::create_dir_all(&self.services.knowledge_dir);
                self.services
                    .platform
                    .open(&self.services.knowledge_dir.to_string_lossy());
                // Pick up new files soon after the user adds them.
                self.knowledge_checked_at = Instant::now()
                    .checked_sub(KNOWLEDGE_CHECK_INTERVAL - Duration::from_secs(3))
                    .unwrap_or_else(Instant::now);
            }
            CopilotAction::OpenUrl(url) => {
                if url.starts_with("https://") || url.starts_with("http://") {
                    self.services.platform.open(&url);
                }
            }
            CopilotAction::CopyText(text) => {
                self.services.platform.copy_text(&text);
                (self.ui)(CopilotEvent::Notice("Copied".into()));
            }
            CopilotAction::End => return true,
        }
        false
    }

    fn update_settings(&mut self, settings: CopilotSettings) {
        let previous = std::mem::replace(&mut self.settings, settings);
        if let Err(error) = super::store::save(&self.services.settings_path, &self.settings) {
            tracing::error!(%error, "could not save copilot settings");
            (self.ui)(CopilotEvent::Notice("Settings could not be saved".into()));
        }
        tracing::info!(settings = ?self.settings.redacted(), "copilot settings updated");
        (self.ui)(CopilotEvent::Settings(Box::new(self.settings.clone())));
        if previous.provider != self.settings.provider {
            self.load_models(self.settings.provider);
        }
        if previous.use_past_meetings != self.settings.use_past_meetings {
            self.knowledge_fingerprint = None;
            self.load_knowledge();
        }
        if previous.capture_microphone != self.settings.capture_microphone
            || previous.capture_system_audio != self.settings.capture_system_audio
        {
            self.stop_audio();
            self.start_audio();
        }
    }

    fn handle_audio(&mut self, message: AudioMsg) {
        match message {
            AudioMsg::Level { speaker, level } => {
                // Levels are coalesced per source by the capture threads.
                (self.ui)(match speaker {
                    Speaker::Me => CopilotEvent::Levels {
                        me: level,
                        them: -1.0,
                    },
                    Speaker::Them => CopilotEvent::Levels {
                        me: -1.0,
                        them: level,
                    },
                });
            }
            AudioMsg::Speaking { speaker, active } => {
                if speaker == Speaker::Them {
                    self.them_speaking = active;
                    if !active {
                        self.evaluate_turn();
                    }
                }
                (self.ui)(CopilotEvent::Speaking { speaker, active });
            }
            AudioMsg::Utterance { speaker, utterance } => self.queue_utterance(speaker, utterance),
            AudioMsg::Problem(problem) => {
                tracing::warn!(%problem, "copilot audio problem");
                (self.ui)(CopilotEvent::Notice(problem));
            }
            AudioMsg::Status(status) => tracing::info!(%status, "copilot audio status"),
        }
    }

    fn queue_utterance(&mut self, speaker: Speaker, utterance: Utterance) {
        // Times bound the speech itself on the session clock, so the two
        // sides line up: the segmenter releases an utterance after a stretch
        // of silence, and pads it with audio from before speech began.
        let released = self
            .elapsed_ms()
            .saturating_sub(utterance.released_after_ms);
        let start_ms = released.saturating_sub(utterance.duration_ms()) + utterance.lead_ms;
        let end_ms = released.saturating_sub(utterance.tail_ms).max(start_ms);
        if speaker == Speaker::Them {
            // Voices are assigned in the order people spoke.
            let voice = self.voices.assign(voice_print(&utterance.samples));
            self.them_spans.push_back(ThemSpan {
                start_ms,
                end_ms,
                text: None,
                voice,
            });
            while self
                .them_spans
                .front()
                .is_some_and(|span| span.end_ms + 60_000 < end_ms)
            {
                self.them_spans.pop_front();
            }
        }
        let Some(jobs) = &self.jobs else { return };
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        if jobs
            .send(TranscriptionJob {
                speaker,
                start_ms,
                end_ms,
                wav: wav_bytes(&utterance.samples),
            })
            .is_err()
        {
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
        }
    }

    fn handle_transcribed(
        &mut self,
        speaker: Speaker,
        start_ms: u64,
        end_ms: u64,
        text: Result<String, String>,
    ) {
        let text = match text {
            Ok(text) => text,
            Err(error) => {
                tracing::warn!(%error, ?speaker, "copilot transcription failed");
                (self.ui)(CopilotEvent::Notice(error));
                if speaker == Speaker::Them {
                    self.forget_them_span(start_ms, end_ms);
                }
                return;
            }
        };
        let text = text.trim().to_owned();
        if is_noise_transcript(&text, end_ms.saturating_sub(start_ms)) {
            if speaker == Speaker::Them {
                self.forget_them_span(start_ms, end_ms);
            }
            return;
        }
        let mut line = TranscriptLine {
            id: self.next_line,
            speaker,
            text,
            at_ms: start_ms,
            voice: None,
        };
        self.next_line += 1;
        match speaker {
            Speaker::Them => {
                if let Some(span) = self
                    .them_spans
                    .iter_mut()
                    .find(|span| span.start_ms == start_ms && span.end_ms == end_ms)
                {
                    span.text = Some(line.text.clone());
                    line.voice = span.voice;
                }
                // The same call can reach two playback devices (for example a
                // virtual device chained to speakers); keep one copy.
                let duplicate = self.transcript.iter().rev().take(8).any(|existing| {
                    existing.speaker == Speaker::Them
                        && existing.at_ms.abs_diff(line.at_ms) < 4_000
                        && utterance_similarity(&existing.text, &line.text) >= 0.8
                });
                if duplicate {
                    tracing::info!("dropped duplicate system-audio line");
                    return;
                }
                let text = line.text.clone();
                self.publish(line);
                self.resolve_pending_mine(false);
                self.consider_turn(text, end_ms);
            }
            Speaker::Me => {
                let nearby = self.nearby_them(start_ms, end_ms);
                if nearby.is_empty() && !self.them_speaking {
                    self.publish(line);
                } else {
                    // While the other side is still mid-utterance, its span
                    // does not exist yet; wait for it before judging echo.
                    let wait = if self.them_speaking {
                        ECHO_WAIT_WHILE_SPEAKING
                    } else {
                        ECHO_WAIT
                    };
                    self.pending_mine.push(PendingMine {
                        line,
                        end_ms,
                        deadline: Instant::now() + wait,
                    });
                    self.resolve_pending_mine(false);
                }
            }
        }
    }

    fn forget_them_span(&mut self, start_ms: u64, end_ms: u64) {
        self.them_spans
            .retain(|span| !(span.start_ms == start_ms && span.end_ms == end_ms));
        self.resolve_pending_mine(false);
    }

    /// Indices of system-audio spans near enough in time to be the source of
    /// a microphone utterance.
    fn nearby_them(&self, start_ms: u64, end_ms: u64) -> Vec<usize> {
        self.them_spans
            .iter()
            .enumerate()
            .filter(|(_, span)| {
                span.end_ms + ECHO_WINDOW_MS >= start_ms && span.start_ms <= end_ms + ECHO_WINDOW_MS
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// Publishes or drops microphone lines that might be speaker echo once
    /// the overlapping system-audio text is known.
    fn resolve_pending_mine(&mut self, force: bool) {
        let now = Instant::now();
        let pending = std::mem::take(&mut self.pending_mine);
        for item in pending {
            let start_ms = item.line.at_ms;
            let nearby = self.nearby_them(start_ms, item.end_ms);
            let all_known = nearby
                .iter()
                .all(|index| self.them_spans[*index].text.is_some());
            if (!all_known || self.them_speaking) && !force && now < item.deadline {
                self.pending_mine.push(item);
                continue;
            }
            // Echo fragments can straddle two system-audio utterances.
            let theirs = nearby
                .iter()
                .filter_map(|index| self.them_spans[*index].text.as_deref())
                .collect::<Vec<_>>()
                .join(" ");
            let echo = !theirs.is_empty() && is_echo_of(&item.line.text, &theirs);
            if echo {
                tracing::info!("dropped microphone echo of system audio");
            } else {
                self.publish(item.line);
            }
        }
    }

    fn publish(&mut self, line: TranscriptLine) {
        let position = self
            .transcript
            .iter()
            .position(|existing| existing.at_ms > line.at_ms)
            .unwrap_or(self.transcript.len());
        self.transcript.insert(position, line.clone());
        (self.ui)(CopilotEvent::Transcript(line));
    }

    fn handle_answer_finished(&mut self, id: u64, title: String, text: String) {
        if self
            .active_answer
            .as_ref()
            .is_some_and(|active| active.id == id)
        {
            self.active_answer = None;
        }
        if self.discarded.remove(&id) || text.trim().is_empty() {
            return;
        }
        // A draft that finished unseen joins the notes only if it is shown.
        if let Some(draft) = self
            .pending_turn
            .as_mut()
            .and_then(|turn| turn.draft.as_mut())
            .filter(|draft| draft.id == id && draft.gate.is_hidden())
        {
            draft.finished = Some((title, text));
            return;
        }
        self.answers.push((title, text));
    }

    /// Judges whether a line from the other side calls for the user to
    /// respond. A line worded like a question gets its answer drafted at once,
    /// hidden until the judgment and a pause confirm it, so confirming adds no
    /// delay; other lines are answered once the judge asks for it.
    fn consider_turn(&mut self, text: String, end_ms: u64) {
        if !self.settings.auto_answer
            || self.ending
            || !self.insights.situation.user_takes_part()
            || !worth_judging(&text)
        {
            return;
        }
        self.drop_turn();
        let id = self.next_turn;
        self.next_turn += 1;
        self.request_turn_verdict(id, &text);
        let likely = looks_like_question(&text);
        let draft = if likely && self.active_answer.is_none() && self.may_auto_answer(&text) {
            self.answer_with(
                AnswerKind::Detected {
                    question: text.clone(),
                    automatic: true,
                },
                true,
            )
            .map(|(id, gate)| Draft {
                id,
                gate,
                finished: None,
            })
        } else {
            None
        };
        self.pending_turn = Some(PendingTurn {
            id,
            text,
            end_ms,
            verdict: None,
            likely,
            draft,
            heard: Instant::now(),
        });
    }

    fn request_turn_verdict(&self, id: u64, latest: &str) {
        let mut settings = self.settings.clone();
        // Judging keeps pace with the call whatever model drafts answers.
        settings.set_answer_model(self.settings.provider.default_fast_model());
        let now_ms = self.elapsed_ms();
        let context = TurnContext {
            situation: self.insights.situation,
            me_last_spoke_secs: self
                .transcript
                .iter()
                .rev()
                .find(|line| line.speaker == Speaker::Me)
                .map(|line| now_ms.saturating_sub(line.at_ms) / 1_000),
        };
        let request = AnswerRequest {
            system: turn_judge_system_prompt().to_owned(),
            user: turn_judge_user_prompt(
                &render_transcript(&self.transcript, INSIGHTS_TRANSCRIPT_CHARS),
                latest,
                context,
            ),
            image_jpeg: None,
            model: settings.answer_model(),
            effort: CopilotEffort::Low,
            web_search: false,
            max_output_tokens: 200,
            cache_key: format!("{}-turns", self.session_key),
        };
        let providers = Arc::clone(&self.services.providers);
        let messages = self.messages.clone();
        let _ = std::thread::Builder::new()
            .name("copilot-turn".into())
            .spawn(move || {
                let cancel = AtomicBool::new(false);
                let respond = providers(&settings)
                    .and_then(|provider| complete(provider.as_ref(), &request, &cancel))
                    .ok()
                    .and_then(|text| parse_turn_verdict(&text));
                let _ = messages.send(EngineMsg::TurnVerdict { id, respond });
            });
    }

    /// Shows or starts the answer to the pending turn once the other side has
    /// paused and the judge agrees; discards it if they kept talking.
    fn evaluate_turn(&mut self) {
        let Some(turn) = self.pending_turn.as_ref() else {
            return;
        };
        let resumed = self
            .them_spans
            .iter()
            .any(|span| span.start_ms >= turn.end_ms + RESUMED_SPEECH_MS);
        if resumed {
            tracing::info!("the other side kept talking; not answering");
            self.drop_turn();
            return;
        }
        if turn.heard.elapsed() > TURN_EXPIRY {
            self.drop_turn();
            return;
        }
        // A brief sound may be the start of more speech; wait to find out.
        if self.them_speaking || self.elapsed_ms() < turn.end_ms + QUESTION_PAUSE_MS {
            return;
        }
        let respond = match turn.verdict {
            Some(respond) => respond,
            None if turn.heard.elapsed() >= TURN_VERDICT_WAIT => turn.likely,
            None => return,
        };
        if !respond {
            tracing::info!("the line was not aimed at the user; not answering");
            self.drop_turn();
            return;
        }
        let Some(turn) = self.pending_turn.take() else {
            return;
        };
        if let Some(draft) = turn.draft
            && draft.gate.reveal()
        {
            tracing::info!(
                waited_ms = turn.heard.elapsed().as_millis() as u64,
                "answering the other side automatically"
            );
            self.last_auto_question = Some((turn.text, Instant::now()));
            if let Some(finished) = draft.finished {
                self.answers.push(finished);
            }
            return;
        }
        self.maybe_auto_answer(turn.text);
    }

    /// Forgets the pending turn, discarding its hidden draft unseen.
    fn drop_turn(&mut self) {
        let Some(draft) = self.pending_turn.take().and_then(|turn| turn.draft) else {
            return;
        };
        if draft.gate.discard() {
            self.discarded.insert(draft.id);
            if let Some(active) = self.active_answer.take_if(|active| active.id == draft.id) {
                active.cancel.store(true, Ordering::Relaxed);
            }
        }
    }

    /// Whether an automatic answer may start now: not a repeat of the last
    /// one, and never over an answer the user asked for.
    fn may_auto_answer(&self, question: &str) -> bool {
        if let Some((previous, at)) = &self.last_auto_question
            && (at.elapsed() < AUTO_ANSWER_COOLDOWN
                || utterance_similarity(previous, question) >= 0.6)
        {
            return false;
        }
        self.active_answer
            .as_ref()
            .is_none_or(|active| active.automatic)
    }

    fn maybe_auto_answer(&mut self, question: String) {
        if !self.settings.auto_answer || self.ending || !self.may_auto_answer(&question) {
            return;
        }
        tracing::info!("answering the other side automatically");
        self.last_auto_question = Some((question.clone(), Instant::now()));
        self.answer(AnswerKind::Detected {
            question,
            automatic: true,
        });
    }

    fn cancel_answer(&mut self) {
        if let Some(active) = self.active_answer.take() {
            active.cancel.store(true, Ordering::Relaxed);
            if active.gate.discard() {
                self.discarded.insert(active.id);
            } else {
                active.gate.send(CopilotEvent::AnswerStatus {
                    id: active.id,
                    status: AnswerStatus::Done,
                });
            }
        }
    }

    fn answer(&mut self, kind: AnswerKind) {
        self.answer_with(kind, false);
    }

    /// Starts an answer, shown at once or held hidden for [`RevealGate::reveal`].
    fn answer_with(&mut self, kind: AnswerKind, hidden: bool) -> Option<(u64, Arc<RevealGate>)> {
        if self.ending {
            return None;
        }
        self.cancel_answer();
        let id = self.next_answer;
        self.next_answer += 1;
        let (title, instruction, automatic, wants_screen) = match &kind {
            AnswerKind::Quick(action) => (
                action.label().to_owned(),
                action.instruction().to_owned(),
                false,
                action.wants_screen(),
            ),
            AnswerKind::Typed(question) => (
                question.trim().to_owned(),
                typed_question_instruction(question),
                false,
                true,
            ),
            AnswerKind::Detected {
                question,
                automatic,
            } => (
                question.trim().to_owned(),
                detected_question_instruction(question),
                *automatic,
                false,
            ),
        };
        let use_screen = wants_screen && self.settings.screen_context;
        let model = self.settings.answer_model();
        let cancel = Arc::new(AtomicBool::new(false));
        let ui = Arc::clone(&self.ui);
        let gate = Arc::new(if hidden {
            RevealGate::hidden(ui)
        } else {
            RevealGate::shown(ui)
        });
        self.active_answer = Some(ActiveAnswer {
            id,
            automatic,
            cancel: Arc::clone(&cancel),
            gate: Arc::clone(&gate),
        });
        gate.send(CopilotEvent::AnswerStarted {
            id,
            title: title.clone(),
            used_screen: use_screen,
            model: model.clone(),
            automatic,
        });
        let transcript = render_transcript(&self.transcript, ANSWER_TRANSCRIPT_CHARS);
        let knowledge = self.knowledge_for(&kind);
        let settings = self.settings.clone();
        let fast_model = settings.fast_model();
        // The instant draft also helps when it uses the answer model: without
        // web search it starts in about a second.
        let instant = settings.instant_answers && (fast_model != model || settings.web_search);
        let providers = Arc::clone(&self.services.providers);
        let platform = Arc::clone(&self.services.platform);
        let thread_gate = Arc::clone(&gate);
        let messages = self.messages.clone();
        let session_key = self.session_key.clone();
        let spawned = std::thread::Builder::new()
            .name("copilot-answer".into())
            .spawn(move || {
                let gate = thread_gate;
                let started = Instant::now();
                let image = if use_screen {
                    match platform.capture_screen() {
                        Ok(jpeg) => Some(Arc::new(jpeg)),
                        Err(error) => {
                            tracing::warn!(%error, "screen capture failed");
                            None
                        }
                    }
                } else {
                    None
                };
                let system = answer_system_prompt(&settings);
                let user =
                    answer_user_prompt(&transcript, &instruction, image.is_some(), &knowledge);
                let instant_text = Arc::new(Mutex::new(String::new()));
                let (instant_done, instant_finished) = mpsc::channel::<()>();
                if instant {
                    let mut fast_settings = settings.clone();
                    fast_settings.set_answer_model(fast_model.clone());
                    let request = AnswerRequest {
                        system: system.clone(),
                        user: format!("{user}\n\n{INSTANT_GUIDANCE}"),
                        image_jpeg: image.clone(),
                        model: fast_model,
                        effort: CopilotEffort::Low,
                        web_search: false,
                        max_output_tokens: 1024,
                        cache_key: format!("{session_key}-instant"),
                    };
                    let providers = Arc::clone(&providers);
                    let gate = Arc::clone(&gate);
                    let cancel = Arc::clone(&cancel);
                    let instant_text = Arc::clone(&instant_text);
                    let _ = std::thread::Builder::new()
                        .name("copilot-instant".into())
                        .spawn(move || {
                            let result = providers(&fast_settings).and_then(|provider| {
                                let mut first = true;
                                provider.stream(&request, &cancel, &mut |event| {
                                    if cancel.load(Ordering::Relaxed) {
                                        return;
                                    }
                                    if let StreamEvent::Text(delta) = event {
                                        if first {
                                            first = false;
                                            tracing::info!(
                                                first_text_ms =
                                                    started.elapsed().as_millis() as u64,
                                                "copilot instant draft streaming"
                                            );
                                        }
                                        if let Ok(mut text) = instant_text.lock() {
                                            text.push_str(&delta);
                                        }
                                        gate.send(CopilotEvent::InstantDelta { id, text: delta });
                                    }
                                })
                            });
                            if let Err(error) = result {
                                tracing::warn!(%error, "copilot instant draft failed");
                            }
                            gate.send(CopilotEvent::InstantFinished { id });
                            let _ = instant_done.send(());
                        });
                } else {
                    drop(instant_done);
                }
                // The researched answer continues the draft the user is
                // already reading, so it starts once the draft is written.
                let draft = if instant {
                    let _ = instant_finished.recv_timeout(INSTANT_WAIT);
                    instant_text
                        .lock()
                        .map(|text| text.clone())
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                let has_draft = !draft.trim().is_empty();
                let request = AnswerRequest {
                    system,
                    user: if has_draft {
                        format!("{user}\n\n{}", researched_guidance(&draft))
                    } else {
                        user
                    },
                    image_jpeg: image,
                    model,
                    effort: settings.effort,
                    web_search: settings.web_search,
                    max_output_tokens: 4096,
                    cache_key: session_key,
                };
                let mut text = String::new();
                let result = providers(&settings).and_then(|provider| {
                    let mut streaming = false;
                    provider.stream(&request, &cancel, &mut |event| {
                        if cancel.load(Ordering::Relaxed) {
                            return;
                        }
                        match event {
                            StreamEvent::Text(delta) => {
                                if !streaming {
                                    streaming = true;
                                    tracing::info!(
                                        first_text_ms = started.elapsed().as_millis() as u64,
                                        "copilot answer streaming"
                                    );
                                    gate.send(CopilotEvent::AnswerStatus {
                                        id,
                                        status: AnswerStatus::Streaming,
                                    });
                                }
                                text.push_str(&delta);
                                gate.send(CopilotEvent::AnswerDelta { id, text: delta });
                            }
                            StreamEvent::Searching(query) => {
                                gate.send(CopilotEvent::AnswerStatus {
                                    id,
                                    status: AnswerStatus::Searching(query),
                                })
                            }
                            StreamEvent::Source(source) => {
                                gate.send(CopilotEvent::AnswerSource { id, source });
                            }
                            StreamEvent::Done | StreamEvent::Error(_) => {}
                        }
                    })
                });
                if !cancel.load(Ordering::Relaxed) {
                    let status = match result {
                        // The draft stands on its own if the follow-up fails.
                        Ok(()) if text.trim().is_empty() && !has_draft => {
                            AnswerStatus::Failed("The model returned no answer.".into())
                        }
                        Ok(()) => AnswerStatus::Done,
                        Err(error) if has_draft => {
                            tracing::warn!(%error, "copilot researched follow-up failed");
                            AnswerStatus::Done
                        }
                        Err(error) => {
                            tracing::warn!(%error, "copilot answer failed");
                            AnswerStatus::Failed(error)
                        }
                    };
                    gate.send(CopilotEvent::AnswerStatus { id, status });
                }
                tracing::info!(
                    total_ms = started.elapsed().as_millis() as u64,
                    chars = text.len(),
                    "copilot answer finished"
                );
                if let Ok(instant) = instant_text.lock() {
                    text = combined_answer(&instant, &text);
                }
                if gate.is_discarded() {
                    text.clear();
                }
                let _ = messages.send(EngineMsg::AnswerFinished { id, title, text });
            });
        if let Err(error) = spawned {
            tracing::error!(%error, "could not start copilot answer");
            return None;
        }
        Some((id, gate))
    }

    /// Excerpts from the user's documents and meetings for an answer: the
    /// question itself leads the search, the latest conversation informs it.
    fn knowledge_for(&self, kind: &AnswerKind) -> String {
        let recent = |count: usize| {
            let lines: Vec<&str> = self
                .transcript
                .iter()
                .rev()
                .take(count)
                .map(|line| line.text.as_str())
                .collect();
            lines.into_iter().rev().collect::<Vec<_>>().join(" ")
        };
        let focus = match kind {
            AnswerKind::Typed(question) | AnswerKind::Detected { question, .. } => question.clone(),
            AnswerKind::Quick(_) => recent(2),
        };
        self.knowledge.excerpts(&focus, &recent(6))
    }

    fn tick(&mut self) {
        self.check_knowledge();
        if !self.pending_mine.is_empty() {
            self.resolve_pending_mine(false);
        }
        self.evaluate_turn();
        self.maybe_request_insights();
    }

    fn maybe_request_insights(&mut self) {
        if !self.settings.live_insights || self.insights_in_flight || self.ending {
            return;
        }
        let new_lines = self
            .transcript
            .len()
            .saturating_sub(self.lines_at_last_insights);
        if new_lines < INSIGHTS_MIN_NEW_LINES || self.transcript.len() < 2 {
            return;
        }
        if self
            .last_insights_at
            .is_some_and(|at| at.elapsed() < INSIGHTS_INTERVAL)
        {
            return;
        }
        self.insights_in_flight = true;
        self.last_insights_at = Some(Instant::now());
        self.lines_at_last_insights = self.transcript.len();
        let mut settings = self.settings.clone();
        settings.set_answer_model(self.settings.provider.default_fast_model());
        let request = AnswerRequest {
            system: insights_system_prompt().to_owned(),
            user: insights_user_prompt(&render_transcript(
                &self.transcript,
                INSIGHTS_TRANSCRIPT_CHARS,
            )),
            image_jpeg: None,
            model: settings.answer_model(),
            effort: CopilotEffort::Low,
            web_search: false,
            max_output_tokens: 800,
            cache_key: format!("{}-insights", self.session_key),
        };
        let providers = Arc::clone(&self.services.providers);
        let messages = self.messages.clone();
        let _ = std::thread::Builder::new()
            .name("copilot-insights".into())
            .spawn(move || {
                let cancel = AtomicBool::new(false);
                let result = providers(&settings)
                    .and_then(|provider| complete(provider.as_ref(), &request, &cancel))
                    .map(|text| parse_insights(&text));
                let _ = messages.send(EngineMsg::InsightsFinished(result));
            });
    }

    fn handle_insights(&mut self, result: Result<Option<CopilotInsights>, String>) {
        self.insights_in_flight = false;
        let insights = match result {
            Ok(Some(insights)) => insights,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(%error, "copilot insights failed");
                return;
            }
        };
        // A detected question is offered as a click-to-answer card; only the
        // other side's own pause-confirmed questions are answered unprompted.
        let before = self.insights.situation;
        self.insights = insights.clone();
        (self.ui)(CopilotEvent::Insights(insights));
        let after = self.insights.situation;
        if before.user_takes_part() && !after.user_takes_part() {
            tracing::info!(situation = ?after, "automatic answers paused");
            self.drop_turn();
        } else if !before.user_takes_part() && after.user_takes_part() {
            tracing::info!(situation = ?after, "automatic answers resumed");
        }
    }

    /// Ends the session: drains audio and transcription, then writes notes.
    fn finish(mut self, incoming: &Receiver<EngineMsg>) {
        self.ending = true;
        self.cancel_answer();
        self.stop_audio();
        let deadline = Instant::now() + Duration::from_secs(6);
        let mut close_requested = false;
        loop {
            // Flushed utterances arrive as audio messages; transcripts follow.
            match incoming.recv_timeout(Duration::from_millis(100)) {
                Ok(EngineMsg::Audio(AudioMsg::Utterance { speaker, utterance })) => {
                    self.queue_utterance(speaker, utterance);
                }
                Ok(EngineMsg::Transcribed {
                    speaker,
                    start_ms,
                    end_ms,
                    text,
                }) => self.handle_transcribed(speaker, start_ms, end_ms, text),
                Ok(EngineMsg::AnswerFinished { id, title, text }) => {
                    self.handle_answer_finished(id, title, text);
                }
                // Ending again while notes are written skips the wrap-up.
                Ok(EngineMsg::Action(CopilotAction::End)) => close_requested = true,
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if self.in_flight.load(Ordering::Acquire) == 0 || Instant::now() >= deadline {
                break;
            }
        }
        self.resolve_pending_mine(true);
        drop(self.jobs.take());
        if !self.settings.save_notes || self.transcript.is_empty() {
            (self.ui)(CopilotEvent::Exit);
            return;
        }
        (self.ui)(CopilotEvent::Ending);
        let summary = self.summarize();
        match self.write_notes(summary.as_deref()) {
            Ok(path) => {
                tracing::info!(path = %path.display(), "meeting notes saved");
                let (summary, follow_up) = summary
                    .as_deref()
                    .map(split_follow_up)
                    .map_or((None, None), |(notes, email)| {
                        ((!notes.is_empty()).then_some(notes), email)
                    });
                (self.ui)(CopilotEvent::NotesSaved {
                    path: path.to_string_lossy().into_owned(),
                    summary,
                    follow_up,
                });
                self.notes_path = Some(path);
                if !close_requested {
                    self.wrap_up(incoming);
                }
            }
            Err(error) => tracing::error!(%error, "could not save meeting notes"),
        }
        (self.ui)(CopilotEvent::Exit);
    }

    /// Keeps the wrap-up open for copying the follow-up and opening the
    /// notes, until the user is done with it.
    fn wrap_up(&mut self, incoming: &Receiver<EngineMsg>) {
        let deadline = Instant::now() + WRAP_UP_TIMEOUT;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match incoming.recv_timeout(left) {
                Ok(EngineMsg::Action(CopilotAction::End)) => return,
                Ok(EngineMsg::Action(
                    action @ (CopilotAction::CopyText(_)
                    | CopilotAction::OpenNotes
                    | CopilotAction::OpenNotesFolder
                    | CopilotAction::OpenUrl(_)),
                )) => {
                    self.handle_action(action);
                }
                Ok(EngineMsg::ShowRequested | EngineMsg::Hotkey(_)) => {
                    (self.ui)(CopilotEvent::Show);
                }
                Ok(_) => {}
                Err(_) => return,
            }
        }
    }

    fn summarize(&self) -> Option<String> {
        if self.transcript.len() < 3 {
            return None;
        }
        let mut settings = self.settings.clone();
        settings.set_answer_model(self.settings.fast_model());
        let request = AnswerRequest {
            system: notes_system_prompt().to_owned(),
            user: format!(
                "<transcript>\n{}\n</transcript>",
                render_transcript(&self.transcript, 60_000)
            ),
            image_jpeg: None,
            model: settings.answer_model(),
            effort: CopilotEffort::Low,
            web_search: false,
            max_output_tokens: 2048,
            cache_key: format!("{}-notes", self.session_key),
        };
        let providers = Arc::clone(&self.services.providers);
        let (sender, receiver) = mpsc::channel();
        let _ = std::thread::Builder::new()
            .name("copilot-notes".into())
            .spawn(move || {
                let cancel = AtomicBool::new(false);
                let result = providers(&settings)
                    .and_then(|provider| complete(provider.as_ref(), &request, &cancel));
                let _ = sender.send(result);
            });
        match receiver.recv_timeout(Duration::from_secs(30)) {
            Ok(Ok(summary)) if !summary.trim().is_empty() => Some(summary),
            Ok(Err(error)) => {
                tracing::warn!(%error, "meeting summary failed");
                None
            }
            _ => None,
        }
    }

    fn write_notes(&self, summary: Option<&str>) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(&self.services.notes_dir)?;
        let started = chrono::Local::now()
            - chrono::Duration::milliseconds(self.started.elapsed().as_millis() as i64);
        let topic = self.insights.topic.trim();
        let title = if topic.is_empty() {
            "Meeting notes".to_owned()
        } else {
            topic.to_owned()
        };
        let slug: String = title
            .chars()
            .map(|character| {
                if character.is_alphanumeric() || character == ' ' || character == '-' {
                    character
                } else {
                    ' '
                }
            })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let name = format!(
            "{} {}.md",
            started.format("%Y-%m-%d %H%M"),
            slug.chars().take(60).collect::<String>()
        );
        let path = self.services.notes_dir.join(name);
        let markdown = meeting_notes_markdown(
            &title,
            &started.format("%Y-%m-%d %H:%M").to_string(),
            self.elapsed_ms(),
            summary,
            &self.transcript,
            &self.answers,
        );
        std::fs::write(&path, markdown)?;
        Ok(path)
    }
}

enum AnswerKind {
    Quick(QuickAction),
    Typed(String),
    Detected { question: String, automatic: bool },
}

/// Speech models sometimes invent stock phrases from noise or silence.
fn is_noise_transcript(text: &str, duration_ms: u64) -> bool {
    let normalized: String = text
        .to_lowercase()
        .chars()
        .filter(|character| character.is_alphanumeric() || character.is_whitespace())
        .collect();
    let normalized = normalized.trim();
    if normalized.is_empty() {
        return true;
    }
    const STOCK: [&str; 8] = [
        "thank you",
        "thanks for watching",
        "thank you for watching",
        "you",
        "bye",
        "uh",
        "um",
        "subtitles by the amaraorg community",
    ];
    duration_ms < 2_500 && STOCK.contains(&normalized)
}

#[cfg(test)]
mod tests {
    use agentdictate_core::Situation;

    use super::*;

    struct FakeAudio;
    impl AudioControl for FakeAudio {
        fn set_paused(&self, _paused: bool) {}
        fn stop(self: Box<Self>) {}
    }

    struct FakePlatform;
    impl Platform for FakePlatform {
        fn start_audio(
            &self,
            _microphone: bool,
            _system_audio: bool,
            _sink: AudioSink,
        ) -> Result<Box<dyn AudioControl>, String> {
            Ok(Box::new(FakeAudio))
        }
        fn capture_screen(&self) -> Result<Vec<u8>, String> {
            Ok(vec![0xff, 0xd8])
        }
        fn copy_text(&self, _text: &str) {}
        fn open(&self, _target: &str) {}
    }

    struct ScriptedProvider {
        requests: Arc<Mutex<Vec<AnswerRequest>>>,
    }
    impl AnswerProvider for ScriptedProvider {
        fn stream(
            &self,
            request: &AnswerRequest,
            _cancel: &AtomicBool,
            sink: &mut dyn FnMut(StreamEvent),
        ) -> Result<(), String> {
            self.requests.lock().unwrap().push(request.clone());
            if request.system.contains("monitor a live meeting") {
                sink(StreamEvent::Text(
                    r#"{"topic":"Pricing","summary":["Discussing plans"],"question":null,"suggestions":["Suggest a reply"]}"#.into(),
                ));
            } else if request.system == notes_system_prompt() {
                sink(StreamEvent::Text(
                    "Agreed on a pilot.\n\n**Follow-up email**\nHi Sam,\n\nThanks for today."
                        .into(),
                ));
            } else if request.system == turn_judge_system_prompt() {
                // Tag questions in these fixtures are rhetorical.
                let directed = !request.user.contains("isn't it");
                sink(StreamEvent::Text(format!(r#"{{"answer": {directed}}}"#)));
            } else {
                sink(StreamEvent::Searching("pricing".into()));
                sink(StreamEvent::Text("Say: **$20** per seat.".into()));
            }
            sink(StreamEvent::Done);
            Ok(())
        }
    }

    type Harness = (
        Engine,
        Receiver<EngineMsg>,
        Arc<Mutex<Vec<CopilotEvent>>>,
        Arc<Mutex<Vec<AnswerRequest>>>,
        tempfile::TempDir,
    );

    fn harness(settings: CopilotSettings) -> Harness {
        let directory = tempfile::tempdir().unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider_requests = Arc::clone(&requests);
        let ui_events = Arc::clone(&events);
        let (messages, incoming) = mpsc::channel();
        let services = Services {
            platform: Arc::new(FakePlatform),
            providers: Arc::new(move |_| {
                Ok(Box::new(ScriptedProvider {
                    requests: Arc::clone(&provider_requests),
                }) as Box<dyn AnswerProvider>)
            }),
            transcriber: Arc::new(|wav, _| Ok(format!("{} bytes", wav.len()))),
            models: Arc::new(|provider| provider.suggested_models()),
            settings_path: directory.path().join("copilot.json"),
            notes_dir: directory.path().join("meetings"),
            knowledge_dir: directory.path().join("knowledge"),
            language: String::new(),
        };
        let engine = Engine::new(
            services,
            settings,
            Arc::new(move |event| ui_events.lock().unwrap().push(event)),
            messages,
        );
        (engine, incoming, events, requests, directory)
    }

    fn drain(engine: &mut Engine, incoming: &Receiver<EngineMsg>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match incoming.recv_timeout(Duration::from_millis(50)) {
                Ok(message) => {
                    engine.handle(message);
                }
                Err(_)
                    if engine.active_answer.is_none()
                        && !engine.insights_in_flight
                        && engine.pending_turn.is_none() =>
                {
                    return;
                }
                Err(_) => engine.evaluate_turn(),
            }
        }
    }

    fn wait_for_requests(requests: &Arc<Mutex<Vec<AnswerRequest>>>, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while requests.lock().unwrap().len() < count && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Places the session clock well past the fixture utterances, as if the
    /// other side had already been silent for a while.
    fn after_a_pause(engine: &mut Engine) {
        engine.started = Instant::now() - Duration::from_secs(30);
    }

    /// Feeds the capture thread's messages for a finished system-audio
    /// utterance, checking pending work between them as the engine loop does.
    fn them_finish_speaking(engine: &mut Engine, seconds: u64) {
        let utterance = Utterance {
            samples: vec![0; (seconds * 16_000) as usize],
            start_sample: 0,
            voiced_blocks: (seconds * 50) as usize,
            peak_rms: 0.2,
            lead_ms: 0,
            tail_ms: 0,
            released_after_ms: 0,
        };
        for message in AudioMsg::from_segment(Speaker::Them, SegmentEvent::Utterance(utterance)) {
            engine.handle_audio(message);
            engine.tick();
        }
    }

    fn answers_started(events: &Arc<Mutex<Vec<CopilotEvent>>>) -> usize {
        events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| matches!(event, CopilotEvent::AnswerStarted { .. }))
            .count()
    }

    #[test]
    fn question_from_the_other_side_triggers_an_automatic_streamed_answer() {
        let (mut engine, incoming, events, requests, _directory) =
            harness(CopilotSettings::default());
        after_a_pause(&mut engine);
        engine.handle_transcribed(
            Speaker::Them,
            1_000,
            3_000,
            Ok("What does pricing look like for a team of ten?".into()),
        );
        drain(&mut engine, &incoming);
        let events = events.lock().unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            CopilotEvent::AnswerStarted {
                automatic: true,
                used_screen: false,
                ..
            }
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            CopilotEvent::AnswerDelta { text, .. } if text.contains("$20")
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            CopilotEvent::AnswerStatus { status: AnswerStatus::Searching(query), .. } if query == "pricing"
        )));
        let requests = requests.lock().unwrap();
        let judge = requests
            .iter()
            .find(|request| request.system == turn_judge_system_prompt())
            .expect("the question is judged before it is shown");
        assert_eq!(judge.model, "gpt-6-luna");
        assert!(judge.user.contains("Me has not spoken yet."));
        let researched: Vec<_> = requests
            .iter()
            .filter(|request| {
                !request.user.contains(INSTANT_GUIDANCE)
                    && request.system != turn_judge_system_prompt()
            })
            .collect();
        assert!(researched[0].user.contains("They just asked"));
        assert!(
            researched[0]
                .user
                .contains("[0:01] Them: What does pricing look like")
        );
        // The researched answer continues the instant draft.
        assert!(
            researched[0]
                .user
                .contains("<draft>\nSay: **$20** per seat.\n</draft>")
        );
        assert!(researched[0].web_search);
        let instant = requests
            .iter()
            .find(|request| request.user.contains(INSTANT_GUIDANCE))
            .expect("an instant draft request");
        assert_eq!(instant.model, "gpt-6-sol");
        assert!(!instant.web_search);
        assert_eq!(engine.answers.len(), 1);
    }

    #[test]
    fn rhetorical_questions_followed_by_more_speech_are_not_answered() {
        let (mut engine, incoming, events, _requests, _directory) =
            harness(CopilotSettings::default());
        after_a_pause(&mut engine);
        engine.handle_transcribed(
            Speaker::Them,
            1_000,
            3_000,
            Ok("So why does a sales team need a daily standup?".into()),
        );
        // They carry straight on with their own answer.
        engine.them_spans.push_back(ThemSpan {
            start_ms: 3_700,
            end_ms: 6_000,
            text: None,
            voice: None,
        });
        drain(&mut engine, &incoming);
        assert_eq!(answers_started(&events), 0);
        assert!(engine.pending_turn.is_none());
    }

    #[test]
    fn a_rhetorical_question_is_not_answered_as_their_next_sentence_ends() {
        let (mut engine, incoming, events, _requests, _directory) =
            harness(CopilotSettings::default());
        after_a_pause(&mut engine);
        engine.handle_transcribed(
            Speaker::Them,
            1_000,
            3_000,
            Ok("How do these numbers get pulled out of thin air?".into()),
        );
        engine.handle_audio(AudioMsg::Speaking {
            speaker: Speaker::Them,
            active: true,
        });
        let verdict = incoming.recv_timeout(Duration::from_secs(5)).unwrap();
        engine.handle(verdict);
        // The moment their next sentence ends must not look like a pause
        // after the question.
        them_finish_speaking(&mut engine, 3);
        drain(&mut engine, &incoming);
        assert_eq!(answers_started(&events), 0);
    }

    #[test]
    fn questions_wait_while_the_other_side_might_still_be_talking() {
        let (mut engine, incoming, events, _requests, _directory) =
            harness(CopilotSettings::default());
        after_a_pause(&mut engine);
        engine.handle_audio(AudioMsg::Speaking {
            speaker: Speaker::Them,
            active: true,
        });
        engine.handle_transcribed(
            Speaker::Them,
            1_000,
            3_000,
            Ok("What does pricing look like for a team of ten?".into()),
        );
        // The verdict arrives while a sound is still being judged as speech.
        let message = incoming.recv_timeout(Duration::from_secs(5)).unwrap();
        engine.handle(message);
        assert_eq!(answers_started(&events), 0);
        assert!(engine.pending_turn.is_some());
        // The sound was not speech: the question stands and is answered.
        engine.handle_audio(AudioMsg::Speaking {
            speaker: Speaker::Them,
            active: false,
        });
        drain(&mut engine, &incoming);
        assert_eq!(answers_started(&events), 1);
    }

    #[test]
    fn questions_the_judge_rejects_are_discarded_unseen() {
        let (mut engine, incoming, events, requests, _directory) =
            harness(CopilotSettings::default());
        after_a_pause(&mut engine);
        engine.handle_transcribed(
            Speaker::Them,
            1_000,
            3_000,
            Ok("Daily coaching is the whole game, isn't it?".into()),
        );
        drain(&mut engine, &incoming);
        assert_eq!(answers_started(&events), 0);
        // The draft started early, but never reaches the window or the notes.
        assert!(requests.lock().unwrap().len() > 1);
        assert!(events.lock().unwrap().iter().all(|event| !matches!(
            event,
            CopilotEvent::AnswerDelta { .. } | CopilotEvent::InstantDelta { .. }
        )));
        assert!(engine.answers.is_empty());
    }

    #[test]
    fn answers_are_drafted_unseen_and_appear_once_the_judge_agrees() {
        let (mut engine, incoming, events, requests, _directory) =
            harness(CopilotSettings::default());
        after_a_pause(&mut engine);
        engine.handle_transcribed(
            Speaker::Them,
            1_000,
            3_000,
            Ok("What does pricing look like for a team of ten?".into()),
        );
        // The judge and both answer lanes run at once.
        wait_for_requests(&requests, 3);
        assert_eq!(answers_started(&events), 0, "drafted out of sight");
        drain(&mut engine, &incoming);
        assert_eq!(answers_started(&events), 1);
        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            CopilotEvent::AnswerDelta { text, .. } if text.contains("$20")
        )));
        assert_eq!(engine.answers.len(), 1);
    }

    #[test]
    fn requests_without_a_question_mark_are_answered_once_judged() {
        let (mut engine, incoming, events, requests, _directory) =
            harness(CopilotSettings::default());
        after_a_pause(&mut engine);
        engine.handle_transcribed(
            Speaker::Them,
            1_000,
            3_000,
            Ok("I'd love to understand how you handle security reviews.".into()),
        );
        drain(&mut engine, &incoming);
        assert_eq!(answers_started(&events), 1);
        assert_eq!(
            requests.lock().unwrap()[0].system,
            turn_judge_system_prompt()
        );
    }

    #[test]
    fn automatic_answers_pause_while_the_user_watches_a_recording() {
        let (mut engine, incoming, events, requests, _directory) =
            harness(CopilotSettings::default());
        after_a_pause(&mut engine);
        engine.handle_insights(Ok(Some(CopilotInsights {
            topic: "Scaling a service business".into(),
            situation: Situation::Media,
            ..CopilotInsights::default()
        })));
        engine.handle_transcribed(
            Speaker::Them,
            1_000,
            3_000,
            Ok("How do these numbers get pulled out of thin air?".into()),
        );
        drain(&mut engine, &incoming);
        assert_eq!(answers_started(&events), 0);
        assert!(requests.lock().unwrap().is_empty(), "nothing is judged");
        // Back in a conversation, questions are judged again.
        engine.handle_insights(Ok(Some(CopilotInsights {
            situation: Situation::Conversation,
            ..CopilotInsights::default()
        })));
        engine.handle_transcribed(
            Speaker::Them,
            4_000,
            6_000,
            Ok("What does pricing look like for a team of ten?".into()),
        );
        drain(&mut engine, &incoming);
        assert_eq!(answers_started(&events), 1);
    }

    #[test]
    fn detected_insight_questions_are_offered_but_not_answered() {
        let (mut engine, incoming, events, _requests, _directory) =
            harness(CopilotSettings::default());
        engine.handle_insights(Ok(Some(CopilotInsights {
            topic: "Sales cadence".into(),
            summary: vec!["Advice on daily check-ins".into()],
            question: Some("Instead of weekly, meet with them every single day".into()),
            suggestions: Vec::new(),
            situation: Situation::Unclear,
        })));
        drain(&mut engine, &incoming);
        assert_eq!(answers_started(&events), 0);
        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            CopilotEvent::Insights(insights) if insights.question.is_some()
        )));
    }

    #[test]
    fn answers_draw_on_the_users_documents() {
        let (mut engine, incoming, _events, requests, directory) =
            harness(CopilotSettings::default());
        let knowledge = directory.path().join("knowledge");
        std::fs::create_dir_all(&knowledge).unwrap();
        std::fs::write(knowledge.join("pricing.md"), "Growth plan: $49 per seat.").unwrap();
        engine.load_knowledge();
        let loaded = incoming.recv_timeout(Duration::from_secs(5)).unwrap();
        engine.handle(loaded);
        engine.handle_action(CopilotAction::Ask("What do we charge per seat?".into()));
        drain(&mut engine, &incoming);
        let requests = requests.lock().unwrap();
        assert!(!requests.is_empty());
        assert!(requests.iter().all(|request| {
            request
                .user
                .contains("<my_knowledge>\n[pricing.md]\nGrowth plan: $49 per seat.")
        }));
    }

    #[test]
    fn knowledge_summaries_reach_the_panel() {
        let (mut engine, incoming, events, _requests, directory) =
            harness(CopilotSettings::default());
        let knowledge = directory.path().join("knowledge");
        std::fs::create_dir_all(&knowledge).unwrap();
        std::fs::write(knowledge.join("offer.txt"), "Done-for-you outbound.").unwrap();
        std::fs::write(knowledge.join("deck.pdf"), "%PDF").unwrap();
        engine.load_knowledge();
        let loaded = incoming.recv_timeout(Duration::from_secs(5)).unwrap();
        engine.handle(loaded);
        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            CopilotEvent::Knowledge(summary)
                if summary.documents == 1 && summary.unreadable == ["deck.pdf"]
        )));
    }

    #[test]
    fn judging_and_insights_use_the_quickest_model_whatever_drafts_use() {
        let mut settings = CopilotSettings::default();
        settings.set_fast_model("gpt-6-astra");
        let (mut engine, incoming, _events, requests, _directory) = harness(settings);
        after_a_pause(&mut engine);
        engine.handle_transcribed(
            Speaker::Them,
            1_000,
            3_000,
            Ok("What does pricing look like for a team of ten?".into()),
        );
        engine.handle_transcribed(Speaker::Me, 4_000, 5_000, Ok("Happy to explain.".into()));
        engine.tick();
        drain(&mut engine, &incoming);
        let requests = requests.lock().unwrap();
        let model_for = |marker: &dyn Fn(&AnswerRequest) -> bool| {
            requests
                .iter()
                .find(|request| marker(request))
                .map(|request| request.model.clone())
        };
        assert_eq!(
            model_for(&|request| request.system == turn_judge_system_prompt()).as_deref(),
            Some("gpt-6-luna")
        );
        assert_eq!(
            model_for(&|request| request.system.contains("monitor a live meeting")).as_deref(),
            Some("gpt-6-luna")
        );
        assert_eq!(
            model_for(&|request| request.user.contains(INSTANT_GUIDANCE)).as_deref(),
            Some("gpt-6-astra")
        );
    }

    #[test]
    fn ending_writes_notes_and_offers_the_follow_up_email() {
        let (mut engine, incoming, events, _requests, directory) =
            harness(CopilotSettings::default());
        for (index, (speaker, text)) in [
            (Speaker::Them, "Can we start with a pilot?"),
            (Speaker::Me, "Yes, thirty days with ten seats."),
            (Speaker::Them, "Great, send the details."),
        ]
        .into_iter()
        .enumerate()
        {
            engine.publish(TranscriptLine {
                id: index as u64 + 1,
                speaker,
                text: text.into(),
                at_ms: index as u64 * 2_000,
                voice: None,
            });
        }
        // Ending twice skips the wrap-up's wait.
        engine
            .messages
            .send(EngineMsg::Action(CopilotAction::End))
            .unwrap();
        engine.finish(&incoming);
        let events = events.lock().unwrap();
        let saved = events
            .iter()
            .find_map(|event| match event {
                CopilotEvent::NotesSaved {
                    path,
                    summary,
                    follow_up,
                } => Some((path.clone(), summary.clone(), follow_up.clone())),
                _ => None,
            })
            .expect("notes are saved");
        assert!(std::path::Path::new(&saved.0).starts_with(directory.path().join("meetings")));
        assert_eq!(saved.1.as_deref(), Some("Agreed on a pilot."));
        assert_eq!(saved.2.as_deref(), Some("Hi Sam,\n\nThanks for today."));
        assert!(matches!(events.last(), Some(CopilotEvent::Exit)));
        let notes = std::fs::read_to_string(&saved.0).unwrap();
        assert!(notes.contains("Hi Sam,"));
    }

    #[test]
    fn assist_attaches_the_screen_and_typed_questions_use_the_answer_model() {
        let (mut engine, incoming, _events, requests, _directory) =
            harness(CopilotSettings::default());
        engine.handle_action(CopilotAction::Assist);
        drain(&mut engine, &incoming);
        engine.handle_action(CopilotAction::Ask("Who founded Anthropic?".into()));
        drain(&mut engine, &incoming);
        let requests = requests.lock().unwrap();
        let researched: Vec<_> = requests
            .iter()
            .filter(|request| !request.user.contains(INSTANT_GUIDANCE))
            .collect();
        assert_eq!(researched.len(), 2);
        assert!(researched[0].image_jpeg.is_some());
        assert!(
            researched[0]
                .user
                .contains("screenshot of my screen is attached")
        );
        assert!(
            researched[1]
                .user
                .contains("My question: Who founded Anthropic?")
        );
        assert_eq!(researched[1].model, "gpt-6-sol");
        // The instant draft sees the same screenshot.
        assert!(
            requests
                .iter()
                .filter(|request| request.user.contains(INSTANT_GUIDANCE))
                .all(|request| request.image_jpeg.is_some())
        );
    }

    #[test]
    fn instant_drafts_are_skipped_when_disabled() {
        let (mut engine, incoming, events, requests, _directory) = harness(CopilotSettings {
            instant_answers: false,
            ..CopilotSettings::default()
        });
        engine.handle_action(CopilotAction::Quick(QuickAction::Recap));
        drain(&mut engine, &incoming);
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].user.contains("<draft>"));
        assert!(
            !events
                .lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event, CopilotEvent::InstantDelta { .. }))
        );
    }

    #[test]
    fn microphone_echo_of_system_audio_is_dropped() {
        let (mut engine, _incoming, events, _requests, _directory) = harness(CopilotSettings {
            auto_answer: false,
            ..CopilotSettings::default()
        });
        engine.them_spans.push_back(ThemSpan {
            start_ms: 1_000,
            end_ms: 4_000,
            text: None,
            voice: None,
        });
        engine.handle_transcribed(
            Speaker::Me,
            1_200,
            4_100,
            Ok("we will ship the pricing page on monday".into()),
        );
        assert_eq!(engine.pending_mine.len(), 1);
        engine.handle_transcribed(
            Speaker::Them,
            1_000,
            4_000,
            Ok("We will ship the pricing page on Monday.".into()),
        );
        assert!(engine.pending_mine.is_empty());
        let lines: Vec<_> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                CopilotEvent::Transcript(line) => Some(line.speaker),
                _ => None,
            })
            .collect();
        assert_eq!(lines, vec![Speaker::Them]);
    }

    #[test]
    fn microphone_lines_wait_while_the_other_side_is_still_speaking() {
        let (mut engine, _incoming, events, _requests, _directory) = harness(CopilotSettings {
            auto_answer: false,
            ..CopilotSettings::default()
        });
        engine.handle_audio(AudioMsg::Speaking {
            speaker: Speaker::Them,
            active: true,
        });
        engine.handle_transcribed(
            Speaker::Me,
            1_000,
            3_000,
            Ok("the quarterly numbers look strong this time".into()),
        );
        assert_eq!(engine.pending_mine.len(), 1);
        engine.handle_audio(AudioMsg::Speaking {
            speaker: Speaker::Them,
            active: false,
        });
        engine.them_spans.push_back(ThemSpan {
            start_ms: 500,
            end_ms: 6_000,
            text: None,
            voice: None,
        });
        engine.handle_transcribed(
            Speaker::Them,
            500,
            6_000,
            Ok("I think the quarterly numbers look strong this time around.".into()),
        );
        assert!(engine.pending_mine.is_empty());
        let mine = events.lock().unwrap().iter().any(
            |event| matches!(event, CopilotEvent::Transcript(line) if line.speaker == Speaker::Me),
        );
        assert!(!mine, "echo of the other side must not appear as the user");
    }

    #[test]
    fn echo_fragments_spanning_two_utterances_are_dropped() {
        let (mut engine, _incoming, events, _requests, _directory) = harness(CopilotSettings {
            auto_answer: false,
            ..CopilotSettings::default()
        });
        for (start_ms, end_ms, text) in [
            (1_000, 3_000, "The thing about pricing is"),
            (
                3_400,
                6_000,
                "that most founders get it wrong every single time.",
            ),
        ] {
            engine.them_spans.push_back(ThemSpan {
                start_ms,
                end_ms,
                text: None,
                voice: None,
            });
            engine.handle_transcribed(Speaker::Them, start_ms, end_ms, Ok(text.into()));
        }
        // Straddles both lines, and a fragment of the second one.
        engine.handle_transcribed(
            Speaker::Me,
            2_400,
            4_200,
            Ok("pricing is that most founders".into()),
        );
        engine.handle_transcribed(
            Speaker::Me,
            4_800,
            5_900,
            Ok("get it wrong every time".into()),
        );
        assert!(engine.pending_mine.is_empty());
        let mine = events.lock().unwrap().iter().any(
            |event| matches!(event, CopilotEvent::Transcript(line) if line.speaker == Speaker::Me),
        );
        assert!(!mine, "speaker echo must not appear as the user");
    }

    #[test]
    fn echo_heard_mid_sentence_waits_for_the_sentence_to_be_transcribed() {
        let (mut engine, _incoming, events, _requests, _directory) = harness(CopilotSettings {
            auto_answer: false,
            ..CopilotSettings::default()
        });
        after_a_pause(&mut engine);
        engine.handle_audio(AudioMsg::Speaking {
            speaker: Speaker::Them,
            active: true,
        });
        let now = engine.elapsed_ms();
        engine.handle_transcribed(
            Speaker::Me,
            now - 2_500,
            now - 1_500,
            Ok("And we want full".into()),
        );
        them_finish_speaking(&mut engine, 4);
        let mine = |events: &Arc<Mutex<Vec<CopilotEvent>>>| {
            events.lock().unwrap().iter().any(
                |event| matches!(event, CopilotEvent::Transcript(line) if line.speaker == Speaker::Me),
            )
        };
        assert!(!mine(&events), "held until their sentence is known");
        assert_eq!(engine.pending_mine.len(), 1);
        let span = engine.them_spans.back().unwrap();
        let (start_ms, end_ms) = (span.start_ms, span.end_ms);
        engine.handle_transcribed(
            Speaker::Them,
            start_ms,
            end_ms,
            Ok("We're doing a 1,000-person event in Vegas, and we want full AV.".into()),
        );
        assert!(engine.pending_mine.is_empty());
        assert!(!mine(&events), "speaker echo must not appear as the user");
    }

    #[test]
    fn replies_that_repeat_the_question_are_kept() {
        let (mut engine, _incoming, events, _requests, _directory) = harness(CopilotSettings {
            auto_answer: false,
            ..CopilotSettings::default()
        });
        engine.them_spans.push_back(ThemSpan {
            start_ms: 1_000,
            end_ms: 3_000,
            text: None,
            voice: None,
        });
        engine.handle_transcribed(
            Speaker::Them,
            1_000,
            3_000,
            Ok("What does pricing look like for a team of ten?".into()),
        );
        // A quick reply, starting 0.6 s after they finished.
        engine.handle_transcribed(
            Speaker::Me,
            3_600,
            5_000,
            Ok("Pricing for a team of ten, sure.".into()),
        );
        assert!(events.lock().unwrap().iter().any(
            |event| matches!(event, CopilotEvent::Transcript(line) if line.speaker == Speaker::Me),
        ));
    }

    #[test]
    fn the_same_call_heard_on_two_devices_appears_once() {
        let (mut engine, _incoming, events, _requests, _directory) = harness(CopilotSettings {
            auto_answer: false,
            ..CopilotSettings::default()
        });
        engine.handle_transcribed(
            Speaker::Them,
            1_000,
            3_000,
            Ok("Let's review the launch checklist together.".into()),
        );
        engine.handle_transcribed(
            Speaker::Them,
            1_200,
            3_100,
            Ok("Let's review the launch checklist together".into()),
        );
        let lines = events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| matches!(event, CopilotEvent::Transcript(_)))
            .count();
        assert_eq!(lines, 1);
    }

    #[test]
    fn genuine_overlapping_speech_is_kept_after_the_echo_wait() {
        let (mut engine, _incoming, events, _requests, _directory) =
            harness(CopilotSettings::default());
        engine.them_spans.push_back(ThemSpan {
            start_ms: 0,
            end_ms: 3_000,
            text: Some("The budget is tight this quarter.".into()),
            voice: None,
        });
        engine.handle_transcribed(
            Speaker::Me,
            500,
            2_500,
            Ok("Understood, let me check.".into()),
        );
        assert!(engine.pending_mine.is_empty());
        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            CopilotEvent::Transcript(line) if line.speaker == Speaker::Me
        )));
    }

    #[test]
    fn insights_are_requested_with_the_fast_model_and_forwarded() {
        let (mut engine, incoming, events, requests, _directory) = harness(CopilotSettings {
            auto_answer: false,
            ..CopilotSettings::default()
        });
        engine.handle_transcribed(
            Speaker::Them,
            0,
            1_000,
            Ok("Let's talk pricing today.".into()),
        );
        engine.handle_transcribed(Speaker::Me, 2_000, 3_000, Ok("Sounds good to me.".into()));
        engine.tick();
        drain(&mut engine, &incoming);
        assert_eq!(requests.lock().unwrap()[0].model, "gpt-6-luna");
        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            CopilotEvent::Insights(insights) if insights.topic == "Pricing"
        )));
    }

    #[test]
    fn settings_updates_are_saved_and_echoed() {
        let (mut engine, _incoming, events, _requests, directory) =
            harness(CopilotSettings::default());
        let settings = CopilotSettings {
            provider: CopilotProvider::ClaudeCode,
            ..CopilotSettings::default()
        };
        engine.handle_action(CopilotAction::UpdateSettings(Box::new(settings.clone())));
        assert_eq!(
            super::super::store::load(&directory.path().join("copilot.json")),
            settings
        );
        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            CopilotEvent::Settings(saved) if saved.provider == CopilotProvider::ClaudeCode
        )));
    }

    #[test]
    fn notes_are_written_with_transcript_and_answers() {
        let (mut engine, _incoming, _events, _requests, directory) =
            harness(CopilotSettings::default());
        engine.publish(TranscriptLine {
            id: 1,
            speaker: Speaker::Them,
            text: "Hello there".into(),
            at_ms: 0,
            voice: None,
        });
        engine.answers.push(("Q".into(), "A".into()));
        let path = engine.write_notes(Some("Summary text")).unwrap();
        assert!(path.starts_with(directory.path().join("meetings")));
        let notes = std::fs::read_to_string(path).unwrap();
        assert!(notes.contains("Summary text"));
        assert!(notes.contains("Hello there"));
    }

    #[test]
    fn stock_noise_phrases_are_filtered_only_for_short_audio() {
        assert!(is_noise_transcript("Thank you.", 900));
        assert!(!is_noise_transcript("Thank you.", 4_000));
        assert!(!is_noise_transcript("Thank you for the update", 900));
        assert!(is_noise_transcript("  ", 10_000));
    }
}
