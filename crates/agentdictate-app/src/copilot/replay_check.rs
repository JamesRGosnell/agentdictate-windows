//! `agentdictate copilot --replay <call.wav> [--upload]`: plays a recorded
//! call through a real copilot session with no window and reports how soon
//! the transcript and automatic answers arrived.
//!
//! The recording is the other side of the call. The session uses the saved
//! answer engine and transcription, but saves no notes, writes no settings,
//! and skips web search, screenshots and live insights, so only the
//! transcription and the answers to questions cost anything. `--upload`
//! turns streaming off to compare with transcribing at each pause.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

use agentdictate_core::{CopilotAction, CopilotEvent, Speaker};

use super::{
    engine::{AudioControl, AudioSink, CallShot, Engine, EngineMsg, Platform},
    session_services, store,
    vad::SAMPLE_RATE,
    win::replay::{ReplayAudio, read_speech},
};

/// The replay waits this long before the call begins.
const LEAD_IN: Duration = Duration::from_millis(2_500);
/// Time after the call for the last answers to arrive.
const TAIL: Duration = Duration::from_secs(20);

struct ReplayPlatform {
    path: PathBuf,
}

impl Platform for ReplayPlatform {
    fn start_audio(
        &self,
        _microphone: bool,
        _system_audio: bool,
        sink: AudioSink,
    ) -> Result<Box<dyn AudioControl>, String> {
        ReplayAudio::start(self.path.clone(), sink)
            .map(|replay| Box::new(replay) as Box<dyn AudioControl>)
    }

    fn capture_screen(&self) -> Result<Vec<u8>, String> {
        Err("no screen during a replay".into())
    }

    fn capture_call_window(&self) -> Result<CallShot, String> {
        Err("no screen during a replay".into())
    }

    fn meeting_signal(&self) -> Option<String> {
        None
    }

    fn copy_text(&self, _text: &str) {}

    fn open(&self, _target: &str) {}
}

/// Replays `path` and prints the timings. Returns the number of problems.
pub(crate) fn run(path: &Path, upload: bool) -> anyhow::Result<usize> {
    let seconds =
        read_speech(path).map_err(anyhow::Error::msg)?.len() as f64 / f64::from(SAMPLE_RATE);
    let paths = crate::AppPaths::from_environment()?;
    let mut settings = store::load(&store::settings_path(&paths.config_file));
    settings.save_notes = false;
    settings.live_insights = false;
    settings.web_search = false;
    settings.screen_context = false;
    settings.auto_answer = true;
    let mut services = session_services(
        &paths,
        &settings,
        // Settings the session adopts stay out of the user's own file.
        paths.cache.join("replay-copilot.json"),
        Arc::new(ReplayPlatform {
            path: path.to_owned(),
        }),
    )?;
    if upload {
        services.live = None;
    }
    println!(
        "Replaying {:.0}s of {} with {} transcription and {} answers",
        seconds,
        path.display(),
        if services.live.is_some() {
            "streaming"
        } else {
            "upload"
        },
        settings.provider.label()
    );

    let events: Arc<Mutex<Vec<(Instant, CopilotEvent)>>> = Arc::default();
    let ui = {
        let events = Arc::clone(&events);
        Arc::new(move |event: CopilotEvent| {
            events.lock().unwrap().push((Instant::now(), event));
        })
    };
    let (sender, incoming) = mpsc::channel::<EngineMsg>();
    let engine = Engine::new(services, settings, ui, sender.clone());
    let worker = std::thread::spawn(move || engine.run(incoming));
    std::thread::sleep(LEAD_IN + Duration::from_secs_f64(seconds) + TAIL);
    let _ = sender.send(EngineMsg::Action(CopilotAction::End));
    let _ = worker.join();

    let events = std::mem::take(&mut *events.lock().unwrap());
    Ok(report(&events))
}

fn report(events: &[(Instant, CopilotEvent)]) -> usize {
    let mut problems = 0;
    let mut speech_ended: Option<Instant> = None;
    let mut speech_started: Option<Instant> = None;
    let mut first_partial: Option<Duration> = None;
    let mut partials = 0;
    let mut lags = Vec::new();
    let mut answers: Vec<(u64, Instant)> = Vec::new();
    let mut first_text = Vec::new();
    for (at, event) in events {
        match event {
            CopilotEvent::Speaking {
                speaker: Speaker::Them,
                active,
            } => {
                if *active {
                    speech_started = Some(*at);
                } else {
                    speech_ended = Some(*at);
                }
            }
            CopilotEvent::PartialTranscript {
                speaker: Speaker::Them,
                text,
            } if !text.is_empty() => {
                partials += 1;
                if first_partial.is_none() {
                    first_partial = speech_started.map(|started| at.duration_since(started));
                }
            }
            CopilotEvent::Transcript(line) if line.speaker == Speaker::Them => {
                let lag = speech_ended.map(|ended| at.saturating_duration_since(ended));
                println!(
                    "  [{:>5} ms after speech ended] {}",
                    lag.map_or_else(|| "?".into(), |lag| lag.as_millis().to_string()),
                    line.text
                );
                lags.extend(lag);
            }
            CopilotEvent::AnswerStarted {
                id,
                automatic: true,
                title,
                ..
            } => {
                println!("  answer started for: {title}");
                answers.push((*id, speech_ended.unwrap_or(*at)));
            }
            CopilotEvent::InstantDelta { id, .. } | CopilotEvent::AnswerDelta { id, .. } => {
                if let Some(index) = answers.iter().position(|(answer, _)| answer == id) {
                    let (_, ended) = answers.remove(index);
                    first_text.push(at.saturating_duration_since(ended));
                }
            }
            CopilotEvent::Problem(Some(problem)) => {
                println!("  problem: {problem}");
                problems += 1;
            }
            CopilotEvent::Notice(notice) => println!("  notice: {notice}"),
            _ => {}
        }
    }
    let median = |values: &mut Vec<Duration>| {
        values.sort_unstable();
        values.get(values.len() / 2).map_or_else(
            || "none".into(),
            |value| format!("{} ms", value.as_millis()),
        )
    };
    println!(
        "Them lines {}, speech end to final line median {}",
        lags.len(),
        median(&mut lags)
    );
    println!(
        "partial updates {partials}, first after {}",
        first_partial.map_or_else(
            || "none".into(),
            |first| format!("{} ms of speech", first.as_millis())
        )
    );
    println!(
        "automatic answers {}, question end to first answer text median {}",
        first_text.len(),
        median(&mut first_text)
    );
    if lags.is_empty() {
        println!("no transcript came back");
        problems += 1;
    }
    problems
}
