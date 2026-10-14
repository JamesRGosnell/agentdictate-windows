//! Meeting copilot: a live, capture-invisible assistant for calls.
//!
//! The copilot runs as its own process (`agentdictated --copilot`) so a
//! meeting session never interferes with dictation. It listens to the
//! microphone and system audio, transcribes both sides, answers questions as
//! they are asked, and writes meeting notes when the session ends.

// The session runtime is Windows-only; shared logic stays testable elsewhere.
#![cfg_attr(not(all(windows, feature = "desktop")), allow(dead_code))]

#[cfg(windows)]
mod check;
mod engine;
mod knowledge;
mod meeting;
mod providers;
mod reveal;
mod store;
mod stream;
mod vad;
mod voices;
#[cfg(windows)]
mod win;

pub const COPILOT_ARGUMENT: &str = "--copilot";

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

/// Errors worth retrying: rate limits, outages, and dropped connections.
fn is_transient_failure(error: &str) -> bool {
    let error = error.to_lowercase();
    ["limit", "temporarily unavailable", "could not reach"]
        .iter()
        .any(|pattern| error.contains(pattern))
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

/// Plays a recording through the speakers and reports any echo that reached
/// the microphone. Returns the number of failed checks.
#[cfg(windows)]
pub fn run_copilot_echo_test(speech: &std::path::Path) -> anyhow::Result<usize> {
    check::echo_test(speech)
}

/// When the dictation shortcut should open the copilot instead, returns why.
#[cfg(windows)]
pub fn copilot_launch_reason(config_file: &std::path::Path) -> Option<String> {
    let settings = store::load(&store::settings_path(config_file));
    if !settings.launch_from_dictation_hotkey || win::instance::is_running() {
        return None;
    }
    let signal = meeting::classify(
        &win::signals::active_microphone_apps(),
        &win::signals::window_titles(),
    );
    signal.in_meeting.then_some(signal.reason)
}

/// Runs the copilot process until the user ends the session.
#[cfg(all(windows, feature = "desktop"))]
pub fn run_copilot() -> anyhow::Result<()> {
    use std::{
        rc::Rc,
        sync::{Arc, Mutex, mpsc},
    };

    use agentdictate_core::{CopilotAction, CopilotEvent, CopilotProvider, ModelChoice};
    use agentdictate_ui::{CopilotViewModel, CopilotWindowHost, run_copilot_window};

    use crate::codex_subscription::{ChatGptCredentials, transcribe_wav_with_chatgpt};
    use engine::{Engine, EngineMsg, ModelLoader, ProviderFactory, Services, Transcriber, UiSink};

    let Some(instance) = win::instance::acquire() else {
        tracing::info!("meeting copilot is already running; asked it to show");
        return Ok(());
    };
    tracing::info!("meeting copilot starting");
    let paths = crate::AppPaths::from_environment()?;
    paths.ensure_directories()?;
    let settings_path = store::settings_path(&paths.config_file);
    let settings = store::load(&settings_path);
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
        Arc::new(move |wav: Vec<u8>, language: &str| {
            // A meeting sends many short requests; ride out brief rate limits
            // and network blips instead of dropping what was said.
            let mut attempt = 0;
            let result = loop {
                match transcribe_wav_with_chatgpt(&client, &chatgpt, &wav, language) {
                    Err(error) if attempt < 2 && is_transient_failure(&error) => {
                        attempt += 1;
                        tracing::warn!(%error, attempt, "retrying copilot transcription");
                        std::thread::sleep(std::time::Duration::from_millis(1_500 * attempt));
                    }
                    result => break result,
                }
            };
            match result {
                Ok(text) => Ok(text),
                Err(error) if !openai_key.is_empty() => {
                    tracing::warn!(%error, "ChatGPT transcription failed; using the OpenAI API");
                    providers::transcribe_wav_with_openai(&client, &openai_key, wav, language)
                }
                Err(error) => Err(error),
            }
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

    let (ui_sender, ui_events) = mpsc::channel::<CopilotEvent>();
    let ui: UiSink = Arc::new(move |event| {
        let _ = ui_sender.send(event);
    });
    let (engine_sender, engine_messages) = mpsc::channel::<EngineMsg>();
    let services = Services {
        platform: Arc::new(win::WindowsPlatform),
        providers: provider_factory,
        transcriber,
        models: model_loader,
        settings_path,
        notes_dir: paths.meetings_dir(),
        knowledge_dir: paths.knowledge_dir(),
        language,
    };
    let engine = Engine::new(
        services,
        settings.clone(),
        Arc::clone(&ui),
        engine_sender.clone(),
    );
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
            let _ = sender.send(EngineMsg::Action(action));
        })
    };
    run_copilot_window(initial, ui_events, actions, move |hwnd, _scale| {
        Rc::new(win::host::Win32CopilotHost::attach(
            hwnd,
            hide_from_screen_share,
        )) as Rc<dyn CopilotWindowHost>
    });
    tracing::info!("meeting copilot window closed");
    let _ = engine_sender.send(EngineMsg::Action(CopilotAction::End));
    drop(engine_sender);
    let _ = engine_thread.join();
    drop(hotkeys);
    drop(instance);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn only_the_copilot_flag_selects_copilot_mode() {
        assert!(is_copilot_argument(Some("--copilot")));
        assert!(!is_copilot_argument(Some("--service")));
        assert!(!is_copilot_argument(None));
    }
}
