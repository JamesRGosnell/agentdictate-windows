//! The meeting copilot session: audio in, transcript and answers out.
//!
//! One engine thread owns all session state. Audio processing, transcription
//! workers, and answer streams run on their own threads and report back
//! through [`EngineMsg`]; streamed answer text goes straight to the UI so a
//! busy engine never delays the words the user is waiting for.

use std::{
    collections::VecDeque,
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
    CopilotSettings, ECHO_SIMILARITY, INSTANT_GUIDANCE, ListenState, ModelChoice, QuickAction,
    RESEARCHED_GUIDANCE, Speaker, TranscriptLine, answer_system_prompt, answer_user_prompt,
    detected_question_instruction, insights_system_prompt, insights_user_prompt,
    looks_like_question, meeting_notes_markdown, notes_system_prompt, parse_insights,
    render_transcript, typed_question_instruction, utterance_similarity,
};

use super::{
    providers::{AnswerProvider, AnswerRequest, complete},
    stream::StreamEvent,
    vad::{Utterance, wav_bytes},
};

const TRANSCRIPTION_WORKERS: usize = 3;
const ANSWER_TRANSCRIPT_CHARS: usize = 24_000;
const INSIGHTS_TRANSCRIPT_CHARS: usize = 8_000;
const INSIGHTS_INTERVAL: Duration = Duration::from_secs(12);
const AUTO_ANSWER_COOLDOWN: Duration = Duration::from_secs(6);
const ECHO_WAIT: Duration = Duration::from_secs(4);

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
    Hotkey(HotkeyCommand),
    ShowRequested,
    ModelsLoaded(CopilotProvider, Vec<ModelChoice>),
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
    pending_mine: Vec<PendingMine>,
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
            pending_mine: Vec::new(),
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
                if self
                    .active_answer
                    .as_ref()
                    .is_some_and(|active| active.id == id)
                {
                    self.active_answer = None;
                }
                if !text.trim().is_empty() {
                    self.answers.push((title, text));
                }
            }
            EngineMsg::InsightsFinished(result) => self.handle_insights(result),
            EngineMsg::Hotkey(HotkeyCommand::Assist) => {
                self.answer(AnswerKind::Quick(QuickAction::WhatToSay))
            }
            EngineMsg::Hotkey(HotkeyCommand::Toggle) => (self.ui)(CopilotEvent::ToggleVisibility),
            EngineMsg::Hotkey(HotkeyCommand::Ask) => {
                (self.ui)(CopilotEvent::Show);
                (self.ui)(CopilotEvent::FocusAsk);
            }
            EngineMsg::ShowRequested => (self.ui)(CopilotEvent::Show),
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
                (self.ui)(CopilotEvent::Speaking { speaker, active });
            }
            AudioMsg::Utterance { speaker, utterance } => self.queue_utterance(speaker, utterance),
            AudioMsg::Problem(problem) => {
                tracing::warn!(%problem, "copilot audio problem");
                (self.ui)(CopilotEvent::Notice(problem));
            }
        }
    }

    fn queue_utterance(&mut self, speaker: Speaker, utterance: Utterance) {
        let end_ms = self.elapsed_ms();
        let start_ms = end_ms.saturating_sub(utterance.duration_ms());
        if speaker == Speaker::Them {
            self.them_spans.push_back(ThemSpan {
                start_ms,
                end_ms,
                text: None,
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
        let line = TranscriptLine {
            id: self.next_line,
            speaker,
            text,
            at_ms: start_ms,
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
                }
                let question = looks_like_question(&line.text).then(|| line.text.clone());
                self.publish(line);
                self.resolve_pending_mine(false);
                if let Some(question) = question {
                    self.maybe_auto_answer(question);
                }
            }
            Speaker::Me => {
                let overlapping = self.overlapping_them(start_ms, end_ms);
                if overlapping.is_empty() {
                    self.publish(line);
                } else {
                    self.pending_mine.push(PendingMine {
                        line,
                        end_ms,
                        deadline: Instant::now() + ECHO_WAIT,
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

    /// Indices of system-audio spans that overlap a microphone utterance.
    fn overlapping_them(&self, start_ms: u64, end_ms: u64) -> Vec<usize> {
        let duration = end_ms.saturating_sub(start_ms).max(1);
        self.them_spans
            .iter()
            .enumerate()
            .filter(|(_, span)| {
                let overlap = end_ms
                    .min(span.end_ms)
                    .saturating_sub(start_ms.max(span.start_ms));
                overlap * 10 >= duration * 3
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
            let overlapping = self.overlapping_them(start_ms, item.end_ms);
            let all_known = overlapping
                .iter()
                .all(|index| self.them_spans[*index].text.is_some());
            if !all_known && !force && now < item.deadline {
                self.pending_mine.push(item);
                continue;
            }
            let echo = overlapping.iter().any(|index| {
                self.them_spans[*index].text.as_deref().is_some_and(|them| {
                    utterance_similarity(&item.line.text, them) >= ECHO_SIMILARITY
                })
            });
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

    fn maybe_auto_answer(&mut self, question: String) {
        if !self.settings.auto_answer || self.ending {
            return;
        }
        if let Some((previous, at)) = &self.last_auto_question
            && (at.elapsed() < AUTO_ANSWER_COOLDOWN
                || utterance_similarity(previous, &question) >= 0.6)
        {
            return;
        }
        if self
            .active_answer
            .as_ref()
            .is_some_and(|active| !active.automatic)
        {
            return;
        }
        self.last_auto_question = Some((question.clone(), Instant::now()));
        self.answer(AnswerKind::Detected {
            question,
            automatic: true,
        });
    }

    fn cancel_answer(&mut self) {
        if let Some(active) = self.active_answer.take() {
            active.cancel.store(true, Ordering::Relaxed);
            (self.ui)(CopilotEvent::AnswerStatus {
                id: active.id,
                status: AnswerStatus::Done,
            });
        }
    }

    fn answer(&mut self, kind: AnswerKind) {
        if self.ending {
            return;
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
        self.active_answer = Some(ActiveAnswer {
            id,
            automatic,
            cancel: Arc::clone(&cancel),
        });
        (self.ui)(CopilotEvent::AnswerStarted {
            id,
            title: title.clone(),
            used_screen: use_screen,
            model: model.clone(),
            automatic,
        });
        let transcript = render_transcript(&self.transcript, ANSWER_TRANSCRIPT_CHARS);
        let settings = self.settings.clone();
        let fast_model = settings.fast_model();
        let instant = settings.instant_answers && fast_model != model;
        let providers = Arc::clone(&self.services.providers);
        let platform = Arc::clone(&self.services.platform);
        let ui = Arc::clone(&self.ui);
        let messages = self.messages.clone();
        let session_key = self.session_key.clone();
        let spawned = std::thread::Builder::new()
            .name("copilot-answer".into())
            .spawn(move || {
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
                let user = answer_user_prompt(&transcript, &instruction, image.is_some());
                let instant_text = Arc::new(Mutex::new(String::new()));
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
                    let ui = Arc::clone(&ui);
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
                                        ui(CopilotEvent::InstantDelta { id, text: delta });
                                    }
                                })
                            });
                            if let Err(error) = result {
                                tracing::warn!(%error, "copilot instant draft failed");
                            }
                            ui(CopilotEvent::InstantFinished { id });
                        });
                }
                let request = AnswerRequest {
                    system,
                    user: if instant {
                        format!("{user}\n\n{RESEARCHED_GUIDANCE}")
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
                                    ui(CopilotEvent::AnswerStatus {
                                        id,
                                        status: AnswerStatus::Streaming,
                                    });
                                }
                                text.push_str(&delta);
                                ui(CopilotEvent::AnswerDelta { id, text: delta });
                            }
                            StreamEvent::Searching(query) => ui(CopilotEvent::AnswerStatus {
                                id,
                                status: AnswerStatus::Searching(query),
                            }),
                            StreamEvent::Source(source) => {
                                ui(CopilotEvent::AnswerSource { id, source });
                            }
                            StreamEvent::Done | StreamEvent::Error(_) => {}
                        }
                    })
                });
                if !cancel.load(Ordering::Relaxed) {
                    let status = match result {
                        Ok(()) if text.trim().is_empty() => {
                            AnswerStatus::Failed("The model returned no answer.".into())
                        }
                        Ok(()) => AnswerStatus::Done,
                        Err(error) => {
                            tracing::warn!(%error, "copilot answer failed");
                            AnswerStatus::Failed(error)
                        }
                    };
                    ui(CopilotEvent::AnswerStatus { id, status });
                }
                tracing::info!(
                    total_ms = started.elapsed().as_millis() as u64,
                    chars = text.len(),
                    "copilot answer finished"
                );
                if text.trim().is_empty()
                    && let Ok(instant) = instant_text.lock()
                {
                    text = instant.clone();
                }
                let _ = messages.send(EngineMsg::AnswerFinished { id, title, text });
            });
        if let Err(error) = spawned {
            tracing::error!(%error, "could not start copilot answer");
        }
    }

    fn tick(&mut self) {
        if !self.pending_mine.is_empty() {
            self.resolve_pending_mine(false);
        }
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
        if new_lines == 0 || self.transcript.len() < 2 {
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
        settings.set_answer_model(self.settings.fast_model());
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
        let question = insights.question.clone();
        self.insights = insights.clone();
        (self.ui)(CopilotEvent::Insights(insights));
        if let Some(question) = question {
            let already_answered = self
                .last_auto_question
                .as_ref()
                .is_some_and(|(previous, _)| utterance_similarity(previous, &question) >= 0.5);
            if !already_answered {
                self.maybe_auto_answer(question);
            }
        }
    }

    /// Ends the session: drains audio and transcription, then writes notes.
    fn finish(mut self, incoming: &Receiver<EngineMsg>) {
        self.ending = true;
        self.cancel_answer();
        self.stop_audio();
        let deadline = Instant::now() + Duration::from_secs(6);
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
                Ok(EngineMsg::AnswerFinished { title, text, .. }) if !text.trim().is_empty() => {
                    self.answers.push((title, text));
                }
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
                (self.ui)(CopilotEvent::NotesSaved(
                    path.to_string_lossy().into_owned(),
                ));
                std::thread::sleep(Duration::from_millis(1_800));
            }
            Err(error) => tracing::error!(%error, "could not save meeting notes"),
        }
        (self.ui)(CopilotEvent::Exit);
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
                Err(_) if engine.active_answer.is_none() && !engine.insights_in_flight => return,
                Err(_) => {}
            }
        }
    }

    #[test]
    fn question_from_the_other_side_triggers_an_automatic_streamed_answer() {
        let (mut engine, incoming, events, requests, _directory) =
            harness(CopilotSettings::default());
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
        let researched: Vec<_> = requests
            .iter()
            .filter(|request| !request.user.contains(INSTANT_GUIDANCE))
            .collect();
        assert!(researched[0].user.contains("They just asked"));
        assert!(
            researched[0]
                .user
                .contains("[0:01] Them: What does pricing look like")
        );
        assert!(researched[0].user.contains(RESEARCHED_GUIDANCE));
        assert!(researched[0].web_search);
        let instant = requests
            .iter()
            .find(|request| request.user.contains(INSTANT_GUIDANCE))
            .expect("an instant draft request");
        assert_eq!(instant.model, "gpt-6-luna");
        assert!(!instant.web_search);
        assert_eq!(engine.answers.len(), 1);
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
        assert!(!requests[0].user.contains(RESEARCHED_GUIDANCE));
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
    fn genuine_overlapping_speech_is_kept_after_the_echo_wait() {
        let (mut engine, _incoming, events, _requests, _directory) =
            harness(CopilotSettings::default());
        engine.them_spans.push_back(ThemSpan {
            start_ms: 0,
            end_ms: 3_000,
            text: Some("The budget is tight this quarter.".into()),
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
