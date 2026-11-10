//! `agentdictate copilot --score <recording> [--me=<speaker>] [--minutes=<n>]
//! [--from=<minute>] [--documents=<folder>]`: plays a recorded conversation
//! through a real copilot session, with one speaker as the user, and reports
//! how the whole chain did.
//!
//! The recording is split by speaker with a speaker-labelling transcription.
//! The user's speech plays as the microphone and everyone else as the call,
//! so the session hears what it would have heard live. Every suggestion is
//! then set beside what the person really said next, and a stronger model
//! marks which suggestions were called for, how close each came, and which
//! questions got none. Recordings, transcripts and reports stay on this
//! machine; each run adds one line to `eval\scores\board.csv`.
//!
//! The session uses the saved answer engine but none of the user's own
//! profile, documents or past meetings, since the recording is someone
//! else's conversation; `--documents` names a folder that goes with it.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex, OnceLock, atomic::AtomicBool, mpsc},
    time::{Duration, Instant},
};

use agentdictate_core::{
    BrainSource, CopilotAction, CopilotEffort, CopilotEvent, CopilotSettings, Speaker,
    split_grounding,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{
    engine::{AudioControl, AudioSink, CallShot, Engine, EngineMsg, Platform},
    providers::{self, AnswerRequest},
    session_services, store,
    vad::SAMPLE_RATE,
    win::replay::{Lane, ReplayAudio, read_speech},
};
use crate::codex_subscription::ChatGptCredentials;

const TRANSCRIPTION_ENDPOINT: &str = "https://api.openai.com/v1/audio/transcriptions";
/// Labels who speaks when. OpenAI retires it on 2027-02-26.
const SPEAKER_MODEL: &str = "gpt-4o-transcribe-diarize";
/// Time after the recording for the last answers to arrive.
const TAIL: Duration = Duration::from_secs(20);
/// Speech kept on each side of a labelled stretch, so word edges survive.
const EDGE_SECONDS: f64 = 0.1;
/// The upload limit allows about an hour of compressed speech.
const MAX_MINUTES: f64 = 60.0;
const DEFAULT_MINUTES: f64 = 10.0;
const REPLY_CHARS: usize = 500;
const TRANSCRIPT_CHARS: usize = 60_000;

/// What to score and how.
pub(crate) struct Options {
    pub recording: PathBuf,
    /// The speaker to treat as the user, as the labelling names them.
    pub me: Option<String>,
    pub minutes: Option<f64>,
    /// Where in the recording to start, in minutes.
    pub from: f64,
    pub documents: Option<PathBuf>,
}

/// One stretch of speech by one speaker, timed from the start of the part
/// of the recording being scored.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Segment {
    speaker: String,
    start: f64,
    end: f64,
    text: String,
}

fn parse_segments(body: &str) -> Result<Vec<Segment>, String> {
    let value: Value =
        serde_json::from_str(body).map_err(|error| format!("Unreadable transcript: {error}"))?;
    let segments = value
        .get("segments")
        .and_then(Value::as_array)
        .ok_or_else(|| "The transcript has no speaker segments.".to_owned())?;
    let mut parsed: Vec<Segment> = segments
        .iter()
        .filter_map(|segment| {
            let text = segment.get("text")?.as_str()?.trim().to_owned();
            let start = segment.get("start")?.as_f64()?;
            let end = segment.get("end")?.as_f64()?;
            (!text.is_empty() && end > start).then(|| Segment {
                speaker: segment
                    .get("speaker")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .trim()
                    .to_owned(),
                start,
                end,
                text,
            })
        })
        .collect();
    parsed.sort_by(|a, b| a.start.total_cmp(&b.start));
    Ok(parsed)
}

/// Each speaker with how long they spoke and their first words, the most
/// talkative first.
fn speakers(segments: &[Segment]) -> Vec<(String, f64, String)> {
    let mut found: BTreeMap<&str, (f64, &str)> = BTreeMap::new();
    for segment in segments {
        let entry = found
            .entry(segment.speaker.as_str())
            .or_insert((0.0, segment.text.as_str()));
        entry.0 += segment.end - segment.start;
    }
    let mut speakers: Vec<(String, f64, String)> = found
        .into_iter()
        .map(|(speaker, (seconds, first))| (speaker.to_owned(), seconds, first.to_owned()))
        .collect();
    speakers.sort_by(|a, b| b.1.total_cmp(&a.1));
    speakers
}

/// The recording as two tracks of equal length: the user's speech, and
/// everyone else's.
fn split(samples: &[f32], segments: &[Segment], me: &str) -> (Vec<f32>, Vec<f32>) {
    let mut mine = vec![0.0; samples.len()];
    let mut theirs = vec![0.0; samples.len()];
    let rate = f64::from(SAMPLE_RATE);
    for (index, segment) in segments.iter().enumerate() {
        // The edge never reaches into a neighbour's speech.
        let earliest = index
            .checked_sub(1)
            .map_or(0.0, |previous| segments[previous].end);
        let latest = segments
            .get(index + 1)
            .map_or(f64::INFINITY, |next| next.start);
        let start = (segment.start - EDGE_SECONDS).max(earliest.min(segment.start));
        let end = (segment.end + EDGE_SECONDS).min(latest.max(segment.end));
        let from = ((start.max(0.0) * rate) as usize).min(samples.len());
        let to = ((end * rate) as usize).min(samples.len());
        let track = if segment.speaker == me {
            &mut mine
        } else {
            &mut theirs
        };
        track[from..to].copy_from_slice(&samples[from..to]);
    }
    (mine, theirs)
}

/// A suggestion the session showed without being asked.
#[derive(Clone, Debug, Default, PartialEq)]
struct Suggestion {
    /// What the session took the question to be.
    question: String,
    /// When the other side stopped speaking, in seconds into the recording.
    asked: f64,
    /// When the first words appeared.
    shown: Option<f64>,
    instant: String,
    researched: String,
}

impl Suggestion {
    /// The words the user would have read.
    fn line(&self) -> String {
        let text = if self.instant.trim().is_empty() {
            &self.researched
        } else {
            &self.instant
        };
        split_grounding(text).0.trim().to_owned()
    }
}

/// The suggestions in a session's events, timed against `began`.
fn suggestions(events: &[(Instant, CopilotEvent)], began: Instant) -> Vec<Suggestion> {
    let seconds = |at: &Instant| at.saturating_duration_since(began).as_secs_f64();
    let mut speech_ended: Option<f64> = None;
    let mut found: Vec<(u64, Suggestion)> = Vec::new();
    for (at, event) in events {
        match event {
            CopilotEvent::Speaking {
                speaker: Speaker::Them,
                active: false,
            } => speech_ended = Some(seconds(at)),
            CopilotEvent::AnswerStarted {
                id,
                automatic: true,
                title,
                ..
            } => found.push((
                *id,
                Suggestion {
                    question: title.clone(),
                    asked: speech_ended.unwrap_or_else(|| seconds(at)),
                    ..Suggestion::default()
                },
            )),
            CopilotEvent::InstantDelta { id, text } | CopilotEvent::AnswerDelta { id, text } => {
                let Some((_, suggestion)) = found.iter_mut().find(|(known, _)| known == id) else {
                    continue;
                };
                suggestion.shown.get_or_insert_with(|| seconds(at));
                if matches!(event, CopilotEvent::InstantDelta { .. }) {
                    suggestion.instant.push_str(text);
                } else {
                    suggestion.researched.push_str(text);
                }
            }
            _ => {}
        }
    }
    found
        .into_iter()
        .map(|(_, suggestion)| suggestion)
        .collect()
}

/// What the user really said after `asked`: when they began and their words
/// until the other side spoke again.
fn reply_after(segments: &[Segment], me: &str, asked: f64) -> Option<(f64, String)> {
    // A reply can begin a moment before the other side's last word ends.
    let first = segments
        .iter()
        .position(|segment| segment.speaker == me && segment.start >= asked - 1.0)?;
    let mut text = String::new();
    for segment in segments[first..]
        .iter()
        .take_while(|segment| segment.speaker == me)
    {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&segment.text);
    }
    Some((segments[first].start, clip(&text, REPLY_CHARS)))
}

fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        text.to_owned()
    } else {
        let clipped: String = text.chars().take(limit).collect();
        format!("{clipped}…")
    }
}

fn clock(seconds: f64) -> String {
    let seconds = seconds.max(0.0) as u64;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// The conversation as the grader reads it.
fn transcript(segments: &[Segment], me: &str, offset: f64) -> String {
    let mut text = String::new();
    for segment in segments {
        let who = if segment.speaker == me {
            "Me".to_owned()
        } else {
            format!("Them ({})", segment.speaker)
        };
        text.push_str(&format!(
            "[{}] {who}: {}\n",
            clock(segment.start + offset),
            segment.text
        ));
    }
    clip(&text, TRANSCRIPT_CHARS)
}

const GRADER_SYSTEM: &str = "You review a meeting assistant after a call. \"Me\" wore the assistant; it listened and, unasked, showed Me a line to say whenever it judged that someone had asked Me something. You get the real transcript and each suggestion with the time it appeared. The assistant did not know Me's private facts, so a suggestion that leaves a [bracket] for a fact only Me knows is not wrong for that. For each suggestion decide: \"warranted\": true when the words just before it were a question, request or objection put to Me that Me had to answer and a prompt could help with; false when it was small talk, rhetorical, aimed at someone else, a remark, or already answered. \"match\": 1 to 5, how well the suggestion would have served Me compared with what Me actually said next: 5 says essentially the same thing, 4 a good answer with a different emphasis, 3 reasonable but generic, 2 weak or beside the point, 1 wrong or contradicting Me. Then list under \"missed\" every question or request put to Me that deserved a suggestion and got none; leave out anything Me could answer instantly from their own life. Respond with ONLY JSON: {\"suggestions\": [{\"n\": 1, \"warranted\": true, \"match\": 4, \"note\": \"a few words\"}], \"missed\": [{\"at\": \"3:12\", \"question\": \"the question, shortened\"}]}";

fn grader_prompt(transcript: &str, suggestions: &[Suggestion], offset: f64) -> String {
    let mut text = format!("<transcript>\n{transcript}</transcript>\n\nSuggestions shown:\n");
    if suggestions.is_empty() {
        text.push_str("(none)\n");
    }
    for (index, suggestion) in suggestions.iter().enumerate() {
        text.push_str(&format!(
            "{}. [{}] after \"{}\" it suggested: \"{}\"\n",
            index + 1,
            clock(suggestion.shown.unwrap_or(suggestion.asked) + offset),
            clip(&suggestion.question, 200),
            clip(&suggestion.line(), 400)
        ));
    }
    text
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Grade {
    warranted: bool,
    matched: Option<u8>,
    note: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Grades {
    /// By suggestion, in the order shown.
    suggestions: Vec<Option<Grade>>,
    /// Time and question.
    missed: Vec<(String, String)>,
}

fn parse_grades(response: &str, count: usize) -> Option<Grades> {
    let start = response.find('{')?;
    let end = response.rfind('}')?;
    let value: Value = serde_json::from_str(response.get(start..=end)?).ok()?;
    let mut suggestions = vec![None; count];
    for graded in value.get("suggestions")?.as_array()? {
        let Some(index) = graded
            .get("n")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .and_then(|n| n.checked_sub(1))
            .filter(|index| *index < count)
        else {
            continue;
        };
        suggestions[index] = Some(Grade {
            warranted: graded
                .get("warranted")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            matched: graded
                .get("match")
                .and_then(Value::as_u64)
                .map(|score| score.clamp(1, 5) as u8),
            note: graded
                .get("note")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        });
    }
    let missed = value
        .get("missed")
        .and_then(Value::as_array)
        .map(|missed| {
            missed
                .iter()
                .filter_map(|item| {
                    Some((
                        item.get("at")
                            .and_then(Value::as_str)
                            .unwrap_or("?")
                            .to_owned(),
                        item.get("question")?.as_str()?.to_owned(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Grades {
        suggestions,
        missed,
    })
}

/// One run's headline numbers.
#[derive(Clone, Debug, Default, PartialEq)]
struct Totals {
    minutes: f64,
    suggestions: usize,
    unwanted: usize,
    missed: usize,
    first_words_median_ms: Option<u64>,
    first_words_p90_ms: Option<u64>,
    /// Wanted suggestions that appeared before the person began replying.
    ahead: usize,
    wanted: usize,
    matched: Option<f64>,
}

fn totals(
    minutes: f64,
    suggestions: &[Suggestion],
    replies: &[Option<(f64, String)>],
    grades: &Grades,
) -> Totals {
    let mut waits: Vec<u64> = suggestions
        .iter()
        .filter_map(|suggestion| {
            suggestion
                .shown
                .map(|shown| ((shown - suggestion.asked).max(0.0) * 1_000.0) as u64)
        })
        .collect();
    waits.sort_unstable();
    let wanted: Vec<usize> = (0..suggestions.len())
        .filter(|index| {
            grades
                .suggestions
                .get(*index)
                .and_then(Option::as_ref)
                .is_some_and(|grade| grade.warranted)
        })
        .collect();
    let scores: Vec<f64> = wanted
        .iter()
        .filter_map(|index| grades.suggestions[*index].as_ref()?.matched)
        .map(f64::from)
        .collect();
    Totals {
        minutes,
        suggestions: suggestions.len(),
        unwanted: suggestions.len() - wanted.len(),
        missed: grades.missed.len(),
        first_words_median_ms: waits.get(waits.len() / 2).copied(),
        first_words_p90_ms: waits
            .get((waits.len() * 9 / 10).min(waits.len().saturating_sub(1)))
            .copied(),
        ahead: wanted
            .iter()
            .filter(|index| {
                matches!(
                    (suggestions[**index].shown, &replies[**index]),
                    (Some(shown), Some((replied, _))) if shown <= *replied
                )
            })
            .count(),
        wanted: wanted.len(),
        matched: (!scores.is_empty()).then(|| scores.iter().sum::<f64>() / scores.len() as f64),
    }
}

impl Totals {
    fn per_ten_minutes(&self, count: usize) -> f64 {
        if self.minutes > 0.0 {
            count as f64 / self.minutes * 10.0
        } else {
            0.0
        }
    }

    fn lines(&self) -> Vec<String> {
        let ms = |value: Option<u64>| value.map_or_else(|| "none".into(), |ms| format!("{ms} ms"));
        vec![
            format!(
                "Suggestions shown: {} in {:.0} min",
                self.suggestions, self.minutes
            ),
            format!(
                "Unwanted pop-ups: {} ({:.1} per 10 min)",
                self.unwanted,
                self.per_ten_minutes(self.unwanted)
            ),
            format!("Missed questions: {}", self.missed),
            format!(
                "Time to first words: median {}, slowest tenth over {}",
                ms(self.first_words_median_ms),
                ms(self.first_words_p90_ms)
            ),
            format!(
                "On screen before the person began replying: {} of {} wanted",
                self.ahead, self.wanted
            ),
            format!(
                "Match with what the person said: {}",
                self.matched
                    .map_or_else(|| "none".into(), |score| format!("{score:.1} of 5"))
            ),
        ]
    }

    const BOARD_HEADER: &'static str = "when,recording,minutes,suggestions,unwanted,missed,first_words_median_ms,first_words_p90_ms,ahead,wanted,match";

    fn board_row(&self, when: &str, recording: &str) -> String {
        let number = |value: Option<u64>| value.map_or_else(String::new, |value| value.to_string());
        format!(
            "{when},{},{:.1},{},{},{},{},{},{},{},{}",
            recording.replace(',', " "),
            self.minutes,
            self.suggestions,
            self.unwanted,
            self.missed,
            number(self.first_words_median_ms),
            number(self.first_words_p90_ms),
            self.ahead,
            self.wanted,
            self.matched
                .map_or_else(String::new, |score| format!("{score:.2}"))
        )
    }
}

struct ScorePlatform {
    mine: Arc<Vec<f32>>,
    theirs: Arc<Vec<f32>>,
    began: Arc<OnceLock<Instant>>,
}

impl Platform for ScorePlatform {
    fn start_audio(
        &self,
        _microphone: bool,
        _system_audio: bool,
        sink: AudioSink,
    ) -> Result<Box<dyn AudioControl>, String> {
        ReplayAudio::start_lanes(
            vec![
                Lane {
                    speaker: Speaker::Me,
                    samples: self.mine.to_vec(),
                },
                Lane {
                    speaker: Speaker::Them,
                    samples: self.theirs.to_vec(),
                },
            ],
            Arc::clone(&self.began),
            sink,
        )
        .map(|replay| Box::new(replay) as Box<dyn AudioControl>)
    }

    fn capture_screen(&self) -> Result<Vec<u8>, String> {
        Err("no screen while scoring".into())
    }

    fn capture_call_window(&self) -> Result<CallShot, String> {
        Err("no screen while scoring".into())
    }

    fn meeting_signal(&self) -> Option<String> {
        None
    }

    fn copy_text(&self, _text: &str) {}

    fn open(&self, _target: &str) {}
}

/// Cuts the part of `recording` to score into 16 kHz mono speech, as a WAV
/// to play and a small MP3 to upload.
fn prepare_audio(
    recording: &Path,
    from: f64,
    minutes: f64,
    wav: &Path,
    mp3: &Path,
) -> anyhow::Result<()> {
    for (target, extra) in [(wav, &[][..]), (mp3, &["-b:a", "48k"][..])] {
        if target.exists() {
            continue;
        }
        let output = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-ss"])
            .arg(format!("{:.3}", from * 60.0))
            .arg("-t")
            .arg(format!("{:.3}", minutes * 60.0))
            .arg("-i")
            .arg(recording)
            .args(["-vn", "-ac", "1", "-ar", "16000"])
            .args(extra)
            .arg(target)
            .output()
            .map_err(|error| {
                anyhow::anyhow!("ffmpeg is needed to read recordings and did not start: {error}")
            })?;
        anyhow::ensure!(
            output.status.success(),
            "ffmpeg could not read {}: {}",
            recording.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn label_speakers(key: &str, mp3: &Path) -> anyhow::Result<String> {
    let audio = std::fs::read(mp3)?;
    let file = reqwest::blocking::multipart::Part::bytes(audio)
        .file_name("recording.mp3")
        .mime_str("audio/mpeg")?;
    let form = reqwest::blocking::multipart::Form::new()
        .part("file", file)
        .text("model", SPEAKER_MODEL)
        .text("response_format", "diarized_json")
        .text("chunking_strategy", "auto");
    let response = providers::http_client()
        .post(TRANSCRIPTION_ENDPOINT)
        .bearer_auth(key)
        .timeout(Duration::from_secs(900))
        .multipart(form)
        .send()?;
    let status = response.status();
    let body = response.text().unwrap_or_default();
    anyhow::ensure!(
        status.is_success(),
        "OpenAI could not label the speakers (HTTP {}): {}",
        status.as_u16(),
        clip(&body, 300)
    );
    Ok(body)
}

/// Scores one recording. Returns the number of problems the session
/// reported.
pub(crate) fn run(options: &Options) -> anyhow::Result<usize> {
    let paths = crate::AppPaths::from_environment()?;
    let _log = crate::init_file_logging(&paths.logs, "copilot-score.log")?;
    let saved = store::load(&store::settings_path(&paths.config_file));
    let dictation_key = agentdictate_runtime::load_settings(&paths.config_file)
        .map(|dictation| dictation.openai_api_key.trim().to_owned())
        .unwrap_or_default();
    let key = if saved.openai_api_key.trim().is_empty() {
        dictation_key.clone()
    } else {
        saved.openai_api_key.trim().to_owned()
    };
    anyhow::ensure!(
        !key.is_empty(),
        "Scoring needs an OpenAI API key, to label who speaks when."
    );
    anyhow::ensure!(
        options.recording.is_file(),
        "{} is not a file",
        options.recording.display()
    );
    let minutes = options
        .minutes
        .unwrap_or(DEFAULT_MINUTES)
        .clamp(0.5, MAX_MINUTES);
    let from = options.from.max(0.0);

    // Prepared audio and speaker labels are kept, so a second run of the
    // same part costs only the session itself.
    let work = paths.cache.join("score");
    std::fs::create_dir_all(&work)?;
    let size = std::fs::metadata(&options.recording)?.len();
    let digest = Sha256::digest(
        format!(
            "{}\0{size}\0{from:.3}\0{minutes:.3}",
            options.recording.display()
        )
        .as_bytes(),
    );
    let stem: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    let wav = work.join(format!("{stem}.wav"));
    let mp3 = work.join(format!("{stem}.mp3"));
    let labels = work.join(format!("{stem}-speakers.json"));
    prepare_audio(&options.recording, from, minutes, &wav, &mp3)?;
    let samples = read_speech(&wav).map_err(anyhow::Error::msg)?;
    let seconds = samples.len() as f64 / f64::from(SAMPLE_RATE);
    anyhow::ensure!(seconds > 5.0, "that part of the recording is empty");

    let body = match std::fs::read_to_string(&labels) {
        Ok(body) => body,
        Err(_) => {
            println!("Labelling who speaks when…");
            let body = label_speakers(&key, &mp3)?;
            std::fs::write(&labels, &body)?;
            body
        }
    };
    let segments = parse_segments(&body).map_err(anyhow::Error::msg)?;
    let heard = speakers(&segments);
    anyhow::ensure!(
        heard.len() >= 2,
        "only one speaker was found; scoring needs a conversation"
    );
    let me = match &options.me {
        Some(me) => heard
            .iter()
            .map(|(speaker, ..)| speaker)
            .find(|speaker| speaker.eq_ignore_ascii_case(me))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no speaker is labelled {me}"))?,
        None => heard[0].0.clone(),
    };
    println!("Speakers:");
    for (speaker, spoken, first) in &heard {
        println!(
            "  {speaker}{} {:.0} s: {}",
            if *speaker == me { " (you)" } else { "" },
            spoken,
            clip(first, 80)
        );
    }
    if options.me.is_none() {
        println!("  The most talkative speaker plays you; choose another with --me=<label>.");
    }
    let (mine, theirs) = split(&samples, &segments, &me);

    let mut settings = CopilotSettings {
        save_notes: false,
        live_insights: false,
        web_search: false,
        screen_context: false,
        auto_answer: true,
        // Someone else's conversation: nothing of the user's own applies.
        brain_source: BrainSource::default(),
        profile: String::new(),
        custom_instructions: String::new(),
        use_past_meetings: false,
        translate_to: String::new(),
        user_name: String::new(),
        meeting_goal: String::new(),
        calendar_ics: String::new(),
        webhook_url: String::new(),
        auto_start: false,
        ..saved
    };
    settings.capture_microphone = true;
    settings.capture_system_audio = true;
    let began: Arc<OnceLock<Instant>> = Arc::default();
    let mut services = session_services(
        &paths,
        &settings,
        // Settings the session adopts stay out of the user's own file.
        paths.cache.join("score-copilot.json"),
        Arc::new(ScorePlatform {
            mine: Arc::new(mine),
            theirs: Arc::new(theirs),
            began: Arc::clone(&began),
        }),
    )?;
    let no_documents = work.join("no-documents");
    std::fs::create_dir_all(&no_documents)?;
    services.knowledge_dir = options.documents.clone().unwrap_or(no_documents);
    services.account_name = String::new();
    println!(
        "Playing {:.1} min of {} with {} answers ({} judging, {} instant)…",
        seconds / 60.0,
        options.recording.display(),
        settings.provider.label(),
        settings.provider.default_fast_model(),
        settings.fast_model()
    );

    let events: Arc<Mutex<Vec<(Instant, CopilotEvent)>>> = Arc::default();
    let ui = {
        let events = Arc::clone(&events);
        Arc::new(move |event: CopilotEvent| {
            events.lock().unwrap().push((Instant::now(), event));
        })
    };
    let (sender, incoming) = mpsc::channel::<EngineMsg>();
    let grader_settings = settings.clone();
    let engine = Engine::new(services, settings, ui, sender.clone());
    let worker = std::thread::spawn(move || engine.run(incoming));
    std::thread::sleep(Duration::from_millis(2_500) + Duration::from_secs_f64(seconds) + TAIL);
    let _ = sender.send(EngineMsg::Action(CopilotAction::End));
    let _ = worker.join();

    let events = std::mem::take(&mut *events.lock().unwrap());
    let began = *began
        .get()
        .ok_or_else(|| anyhow::anyhow!("the recording never started playing"))?;
    let problems: Vec<&String> = events
        .iter()
        .filter_map(|(_, event)| match event {
            CopilotEvent::Problem(Some(problem)) => Some(problem),
            _ => None,
        })
        .collect();
    for problem in &problems {
        println!("  problem: {problem}");
    }
    let shown = suggestions(&events, began);
    let replies: Vec<Option<(f64, String)>> = shown
        .iter()
        .map(|suggestion| reply_after(&segments, &me, suggestion.asked))
        .collect();

    println!("Grading {} suggestion(s)…", shown.len());
    let offset = from * 60.0;
    let request = AnswerRequest {
        system: GRADER_SYSTEM.to_owned(),
        user: grader_prompt(&transcript(&segments, &me, offset), &shown, offset),
        image_jpeg: None,
        model: grader_settings.answer_model(),
        effort: CopilotEffort::Low,
        web_search: false,
        max_output_tokens: 4_000,
        cache_key: format!("score-{stem}"),
    };
    let chatgpt = Arc::new(ChatGptCredentials::discover());
    let graded = providers::provider_for(&grader_settings, &chatgpt, &dictation_key, &[])
        .and_then(|provider| {
            providers::complete(provider.as_ref(), &request, &AtomicBool::new(false))
        })
        .map_err(anyhow::Error::msg)?;
    let grades = parse_grades(&graded, shown.len())
        .ok_or_else(|| anyhow::anyhow!("the grader's reply was not readable"))?;
    let totals = totals(seconds / 60.0, &shown, &replies, &grades);

    let name = options.recording.file_stem().map_or_else(
        || "recording".into(),
        |stem| stem.to_string_lossy().into_owned(),
    );
    let when = chrono::Local::now();
    let mut report = format!(
        "# {name}\n\n{} · minutes {:.0} to {:.0} · you are speaker {me}\n\n",
        when.format("%Y-%m-%d %H:%M"),
        from,
        from + seconds / 60.0
    );
    for line in totals.lines() {
        report.push_str(&format!("- {line}\n"));
    }
    report.push_str("\n## Suggestions\n");
    for (index, suggestion) in shown.iter().enumerate() {
        let grade = grades.suggestions.get(index).and_then(Option::as_ref);
        let wait = suggestion.shown.map_or_else(
            || "never".into(),
            |shown| format!("{:.1} s", (shown - suggestion.asked).max(0.0)),
        );
        report.push_str(&format!(
            "\n### {}. {} — {}\n\n- **Asked:** {}\n- **Suggested after {wait}:** {}\n",
            index + 1,
            clock(suggestion.asked + offset),
            match grade {
                Some(grade) if grade.warranted => format!(
                    "wanted, match {}",
                    grade
                        .matched
                        .map_or_else(|| "?".into(), |score| format!("{score}/5"))
                ),
                Some(_) => "unwanted".to_owned(),
                None => "not graded".to_owned(),
            },
            clip(&suggestion.question, 300),
            clip(&suggestion.line(), 500)
        ));
        if let Some((replied, text)) = &replies[index] {
            let lead = suggestion.shown.map_or_else(String::new, |shown| {
                if shown <= *replied {
                    format!(", {:.1} s after the suggestion", replied - shown)
                } else {
                    format!(", {:.1} s before the suggestion", shown - replied)
                }
            });
            report.push_str(&format!(
                "- **They said** (from {}{lead}): {text}\n",
                clock(replied + offset)
            ));
        }
        if let Some(note) = grade
            .map(|grade| grade.note.trim())
            .filter(|note| !note.is_empty())
        {
            report.push_str(&format!("- **Grader:** {note}\n"));
        }
    }
    report.push_str("\n## Missed questions\n\n");
    if grades.missed.is_empty() {
        report.push_str("None.\n");
    }
    for (at, question) in &grades.missed {
        report.push_str(&format!("- {at}: {question}\n"));
    }

    // Beside the labelled meetings, which also never leave this machine.
    let scores = paths
        .cache
        .parent()
        .unwrap_or(&paths.cache)
        .join("eval")
        .join("scores");
    std::fs::create_dir_all(&scores)?;
    let report_path = scores.join(format!("{name}-{}.md", when.format("%Y%m%d-%H%M%S")));
    std::fs::write(&report_path, &report)?;
    let board = scores.join("board.csv");
    let mut rows =
        std::fs::read_to_string(&board).unwrap_or_else(|_| format!("{}\n", Totals::BOARD_HEADER));
    rows.push_str(&totals.board_row(&when.format("%Y-%m-%d %H:%M").to_string(), &name));
    rows.push('\n');
    std::fs::write(&board, rows)?;

    println!();
    for line in totals.lines() {
        println!("{line}");
    }
    println!("Report: {}", report_path.display());
    println!("Board:  {}", board.display());
    Ok(problems.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(speaker: &str, start: f64, end: f64, text: &str) -> Segment {
        Segment {
            speaker: speaker.into(),
            start,
            end,
            text: text.into(),
        }
    }

    fn call() -> Vec<Segment> {
        vec![
            segment("A", 0.0, 2.0, "Thanks for joining."),
            segment("B", 2.5, 3.0, "Happy to."),
            segment("A", 3.5, 6.0, "What does the Growth plan cost?"),
            segment("B", 6.4, 9.0, "Forty-nine dollars a seat."),
            segment("B", 9.2, 12.0, "Billed yearly."),
            segment("A", 12.5, 13.0, "Great."),
        ]
    }

    #[test]
    fn labelled_speech_is_read_in_order_and_empty_stretches_dropped() {
        let body = r#"{"text":"…","segments":[
            {"speaker":"B","start":2.5,"end":3.0,"text":" Happy to. "},
            {"speaker":"A","start":0.0,"end":2.0,"text":"Thanks for joining."},
            {"speaker":"A","start":3.0,"end":3.0,"text":"uh"},
            {"speaker":"A","start":4.0,"end":5.0,"text":"  "}]}"#;
        assert_eq!(
            parse_segments(body).unwrap(),
            [
                segment("A", 0.0, 2.0, "Thanks for joining."),
                segment("B", 2.5, 3.0, "Happy to.")
            ]
        );
        assert!(parse_segments("{}").is_err());
    }

    #[test]
    fn the_most_talkative_speaker_comes_first() {
        let heard = speakers(&call());
        assert_eq!(heard[0].0, "B");
        assert_eq!(heard[0].2, "Happy to.");
        assert!((heard[0].1 - 5.9).abs() < 1e-6);
        assert_eq!(heard[1].0, "A");
    }

    #[test]
    fn each_side_gets_only_its_own_speech() {
        let rate = SAMPLE_RATE as usize;
        let samples = vec![1.0; 13 * rate];
        let (mine, theirs) = split(&samples, &call(), "B");
        assert_eq!(mine.len(), samples.len());
        // B answers from 6.4 s to 12 s; A asks from 3.5 s to 6 s.
        assert_eq!(mine[7 * rate], 1.0);
        assert_eq!(theirs[7 * rate], 0.0);
        assert_eq!(theirs[4 * rate], 1.0);
        assert_eq!(mine[4 * rate], 0.0);
        // No sample plays on both sides.
        assert!(mine.iter().zip(&theirs).all(|(a, b)| a * b == 0.0));
    }

    #[test]
    fn a_suggestion_is_timed_from_the_end_of_their_speech_to_its_first_words() {
        let began = Instant::now();
        let at = |seconds: f64| began + Duration::from_secs_f64(seconds);
        let events = vec![
            (
                at(6.0),
                CopilotEvent::Speaking {
                    speaker: Speaker::Them,
                    active: false,
                },
            ),
            (
                at(6.9),
                CopilotEvent::AnswerStarted {
                    id: 4,
                    title: "What does the Growth plan cost?".into(),
                    used_screen: false,
                    model: "gpt-6-sol".into(),
                    automatic: true,
                },
            ),
            (
                at(7.0),
                CopilotEvent::InstantDelta {
                    id: 4,
                    text: "It's **$49** a seat.".into(),
                },
            ),
            (
                at(7.2),
                CopilotEvent::InstantDelta {
                    id: 4,
                    text: "\n[from: mine]".into(),
                },
            ),
            (
                at(9.0),
                CopilotEvent::AnswerDelta {
                    id: 4,
                    text: "Longer answer".into(),
                },
            ),
            // An answer the user asked for is not a suggestion.
            (
                at(10.0),
                CopilotEvent::AnswerStarted {
                    id: 5,
                    title: "Asked".into(),
                    used_screen: false,
                    model: "gpt-6-sol".into(),
                    automatic: false,
                },
            ),
        ];
        let shown = suggestions(&events, began);
        assert_eq!(shown.len(), 1);
        assert!((shown[0].asked - 6.0).abs() < 1e-6);
        assert!((shown[0].shown.unwrap() - 7.0).abs() < 1e-6);
        assert_eq!(shown[0].line(), "It's **$49** a seat.");

        let reply = reply_after(&call(), "B", shown[0].asked).unwrap();
        assert!((reply.0 - 6.4).abs() < 1e-6);
        assert_eq!(reply.1, "Forty-nine dollars a seat. Billed yearly.");
        assert!(reply_after(&call(), "B", 12.9).is_none());
    }

    #[test]
    fn grades_become_totals() {
        let shown = vec![
            Suggestion {
                question: "What does it cost?".into(),
                asked: 6.0,
                shown: Some(7.0),
                instant: "It's $49.".into(),
                ..Suggestion::default()
            },
            Suggestion {
                question: "Great.".into(),
                asked: 13.0,
                shown: Some(15.5),
                instant: "Thanks.".into(),
                ..Suggestion::default()
            },
        ];
        let replies = vec![Some((7.4, "Forty-nine.".to_owned())), None];
        let grades = parse_grades(
            r#"Here: {"suggestions":[{"n":1,"warranted":true,"match":4,"note":"same figure"},
                {"n":2,"warranted":false,"match":1,"note":"not a question"},
                {"n":9,"warranted":true,"match":5}],
                "missed":[{"at":"0:20","question":"And the timeline?"}]}"#,
            shown.len(),
        )
        .unwrap();
        assert_eq!(grades.suggestions[0].as_ref().unwrap().matched, Some(4));
        let totals = totals(10.0, &shown, &replies, &grades);
        assert_eq!(
            (totals.suggestions, totals.unwanted, totals.missed),
            (2, 1, 1)
        );
        assert_eq!((totals.ahead, totals.wanted), (1, 1));
        assert_eq!(totals.matched, Some(4.0));
        assert_eq!(totals.first_words_median_ms, Some(2_500));
        assert!(totals.lines()[1].contains("1.0 per 10 min"));
        assert_eq!(
            totals.board_row("2026-11-10 14:00", "sales, call"),
            "2026-11-10 14:00,sales  call,10.0,2,1,1,2500,2500,1,1,4.00"
        );
        assert!(parse_grades("no json", 2).is_none());
    }

    #[test]
    fn the_grader_sees_the_call_with_times_and_who_spoke() {
        let text = transcript(&call(), "B", 120.0);
        assert!(text.starts_with("[2:00] Them (A): Thanks for joining.\n[2:02] Me: Happy to."));
        let prompt = grader_prompt(&text, &[], 0.0);
        assert!(prompt.contains("(none)"));
    }
}
