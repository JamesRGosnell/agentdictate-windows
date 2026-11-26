//! Meeting copilot: a live, capture-invisible assistant for calls.
//!
//! The copilot runs as its own process (`agentdictated --copilot`) so a
//! meeting session never interferes with dictation. It listens to the
//! microphone and system audio, transcribes both sides, answers questions as
//! they are asked, and writes meeting notes when the session ends.

// The session runtime is Windows-only; shared logic stays testable elsewhere.
#![cfg_attr(not(all(windows, feature = "desktop")), allow(dead_code))]

#[cfg(windows)]
mod answer_eval;
mod ask;
mod assembly;
mod brain;
mod calendar;
#[cfg(windows)]
mod check;
mod engine;
mod journal;
mod knowledge;
mod live;
mod meeting;
mod names;
mod people;
mod providers;
#[cfg(windows)]
mod question_eval;
#[cfg(windows)]
mod replay_check;
mod reveal;
#[cfg(windows)]
mod score;
mod store;
mod stream;
mod vad;
mod voices;
#[cfg(windows)]
mod win;

pub const COPILOT_ARGUMENT: &str = "--copilot";

/// Verify the configured brain through the production reader, without audio,
/// screenshots, a model request, or printing private source text.
#[cfg(windows)]
pub fn check_copilot_brain() -> anyhow::Result<()> {
    let paths = crate::AppPaths::from_environment()?;
    let settings = store::load(&store::settings_path(&paths.config_file));
    anyhow::ensure!(
        settings.brain_source != agentdictate_core::BrainSource::None,
        "Select a brain in copilot settings first."
    );
    let context = brain::context(&settings, "")?;
    println!(
        "{} brain read succeeded ({} context characters; source text withheld).",
        settings.brain_source.label(),
        context.chars().count()
    );
    Ok(())
}

#[must_use]
pub fn is_copilot_argument(argument: Option<&str>) -> bool {
    argument == Some(COPILOT_ARGUMENT)
}

/// Readable label for a model id such as `gpt-6.1-sol` ("GPT-6.1 Sol").
fn model_label(id: &str) -> String {
    let mut parts = id.split('-').peekable();
    let mut label = String::new();
    if let Some(first) = parts.next() {
        label.push_str(&if first.eq_ignore_ascii_case("gpt") {
            "GPT".to_owned()
        } else {
            capitalize(first)
        });
    }
    let mut joined_version = false;
    for part in parts {
        let numeric = part
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_digit());
        if numeric && !joined_version && label == "GPT" {
            label.push('-');
            label.push_str(part);
            joined_version = true;
        } else {
            label.push(' ');
            label.push_str(&if numeric {
                part.to_owned()
            } else {
                capitalize(part)
            });
        }
    }
    label
}

fn capitalize(word: &str) -> String {
    let mut characters = word.chars();
    characters.next().map_or_else(String::new, |first| {
        first.to_uppercase().collect::<String>() + characters.as_str()
    })
}

/// Errors worth retrying: brief rate limits, outages, and dropped
/// connections. A failure that will not clear up during a meeting is not.
fn is_transient_failure(error: &str) -> bool {
    if is_lasting_failure(error) {
        return false;
    }
    let error = error.to_lowercase();
    ["limit", "temporarily unavailable", "could not reach"]
        .iter()
        .any(|pattern| error.contains(pattern))
}

/// Failures that will not clear up during a meeting: a used-up plan or
/// quota, and a sign-in or key the service turned down.
fn is_lasting_failure(error: &str) -> bool {
    let error = error.to_lowercase();
    const LASTING: [&str; 13] = [
        "usage limit",
        "quota",
        "billing",
        "sign-in expired",
        "sign in again",
        "not signed in",
        "sign in with chatgpt",
        "rejected the sign-in",
        "access token",
        "account identifier",
        "not available for this account",
        "unavailable for this account",
        "api key",
    ];
    LASTING.iter().any(|pattern| error.contains(pattern)) || resets_much_later(&error)
}

/// Whether a limit says it resets in hours or days rather than minutes.
fn resets_much_later(error: &str) -> bool {
    let Some((_, after)) = error.split_once("resets in") else {
        return false;
    };
    let after = after.trim_start().trim_start_matches("about").trim_start();
    let amount: u64 = after
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap_or(0);
    let unit = after.trim_start_matches(|character: char| character.is_ascii_digit());
    let unit = unit.trim_start();
    unit.starts_with("day")
        || unit.starts_with("week")
        || unit.starts_with('h')
        || (unit.starts_with("min") && amount > 10)
}

/// Rate-limited utterances in a row after which a route stops waiting out
/// its limit, so speech does not queue behind retries that keep failing.
const LIMITED_IN_A_ROW: usize = 3;

type RouteTranscribe = dyn Fn(&[u8], &str) -> Result<String, String> + Send + Sync;

/// One way to transcribe an utterance, such as the OpenAI API or the
/// ChatGPT sign-in.
struct TranscriptionRoute {
    name: &'static str,
    transcribe: Box<RouteTranscribe>,
    /// Failed in a way that will not clear up during the session.
    down: std::sync::atomic::AtomicBool,
    /// Utterances in a row that failed on a rate limit.
    limited: std::sync::atomic::AtomicUsize,
}

impl TranscriptionRoute {
    fn new(
        name: &'static str,
        transcribe: impl Fn(&[u8], &str) -> Result<String, String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            name,
            transcribe: Box::new(transcribe),
            down: std::sync::atomic::AtomicBool::new(false),
            limited: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Transcribes, riding out brief rate limits and network blips.
    fn attempt(
        &self,
        wav: &[u8],
        language: &str,
        pause: &dyn Fn(std::time::Duration),
    ) -> Result<String, String> {
        use std::sync::atomic::Ordering;
        let retries = if self.limited.load(Ordering::Relaxed) >= LIMITED_IN_A_ROW {
            0
        } else {
            2
        };
        let mut attempt = 0;
        let result = loop {
            match (self.transcribe)(wav, language) {
                Err(error) if attempt < retries && is_transient_failure(&error) => {
                    attempt += 1;
                    tracing::warn!(route = self.name, %error, attempt, "retrying copilot transcription");
                    pause(std::time::Duration::from_millis(1_500 * attempt));
                }
                result => break result,
            }
        };
        match &result {
            Err(error) if error.to_lowercase().contains("limit") => {
                self.limited.fetch_add(1, Ordering::Relaxed);
            }
            _ => self.limited.store(0, Ordering::Relaxed),
        }
        result
    }
}

/// Transcribes on the first route that works, in order of preference. A
/// route that fails for good is passed over for the rest of the session
/// while another is left; with none left, all are tried again.
fn transcribe_on_routes(
    routes: &[TranscriptionRoute],
    wav: &[u8],
    language: &str,
    pause: &dyn Fn(std::time::Duration),
) -> Result<String, String> {
    use std::sync::atomic::Ordering;
    let usable: Vec<&TranscriptionRoute> = routes
        .iter()
        .filter(|route| !route.down.load(Ordering::Relaxed))
        .collect();
    let candidates = if usable.is_empty() {
        routes.iter().collect()
    } else {
        usable
    };
    let mut last = Err("No transcription is set up.".to_owned());
    for (index, route) in candidates.iter().enumerate() {
        match route.attempt(wav, language, pause) {
            Ok(text) => {
                if route.down.swap(false, Ordering::Relaxed) {
                    tracing::info!(route = route.name, "copilot transcription works again");
                }
                return Ok(text);
            }
            Err(error) => {
                let next = candidates.get(index + 1).map(|next| next.name);
                if is_lasting_failure(&error) && !route.down.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        route = route.name,
                        %error,
                        next,
                        "copilot transcription failed for good; passing it over for the rest of the session"
                    );
                } else if let Some(next) = next {
                    tracing::warn!(route = route.name, %error, next, "copilot transcription failed; trying the next route");
                }
                last = Err(error);
            }
        }
    }
    last
}

/// Replays a recorded call through a headless session and reports how soon
/// the transcript and answers arrived; returns the number of problems.
#[cfg(windows)]
pub fn run_copilot_replay(
    path: &std::path::Path,
    upload: bool,
    instant: Option<&str>,
) -> anyhow::Result<usize> {
    replay_check::run(path, upload, instant)
}

/// Plays a recorded conversation through a headless session, with one
/// speaker as the user, and scores every suggestion against what that person
/// really said; returns the number of problems.
#[cfg(windows)]
pub fn run_copilot_score(
    recording: &std::path::Path,
    me: Option<&str>,
    minutes: Option<f64>,
    from: f64,
    documents: Option<&std::path::Path>,
) -> anyhow::Result<usize> {
    score::run(&score::Options {
        recording: recording.to_owned(),
        me: me.map(str::to_owned),
        minutes,
        from,
        documents: documents.map(std::path::Path::to_owned),
    })
}

/// Lets the main window's Settings read and save the copilot's settings.
#[cfg(all(windows, feature = "desktop"))]
pub fn install_copilot_settings_store() -> anyhow::Result<()> {
    let paths = crate::AppPaths::from_environment()?;
    let path = std::sync::Arc::new(store::settings_path(&paths.config_file));
    let load_path = std::sync::Arc::clone(&path);
    let ask_paths = paths.clone();
    agentdictate_ui::set_copilot_settings_store(agentdictate_ui::CopilotSettingsStore {
        load: std::sync::Arc::new(move || store::load(&load_path)),
        save: std::sync::Arc::new(move |settings| {
            store::save(&path, settings).map_err(|error| error.to_string())
        }),
        ask_documents: std::sync::Arc::new(move |question| {
            ask::answer(
                &ask_paths.config_file,
                &ask_paths.knowledge_dir(),
                &ask_paths.meetings_dir(),
                question,
                ask::Source::Documents,
            )
        }),
    });
    Ok(())
}

/// Answers a question from the notes of the user's saved meetings, for the
/// Meetings page.
pub(crate) fn ask_meetings(paths: &crate::AppPaths, question: &str) -> Result<String, String> {
    ask::answer(
        &paths.config_file,
        &paths.knowledge_dir(),
        &paths.meetings_dir(),
        question,
        ask::Source::Meetings,
    )
}

/// The OpenAI API key saved in the copilot's settings beside
/// `config_file`, or empty.
pub(crate) fn saved_openai_key(config_file: &std::path::Path) -> String {
    store::load(&store::settings_path(config_file))
        .openai_api_key
        .trim()
        .to_owned()
}

/// Starts the copilot, or brings the running one forward.
#[cfg(windows)]
pub fn launch_copilot() -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;

    if win::instance::is_running() && win::instance::request_show() {
        return Ok(());
    }
    let executable = std::env::current_exe()?.with_file_name("agentdictated.exe");
    // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
    std::process::Command::new(executable)
        .arg(COPILOT_ARGUMENT)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(0x0000_0008 | 0x0000_0200 | 0x0800_0000)
        .spawn()?;
    Ok(())
}

/// Verifies sign-in, transcription, answers, audio, and screen capture,
/// printing a report. Returns the number of failed checks.
#[cfg(windows)]
pub fn run_copilot_check(speech: Option<&std::path::Path>, latency: bool) -> anyhow::Result<usize> {
    check::run(speech, latency)
}

/// Prints how recordings' voices are told apart, for tuning speaker
/// separation.
#[cfg(windows)]
pub fn run_copilot_voices(paths: &[std::path::PathBuf]) -> anyhow::Result<usize> {
    check::voices(paths)
}

/// Checks that scripted calls name the right people. Returns the number of
/// failed checks.
#[cfg(windows)]
pub fn run_copilot_names() -> anyhow::Result<usize> {
    check::names()
}

/// Rewrites a scripted line in every style with the live instant model.
/// Returns the number of failed checks.
#[cfg(windows)]
pub fn run_copilot_rewrites(model: Option<&str>) -> anyhow::Result<usize> {
    check::rewrites(model)
}

/// Measures what priming answers with the user's knowledge buys. Returns the
/// number of failed checks.
#[cfg(windows)]
pub fn run_copilot_primer(instant: Option<&str>) -> anyhow::Result<usize> {
    check::primer(instant)
}

/// Compares models role by role on one realistic call. Returns the number
/// of failed checks.
#[cfg(windows)]
pub fn run_copilot_bakeoff(rounds: usize, models: &[String]) -> anyhow::Result<usize> {
    check::bakeoff(rounds, models)
}

/// Scores which lines of labelled calls the copilot would answer. Returns
/// the number of failed judge calls.
#[cfg(windows)]
pub fn run_copilot_questions(
    files: &[std::path::PathBuf],
    fresh: bool,
    samples: usize,
    ask_model: bool,
) -> anyhow::Result<usize> {
    question_eval::run(files, fresh, samples, ask_model)
}

/// Drafts and grades answers to the questions in labelled calls. Returns
/// the number of failed calls.
#[cfg(windows)]
pub fn run_copilot_answers(
    files: &[std::path::PathBuf],
    fresh: bool,
    samples: usize,
) -> anyhow::Result<usize> {
    answer_eval::run(files, fresh, samples)
}

/// Times the wrap-up summary of a long call for each model. Returns the
/// number of failed checks.
#[cfg(windows)]
pub fn run_copilot_notes_bakeoff(models: &[String]) -> anyhow::Result<usize> {
    check::notes_bakeoff(models)
}

/// Reads the meeting window once, as live naming does. Returns the number of
/// failed checks.
#[cfg(windows)]
pub fn run_copilot_window(image: Option<&std::path::Path>) -> anyhow::Result<usize> {
    check::window(image)
}

/// Plays a recording through the speakers and reports any echo that reached
/// the microphone. Returns the number of failed checks.
#[cfg(windows)]
pub fn run_copilot_echo_test(speech: &std::path::Path) -> anyhow::Result<usize> {
    check::echo_test(speech)
}

/// When the dictation shortcut should open the copilot instead, returns why.
#[cfg(windows)]
pub fn copilot_launch_reason(config_file: &std::path::Path) -> Option<String> {
    let settings_path = store::settings_path(config_file);
    let settings = store::load(&settings_path);
    let listening = win::instance::is_running();
    if !settings.launch_from_dictation_hotkey || listening {
        return None;
    }
    let signal = meeting::classify(
        &win::signals::active_microphone_apps(),
        &win::signals::window_titles(),
    );
    dictation_opens_copilot(
        settings.launch_from_dictation_hotkey,
        listening,
        signal.in_meeting,
    )
    .then_some(signal.reason)
}

/// Whether the dictation shortcut opens the copilot rather than dictating.
/// Pressed in a meeting, it always does, even for a copilot the user ended
/// earlier in that meeting: pressing it asks for it. Only a copilot that is
/// listening keeps it dictating; one still writing the notes of a meeting
/// that ended does not.
fn dictation_opens_copilot(enabled: bool, listening: bool, in_meeting: bool) -> bool {
    enabled && !listening && in_meeting
}

/// How often the background app looks for a meeting to open the copilot for.
#[cfg(windows)]
const AUTO_START_POLL: std::time::Duration = std::time::Duration::from_secs(5);

/// Whether the copilot opens by itself now: a meeting has just begun, the
/// copilot is not running, and the user did not end it during this meeting.
fn should_auto_start(
    in_meeting: bool,
    was_in_meeting: bool,
    running: bool,
    dismissed_at: Option<std::time::SystemTime>,
    now: std::time::SystemTime,
) -> bool {
    // Only the start of a meeting opens it, so ending the copilot mid-call
    // is respected for the rest of that call.
    in_meeting && !was_in_meeting && !running && launch_decision(true, dismissed_at, now).0
}

/// Opens the copilot when a meeting starts, for users who turned that on.
/// Runs for the life of the background app; the setting is read each time,
/// so switching it takes effect without a restart.
#[cfg(windows)]
pub fn watch_for_meetings() {
    let Ok(paths) = crate::AppPaths::from_environment() else {
        return;
    };
    let settings_path = store::settings_path(&paths.config_file);
    let spawned = std::thread::Builder::new()
        .name("copilot-auto-start".into())
        .spawn(move || {
            let mut was_in_meeting = false;
            loop {
                std::thread::sleep(AUTO_START_POLL);
                if !store::load(&settings_path).auto_start {
                    was_in_meeting = false;
                    continue;
                }
                let signal = meeting::classify(
                    &win::signals::active_microphone_apps(),
                    &win::signals::window_titles(),
                );
                let marker = dismissed_marker(&settings_path);
                let dismissed_at = std::fs::metadata(&marker)
                    .and_then(|metadata| metadata.modified())
                    .ok();
                // The meeting the user ended the copilot in is over.
                if !signal.in_meeting && dismissed_at.is_some() {
                    let _ = std::fs::remove_file(&marker);
                }
                if should_auto_start(
                    signal.in_meeting,
                    was_in_meeting,
                    win::instance::is_running(),
                    dismissed_at,
                    std::time::SystemTime::now(),
                ) {
                    tracing::info!(reason = %signal.reason, "a meeting started; opening the copilot");
                    if let Err(error) = launch_copilot() {
                        tracing::warn!(%error, "the copilot could not be opened for the meeting");
                    }
                }
                was_in_meeting = signal.in_meeting;
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "the meeting watcher could not start");
    }
}

/// After the user ends the copilot, it does not open by itself again for the
/// rest of that meeting; this long bounds a meeting.
const DISMISSED_FOR: std::time::Duration = std::time::Duration::from_secs(4 * 60 * 60);

/// Marks that the user ended the copilot during a meeting.
fn dismissed_marker(settings_path: &std::path::Path) -> std::path::PathBuf {
    settings_path.with_file_name("copilot-dismissed")
}

/// Whether the copilot may open by itself, and whether the "ended during
/// this meeting" mark is spent: it lasts until no meeting is detected, or
/// for at most [`DISMISSED_FOR`].
fn launch_decision(
    in_meeting: bool,
    dismissed_at: Option<std::time::SystemTime>,
    now: std::time::SystemTime,
) -> (bool, bool) {
    let fresh =
        dismissed_at.is_some_and(|at| now.duration_since(at).is_ok_and(|age| age < DISMISSED_FOR));
    match (in_meeting, fresh) {
        (false, _) => (false, dismissed_at.is_some()),
        (true, true) => (false, false),
        (true, false) => (true, dismissed_at.is_some()),
    }
}

/// What a copilot session runs on: transcription (streamed when an OpenAI
/// key is set), answer providers, models, and where notes go.
#[cfg(windows)]
fn session_services(
    paths: &crate::AppPaths,
    settings: &agentdictate_core::CopilotSettings,
    settings_path: std::path::PathBuf,
    platform: std::sync::Arc<dyn engine::Platform>,
) -> anyhow::Result<engine::Services> {
    use std::sync::{Arc, Mutex};

    use agentdictate_core::{CopilotProvider, ModelChoice};

    use crate::codex_subscription::{ChatGptCredentials, transcribe_wav_with_chatgpt};
    use engine::{ModelLoader, ProviderFactory, Services, Transcriber};

    let dictation = agentdictate_runtime::load_settings(&paths.config_file).ok();
    let language = if settings.language.trim().is_empty() {
        dictation
            .as_ref()
            .map(|dictation| dictation.language.trim().to_owned())
            .filter(|language| !language.contains(','))
            .unwrap_or_default()
    } else {
        settings.language.trim().to_owned()
    };
    let dictation_openai_key = dictation
        .as_ref()
        .map(|dictation| dictation.openai_api_key.trim().to_owned())
        .unwrap_or_default();
    tracing::info!(settings = ?settings.redacted(), "copilot settings loaded");

    let chatgpt = Arc::new(ChatGptCredentials::discover());
    {
        let chatgpt = Arc::clone(&chatgpt);
        let _ = std::thread::Builder::new()
            .name("copilot-auth-warmup".into())
            .spawn(move || {
                if let Err(error) = chatgpt.bearer(false) {
                    tracing::warn!(%error, "ChatGPT sign-in is unavailable for the copilot");
                }
            });
    }
    let known_chatgpt_models: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    // An AssemblyAI key streams speech to its service, which also judges
    // when a speaker has finished; otherwise an OpenAI API key streams it.
    // Without either, or if the stream fails, speech is uploaded at each
    // pause.
    let openai_live: Option<live::LiveFactory> = {
        let key = if settings.openai_api_key.trim().is_empty() {
            dictation_openai_key.clone()
        } else {
            settings.openai_api_key.trim().to_owned()
        };
        let language = language.clone();
        (!key.is_empty()).then(|| {
            Arc::new(move |speaker, events| {
                live::spawn(
                    live::REALTIME_URL.into(),
                    key.clone(),
                    live::transcription_settings(&language),
                    speaker,
                    events,
                )
            }) as live::LiveFactory
        })
    };
    let assembly_key = settings.assemblyai_api_key.trim().to_owned();
    let live_factory: Option<live::LiveFactory> = if assembly_key.is_empty() {
        openai_live
    } else {
        // The key is tried once as the session starts; the service is used
        // only once it has accepted it, so a wrong key costs nothing.
        let url = assembly::session_url(assembly::STREAMING_URL, &language);
        let accepted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let (url, key, accepted) = (url.clone(), assembly_key.clone(), Arc::clone(&accepted));
            let _ = std::thread::Builder::new()
                .name("copilot-assembly-check".into())
                .spawn(move || match assembly::check_key(&url, &key) {
                    Ok(()) => {
                        tracing::info!("copilot transcribes with AssemblyAI");
                        accepted.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    Err(error) => tracing::warn!(
                        %error,
                        "AssemblyAI did not accept its key; transcribing without it"
                    ),
                });
        }
        Some(Arc::new(move |speaker, events| match &openai_live {
            Some(openai) if !accepted.load(std::sync::atomic::Ordering::Relaxed) => {
                openai(speaker, events)
            }
            _ => assembly::spawn(url.clone(), assembly_key.clone(), speaker, events),
        }) as live::LiveFactory)
    };
    let transcriber: Transcriber = {
        let chatgpt = Arc::clone(&chatgpt);
        let openai_key = if settings.openai_api_key.trim().is_empty() {
            dictation_openai_key.clone()
        } else {
            settings.openai_api_key.trim().to_owned()
        };
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(45))
            .build()?;
        // An OpenAI API key is the user's chosen route for transcription;
        // the ChatGPT sign-in is the fallback.
        let mut routes = Vec::new();
        if !openai_key.is_empty() {
            let client = client.clone();
            routes.push(TranscriptionRoute::new("OpenAI", move |wav, language| {
                providers::transcribe_wav_with_openai(&client, &openai_key, wav.to_vec(), language)
            }));
        }
        routes.push(TranscriptionRoute::new("ChatGPT", move |wav, language| {
            transcribe_wav_with_chatgpt(&client, &chatgpt, wav, language)
        }));
        Arc::new(move |wav: Vec<u8>, language: &str| {
            transcribe_on_routes(&routes, &wav, language, &std::thread::sleep)
        })
    };
    let provider_factory: ProviderFactory = {
        let chatgpt = Arc::clone(&chatgpt);
        let known = Arc::clone(&known_chatgpt_models);
        let dictation_openai_key = dictation_openai_key.clone();
        Arc::new(move |settings| {
            let fallbacks: Vec<String> = known
                .lock()
                .map(|models| models.iter().take(2).cloned().collect())
                .unwrap_or_default();
            providers::provider_for(settings, &chatgpt, &dictation_openai_key, &fallbacks)
        })
    };
    let model_loader: ModelLoader = {
        let chatgpt = Arc::clone(&chatgpt);
        let known = Arc::clone(&known_chatgpt_models);
        Arc::new(move |provider| {
            if provider != CopilotProvider::ChatgptSubscription {
                return provider.suggested_models();
            }
            match chatgpt.list_models() {
                Ok(models) if !models.is_empty() => {
                    if let Ok(mut known) = known.lock() {
                        *known = models.iter().map(|(id, _)| id.clone()).collect();
                    }
                    models
                        .into_iter()
                        .map(|(id, label)| {
                            let label = if label == id { model_label(&id) } else { label };
                            ModelChoice::new(id, label)
                        })
                        .collect()
                }
                Ok(_) => provider.suggested_models(),
                Err(error) => {
                    tracing::warn!(%error, "could not list ChatGPT models");
                    provider.suggested_models()
                }
            }
        })
    };

    Ok(Services {
        platform,
        providers: provider_factory,
        brain: Arc::new(|settings, query| {
            brain::context(settings, query).map_err(|error| error.to_string())
        }),
        transcriber,
        models: model_loader,
        settings_path,
        notes_dir: paths.meetings_dir(),
        knowledge_dir: paths.knowledge_dir(),
        language,
        live: live_factory,
        account_name: account_name(),
        fetch: Arc::new(fetch_text),
        post: Arc::new(post_json),
    })
}

/// The user's given name as the computer's account has it.
#[cfg(windows)]
fn account_name() -> String {
    let name = std::env::var("USERNAME").unwrap_or_default();
    let name = name
        .split(['.', '_', ' ', '-'])
        .next()
        .unwrap_or_default()
        .to_lowercase();
    let mut letters = name.chars();
    match letters.next() {
        Some(first) if first.is_alphabetic() => first.to_uppercase().chain(letters).collect(),
        _ => String::new(),
    }
}

#[cfg(windows)]
fn http_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|error| error.to_string())
}

/// Reads a calendar feed from a web address or a file.
#[cfg(windows)]
fn fetch_text(source: &str) -> Result<String, String> {
    let source = source.trim();
    // Calendars hand out "webcal" addresses for the same feed.
    let address = match source.strip_prefix("webcal://") {
        Some(rest) => format!("https://{rest}"),
        None => source.to_owned(),
    };
    if address.starts_with("https://") || address.starts_with("http://") {
        let response = http_client()?
            .get(&address)
            .send()
            .map_err(|error| error.without_url().to_string())?;
        if !response.status().is_success() {
            return Err(format!(
                "the calendar returned HTTP {}",
                response.status().as_u16()
            ));
        }
        response
            .text()
            .map_err(|error| error.without_url().to_string())
    } else {
        std::fs::read_to_string(&address).map_err(|error| error.to_string())
    }
}

/// Sends a finished meeting to the user's webhook.
#[cfg(windows)]
fn post_json(address: &str, body: &str) -> Result<(), String> {
    if !(address.starts_with("https://") || address.starts_with("http://")) {
        return Err("the webhook address must start with https://".into());
    }
    let response = http_client()?
        .post(address)
        .header("content-type", "application/json")
        .body(body.to_owned())
        .send()
        .map_err(|error| error.without_url().to_string())?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!("HTTP {}", response.status().as_u16()))
    }
}

/// Opens the main window on the Meeting Copilot tab of its Settings, as the
/// tray's Meeting notes item opens it on Meetings. The copilot runs from `agentdictated.exe`;
/// the window is `agentdictate.exe` beside it.
#[cfg(all(windows, feature = "desktop"))]
fn open_main_settings() {
    let opened = std::env::current_exe().and_then(|executable| {
        std::process::Command::new(executable.with_file_name("agentdictate.exe"))
            .args(["--route", "copilot-settings"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
    });
    if let Err(error) = opened {
        tracing::warn!(%error, "settings could not be opened from the copilot");
    }
}

/// Runs the copilot process until the user ends the session.
#[cfg(all(windows, feature = "desktop"))]
pub fn run_copilot() -> anyhow::Result<()> {
    use std::{
        rc::Rc,
        sync::{Arc, mpsc},
    };

    use agentdictate_core::{CopilotAction, CopilotEvent};
    use agentdictate_ui::{CopilotViewModel, CopilotWindowHost, run_copilot_window};

    use engine::{Engine, EngineMsg, UiSink};

    let Some(mut instance) = win::instance::acquire() else {
        tracing::info!("meeting copilot is already running; asked it to show");
        return Ok(());
    };
    tracing::info!("meeting copilot starting");
    let paths = crate::AppPaths::from_environment()?;
    paths.ensure_directories()?;
    let settings_path = store::settings_path(&paths.config_file);
    let settings = store::load(&settings_path);
    let marker = dismissed_marker(&settings_path);
    let window_place = settings_path.with_file_name("copilot-window.json");
    let (ui_sender, ui_events) = mpsc::channel::<CopilotEvent>();
    let ui: UiSink = Arc::new(move |event| {
        let _ = ui_sender.send(event);
    });
    let (engine_sender, engine_messages) = mpsc::channel::<EngineMsg>();
    let services = session_services(
        &paths,
        &settings,
        settings_path,
        Arc::new(win::WindowsPlatform),
    )?;
    let mut engine = Engine::new(
        services,
        settings.clone(),
        Arc::clone(&ui),
        engine_sender.clone(),
    );
    // What only the listening copilot holds: the single-copilot guard and
    // the global shortcuts. Both are let go once the meeting ends, so the
    // dictation shortcut can open a new copilot while this one writes its
    // notes.
    let listening: Arc<std::sync::Mutex<ListeningHold>> = Arc::default();
    {
        let listening = Arc::clone(&listening);
        engine.when_listening_stops(move || {
            if let Ok(mut hold) = listening.lock() {
                hold.stopped = true;
                hold.held.take();
            }
            tracing::info!("meeting copilot stopped listening; a new one may start");
        });
    }
    let engine_thread = std::thread::Builder::new()
        .name("copilot-engine".into())
        .spawn(move || engine.run(engine_messages))?;

    let (hotkeys, shortcuts) = {
        let sender = engine_sender.clone();
        win::hotkeys::start(move |command| {
            let _ = sender.send(EngineMsg::Hotkey(command));
        })
        .map_err(anyhow::Error::msg)?
    };
    {
        let sender = engine_sender.clone();
        instance.watch_show_requests(move || {
            let _ = sender.send(EngineMsg::ShowRequested);
        });
    }
    match listening.lock() {
        Ok(mut hold) if !hold.stopped => hold.held = Some((instance, hotkeys)),
        // The meeting already ended.
        _ => drop((instance, hotkeys)),
    }
    // A listening session always shows in the notification area, even with
    // its window hidden.
    let tray = {
        let sender = engine_sender.clone();
        win::tray::start(move |command| {
            let _ = sender.send(match command {
                win::tray::TrayCommand::ShowOrHide => {
                    EngineMsg::Hotkey(engine::HotkeyCommand::Toggle)
                }
                win::tray::TrayCommand::End => EngineMsg::Action(CopilotAction::End),
            });
        })
    };
    let tray = match tray {
        Ok(tray) => Some(tray),
        Err(error) => {
            tracing::warn!(%error, "copilot tray icon could not be shown");
            None
        }
    };

    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let hide_from_screen_share = settings.hide_from_screen_share;
    let mut initial = CopilotViewModel::new(settings, started);
    initial.shortcuts = shortcuts;
    let actions = {
        let sender = engine_sender.clone();
        Arc::new(move |action: CopilotAction| {
            // "More settings" opens the main window's Settings, which the
            // engine has no part in.
            if matches!(&action, CopilotAction::OpenUrl(url) if url == agentdictate_ui::COPILOT_SETTINGS_LINK)
            {
                open_main_settings();
                return;
            }
            let _ = sender.send(EngineMsg::Action(action));
        })
    };
    run_copilot_window(initial, ui_events, actions, move |hwnd, _scale| {
        Rc::new(win::host::Win32CopilotHost::attach(
            hwnd,
            hide_from_screen_share,
            window_place.clone(),
        )) as Rc<dyn CopilotWindowHost>
    });
    tracing::info!("meeting copilot window closed");
    let _ = engine_sender.send(EngineMsg::Action(CopilotAction::End));
    drop(engine_sender);
    // Ended by the user rather than by the meeting ending: it does not
    // open by itself again during this meeting.
    if engine_thread.join().unwrap_or(false) {
        let _ = std::fs::write(&marker, b"");
    }
    drop(tray);
    if let Ok(mut hold) = listening.lock() {
        hold.held.take();
    }
    Ok(())
}

/// What a copilot holds only while it listens, and whether it has stopped.
#[cfg(all(windows, feature = "desktop"))]
#[derive(Default)]
struct ListeningHold {
    stopped: bool,
    held: Option<(win::instance::InstanceGuard, win::hotkeys::CopilotHotkeys)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ending_the_copilot_lets_the_shortcut_dictate_until_the_meeting_ends() {
        let now = std::time::SystemTime::now();
        let ended = Some(now - std::time::Duration::from_secs(60));
        assert_eq!(launch_decision(true, None, now), (true, false));
        assert_eq!(launch_decision(true, ended, now), (false, false));
        // Pressed outside a meeting: the mark is spent.
        assert_eq!(launch_decision(false, ended, now), (false, true));
        let long_ago = Some(now - DISMISSED_FOR - std::time::Duration::from_secs(1));
        assert_eq!(launch_decision(true, long_ago, now), (true, true));
    }

    #[test]
    fn the_shortcut_opens_a_new_copilot_while_the_last_one_writes_its_notes() {
        // A copilot past its meeting no longer counts as listening.
        assert!(dictation_opens_copilot(true, false, true));
        assert!(!dictation_opens_copilot(true, true, true));
        assert!(!dictation_opens_copilot(true, false, false));
        assert!(!dictation_opens_copilot(false, false, true));
    }

    #[test]
    fn the_copilot_opens_itself_only_as_a_meeting_begins() {
        let now = std::time::SystemTime::now();
        let ended = Some(now - std::time::Duration::from_secs(60));
        assert!(should_auto_start(true, false, false, None, now));
        // Already in the meeting, already open, or no meeting: nothing.
        assert!(!should_auto_start(true, true, false, None, now));
        assert!(!should_auto_start(true, false, true, None, now));
        assert!(!should_auto_start(false, false, false, None, now));
        // Ended by hand a minute ago: a blip in detection does not reopen it.
        assert!(!should_auto_start(true, false, false, ended, now));
    }

    #[test]
    fn model_ids_become_readable_labels() {
        assert_eq!(model_label("gpt-6.1-sol"), "GPT-6.1 Sol");
        assert_eq!(model_label("gpt-5.6-terra"), "GPT-5.6 Terra");
        assert_eq!(model_label("gpt-5.5"), "GPT-5.5");
        assert_eq!(model_label("o3-mini"), "O3 Mini");
    }

    #[test]
    fn rate_limits_and_outages_are_retried_but_rejections_are_not() {
        assert!(is_transient_failure(
            "ChatGPT transcription limit reached. Try again later."
        ));
        assert!(is_transient_failure(
            "ChatGPT transcription is temporarily unavailable."
        ));
        assert!(is_transient_failure(
            "Could not reach ChatGPT transcription: timeout"
        ));
        assert!(!is_transient_failure(
            "ChatGPT transcription rejected the recording."
        ));
        assert!(!is_transient_failure(
            "ChatGPT sign-in expired. Open Codex and sign in again."
        ));
    }

    #[test]
    fn a_used_up_plan_or_a_rejected_sign_in_is_not_waited_out() {
        for lasting in [
            "ChatGPT transcription limit reached. The usage limit has been reached. It resets in about 29 days.",
            "ChatGPT rate limit reached. Try again shortly. It resets in about 3 h 20 min.",
            "OpenAI transcription rate limit reached. Try again shortly. You exceeded your current quota, please check your plan and billing details.",
            "OpenAI transcription rejected the sign-in or key. Incorrect API key provided.",
            "ChatGPT sign-in expired. Open Codex and sign in again.",
            "ChatGPT subscription transcription is unavailable for this account.",
            "Codex is not signed in with ChatGPT.",
        ] {
            assert!(is_lasting_failure(lasting), "{lasting}");
            assert!(!is_transient_failure(lasting), "{lasting}");
        }
        for brief in [
            "ChatGPT transcription limit reached. Try again later.",
            "ChatGPT transcription limit reached. It resets in about 2 min.",
            "Could not reach ChatGPT transcription: timeout",
        ] {
            assert!(!is_lasting_failure(brief), "{brief}");
            assert!(is_transient_failure(brief), "{brief}");
        }
    }

    fn scripted_route(
        name: &'static str,
        calls: &std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
        reply: impl Fn(usize) -> Result<String, String> + Send + Sync + 'static,
    ) -> TranscriptionRoute {
        let calls = std::sync::Arc::clone(calls);
        TranscriptionRoute::new(name, move |_, _| {
            let mut calls = calls.lock().unwrap();
            calls.push(name);
            let made = calls.iter().filter(|call| **call == name).count();
            drop(calls);
            reply(made)
        })
    }

    #[test]
    fn a_route_that_fails_for_good_is_passed_over_for_the_rest_of_the_session() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let routes = [
            scripted_route("OpenAI", &calls, |_| {
                Err(
                    "OpenAI transcription rate limit reached. You exceeded your current quota."
                        .into(),
                )
            }),
            scripted_route("ChatGPT", &calls, |made| Ok(format!("line {made}"))),
        ];
        let no_wait = |_| {};
        assert_eq!(
            transcribe_on_routes(&routes, b"wav", "", &no_wait).unwrap(),
            "line 1"
        );
        assert_eq!(
            transcribe_on_routes(&routes, b"wav", "", &no_wait).unwrap(),
            "line 2"
        );
        // Tried once, never retried, then skipped.
        assert_eq!(*calls.lock().unwrap(), ["OpenAI", "ChatGPT", "ChatGPT"]);
    }

    #[test]
    fn brief_limits_are_retried_until_they_keep_failing() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let routes = [scripted_route("ChatGPT", &calls, |_| {
            Err("ChatGPT transcription limit reached. Try again later.".into())
        })];
        let waits = std::sync::Mutex::new(0);
        let pause = |_| *waits.lock().unwrap() += 1;
        for _ in 0..LIMITED_IN_A_ROW {
            assert!(transcribe_on_routes(&routes, b"wav", "", &pause).is_err());
        }
        assert_eq!(calls.lock().unwrap().len(), 3 * LIMITED_IN_A_ROW);
        // Past that, each utterance gets one try, so speech stops queueing
        // behind retries.
        assert!(transcribe_on_routes(&routes, b"wav", "", &pause).is_err());
        assert_eq!(calls.lock().unwrap().len(), 3 * LIMITED_IN_A_ROW + 1);
        assert_eq!(*waits.lock().unwrap(), 2 * LIMITED_IN_A_ROW);
    }

    #[test]
    fn only_the_copilot_flag_selects_copilot_mode() {
        assert!(is_copilot_argument(Some("--copilot")));
        assert!(!is_copilot_argument(Some("--service")));
        assert!(!is_copilot_argument(None));
    }
}
