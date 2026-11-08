//! `agentdictate copilot --questions`: scores which of the other side's lines
//! the copilot would answer against hand-labelled calls.
//!
//! Each call is replayed line by line through the same turn rules and judge
//! prompt the engine uses, with the live judge model. A line counts as
//! answered when the judge says to respond and the speaker does not keep
//! talking before the answer would appear. Scripted calls ship with the app;
//! labelled real meetings can be passed as files and stay on this machine.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use agentdictate_core::{
    CopilotEffort, CopilotMode, HeardSpan, JUDGE_TRANSCRIPT_CHARS, QUESTION_PAUSE_MS, Situation,
    Speaker, TranscriptLine, TurnContext, VoiceNames, judge_transcript, kept_talking,
    only_listening, parse_turn_verdict, render_transcript, since_user_spoke_ms, turn_candidate,
    turn_judge_system_prompt, turn_judge_user_prompt, turn_likely_question, utterance_similarity,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    providers::{self, AnswerRequest},
    store,
};
use crate::codex_subscription::ChatGptCredentials;

pub(super) const SCRIPTED: &str = include_str!("question_eval.json");
/// Student situations: office hours, study groups, lectures, exams, and a
/// podcast playing in the background.
pub(super) const STUDENTS: &str = include_str!("question_eval_students.json");
/// Matches the engine: verdicts slower than this fall back to the wording.
const VERDICT_WAIT_MS: u64 = 3_000;
/// Matches the engine: automatic answers this close together are suppressed.
const AUTO_ANSWER_COOLDOWN_MS: u64 = 6_000;
pub(super) const PARALLEL_JUDGES: usize = 3;
/// Rate-limited judge calls are retried after these pauses.
pub(super) const RETRY_PAUSES_MS: [u64; 4] = [2_000, 5_000, 10_000, 20_000];

#[derive(Deserialize)]
pub(super) struct EvalCall {
    pub(super) meeting: String,
    /// The meeting type the user would pick; answers follow its guidance.
    #[serde(default)]
    pub(super) mode: CopilotMode,
    lines: Vec<EvalLine>,
}

#[derive(Deserialize)]
struct EvalLine {
    t_s: f64,
    #[serde(default)]
    end_s: Option<f64>,
    speaker: String,
    text: String,
    #[serde(default)]
    label: Option<EvalLabel>,
}

#[derive(Deserialize, Clone, Default)]
pub(super) struct EvalLabel {
    #[serde(default)]
    pub(super) worth_answering: bool,
    #[serde(default)]
    aimed_at_user: bool,
    #[serde(default)]
    pub(super) ambiguous: bool,
    #[serde(default)]
    why: String,
}

pub(super) struct Line {
    pub(super) speaker: Speaker,
    pub(super) voice: Option<u8>,
    pub(super) text: String,
    pub(super) start_ms: u64,
    pub(super) end_ms: u64,
    pub(super) label: Option<EvalLabel>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    NotJudged,
    JudgedNo,
    KeptTalking,
    Cooldown,
    Answered,
}

impl Outcome {
    const fn describe(self) -> &'static str {
        match self {
            Self::NotJudged => "not judged",
            Self::JudgedNo => "judge said no",
            Self::KeptTalking => "kept talking",
            Self::Cooldown => "cooldown",
            Self::Answered => "answered",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy)]
struct Verdict {
    respond: Option<bool>,
    elapsed_ms: u64,
}

#[derive(Default)]
struct Tally {
    true_positive: usize,
    false_positive: usize,
    false_negative: usize,
    aimed_answered: usize,
    answered: usize,
    minutes: f64,
}

impl Tally {
    fn add(&mut self, other: &Self) {
        self.true_positive += other.true_positive;
        self.false_positive += other.false_positive;
        self.false_negative += other.false_negative;
        self.aimed_answered += other.aimed_answered;
        self.answered += other.answered;
        self.minutes += other.minutes;
    }

    fn summary(&self) -> String {
        let precision = ratio(self.true_positive, self.true_positive + self.false_positive);
        let recall = ratio(self.true_positive, self.true_positive + self.false_negative);
        let f_half = if precision + recall > 0.0 {
            1.25 * precision * recall / (0.25 * precision + recall)
        } else {
            0.0
        };
        format!(
            "precision {:.0}% recall {:.0}% F0.5 {:.2} | TP {} FP {} FN {} | {:.1} false pop-ups per 10 min | aimed at user {}/{}",
            precision * 100.0,
            recall * 100.0,
            f_half,
            self.true_positive,
            self.false_positive,
            self.false_negative,
            if self.minutes > 0.0 {
                self.false_positive as f64 / self.minutes * 10.0
            } else {
                0.0
            },
            self.aimed_answered,
            self.answered
        )
    }
}

fn ratio(part: usize, whole: usize) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64
    }
}

pub(super) fn load_calls(text: &str) -> anyhow::Result<Vec<EvalCall>> {
    if text.trim_start().starts_with('[') {
        Ok(serde_json::from_str(text)?)
    } else {
        Ok(vec![serde_json::from_str(text)?])
    }
}

pub(super) fn prepare(call: &EvalCall) -> (Vec<Line>, VoiceNames) {
    let mut voices: BTreeMap<String, u8> = BTreeMap::new();
    let mut names = VoiceNames::new();
    let mut lines: Vec<Line> = call
        .lines
        .iter()
        .map(|line| {
            let me = line.speaker.eq_ignore_ascii_case("me");
            let voice = (!me).then(|| {
                let next = voices.len() as u8;
                let voice = *voices.entry(line.speaker.clone()).or_insert(next);
                if !line.speaker.eq_ignore_ascii_case("them") {
                    names.insert(voice, line.speaker.clone());
                }
                voice
            });
            Line {
                speaker: if me { Speaker::Me } else { Speaker::Them },
                voice,
                text: line.text.trim().to_owned(),
                start_ms: (line.t_s * 1_000.0) as u64,
                end_ms: line.end_s.map_or(0, |end| (end * 1_000.0) as u64),
                label: line.label.clone(),
            }
        })
        .collect();
    lines.sort_by_key(|line| line.start_ms);
    // Without end times, estimate from the words, ending before the next line.
    for index in 0..lines.len() {
        if lines[index].end_ms > lines[index].start_ms {
            continue;
        }
        let words = lines[index].text.split_whitespace().count() as u64;
        let spoken = lines[index].start_ms + 300 + words * 350;
        let next = lines.get(index + 1).map_or(u64::MAX, |next| {
            next.start_ms.max(lines[index].start_ms + 300)
        });
        lines[index].end_ms = spoken.min(next);
    }
    (lines, names)
}

/// The transcript as the engine holds it when line `index` ends.
fn transcript_until(lines: &[Line], index: usize) -> Vec<TranscriptLine> {
    lines[..=index]
        .iter()
        .enumerate()
        .map(|(id, line)| TranscriptLine {
            id: id as u64,
            speaker: line.speaker,
            text: line.text.clone(),
            at_ms: line.start_ms,
            voice: line.voice,
        })
        .collect()
}

fn judge_prompt(lines: &[Line], names: &VoiceNames, index: usize) -> String {
    let now_ms = lines[index].end_ms;
    let transcript = transcript_until(lines, index);
    let me_last_spoke_secs =
        since_user_spoke_ms(&transcript[..index], now_ms).map(|since| since / 1_000);
    turn_judge_user_prompt(
        &render_transcript(
            &judge_transcript(&transcript),
            names,
            JUDGE_TRANSCRIPT_CHARS,
        ),
        &lines[index].text,
        TurnContext {
            situation: Situation::Unclear,
            me_last_spoke_secs,
        },
    )
}

pub(super) fn cache_key(model: &str, system: &str, user: &str) -> String {
    let digest = Sha256::digest(format!("{model}\0{system}\0{user}").as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Replays labelled calls and prints how well answered lines match the
/// labels. Returns the number of failed judge calls.
pub(crate) fn run(files: &[PathBuf], fresh: bool, samples: usize) -> anyhow::Result<usize> {
    let samples = samples.max(1);
    let paths = crate::AppPaths::from_environment()?;
    let mut settings = store::load(&store::settings_path(&paths.config_file));
    // The engine judges with the provider's fast model whatever drafts answers.
    let model = settings.provider.default_fast_model().to_owned();
    settings.set_answer_model(model.clone());
    let chatgpt = Arc::new(ChatGptCredentials::discover());
    let cache_path = paths.cache.join("question-eval-verdicts.json");
    let cache: HashMap<String, Verdict> = if fresh {
        HashMap::new()
    } else {
        std::fs::read_to_string(&cache_path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    };
    let cache = Mutex::new(cache);

    let mut sets: Vec<(String, Vec<EvalCall>)> = vec![
        ("scripted".into(), load_calls(SCRIPTED)?),
        ("students".into(), load_calls(STUDENTS)?),
    ];
    for file in files {
        let text = std::fs::read_to_string(file)
            .map_err(|error| anyhow::anyhow!("{}: {error}", file.display()))?;
        sets.push((label_for(file), load_calls(&text)?));
    }
    println!(
        "Question evaluation with {} {model}, {samples} sample(s) per line{}",
        settings.provider.label(),
        if fresh { " (fresh verdicts)" } else { "" }
    );

    let system = turn_judge_system_prompt();
    let exhausted = AtomicBool::new(false);
    let save = |cache: &Mutex<HashMap<String, Verdict>>| {
        if let Ok(text) = serde_json::to_string(&*cache.lock().unwrap()) {
            let _ = std::fs::write(&cache_path, text);
        }
    };
    let mut failures = 0;
    let mut calls = 0;
    let mut latencies = Vec::new();
    let mut overall = Tally::default();
    for (set, eval_calls) in &sets {
        let mut set_tally = Tally::default();
        for call in eval_calls {
            let (lines, names) = prepare(call);
            let candidates: Vec<usize> = (0..lines.len())
                .filter(|&index| {
                    lines[index].speaker == Speaker::Them
                        && turn_candidate(&lines[index].text)
                        && !only_listening(&transcript_until(&lines, index), lines[index].end_ms)
                })
                .collect();
            for sample in 0..samples {
                let jobs: Vec<(usize, String, String)> = candidates
                    .iter()
                    .map(|&index| {
                        let user = judge_prompt(&lines, &names, index);
                        let key = cache_key(&model, system, &user);
                        let key = if sample == 0 {
                            key
                        } else {
                            format!("{key}#{sample}")
                        };
                        (index, key, user)
                    })
                    .collect();
                let verdicts: Mutex<HashMap<usize, Verdict>> = Mutex::new(HashMap::new());
                let next = Mutex::new(0_usize);
                std::thread::scope(|scope| {
                    for _ in 0..PARALLEL_JUDGES {
                        scope.spawn(|| {
                            loop {
                                let job = {
                                    let mut next = next.lock().unwrap();
                                    let job = jobs.get(*next);
                                    *next += 1;
                                    job
                                };
                                let Some((index, key, user)) = job else { break };
                                if exhausted.load(Ordering::Relaxed) {
                                    break;
                                }
                                if let Some(verdict) = cache.lock().unwrap().get(key).copied() {
                                    verdicts.lock().unwrap().insert(*index, verdict);
                                    continue;
                                }
                                let request = AnswerRequest {
                                    system: system.to_owned(),
                                    user: user.clone(),
                                    image_jpeg: None,
                                    model: model.clone(),
                                    effort: CopilotEffort::Low,
                                    web_search: false,
                                    max_output_tokens: 200,
                                    cache_key: format!("question-eval-{}", call.meeting),
                                };
                                let mut started = Instant::now();
                                let attempt = || {
                                    providers::provider_for(&settings, &chatgpt, "", &[]).and_then(
                                        |provider| {
                                            providers::complete(
                                                provider.as_ref(),
                                                &request,
                                                &AtomicBool::new(false),
                                            )
                                        },
                                    )
                                };
                                let mut respond = attempt();
                                for pause in RETRY_PAUSES_MS {
                                    if !respond
                                        .as_ref()
                                        .is_err_and(|error| error.contains("rate limit"))
                                    {
                                        break;
                                    }
                                    std::thread::sleep(std::time::Duration::from_millis(pause));
                                    started = Instant::now();
                                    respond = attempt();
                                }
                                let verdict = Verdict {
                                    respond: respond.as_deref().ok().and_then(parse_turn_verdict),
                                    elapsed_ms: started.elapsed().as_millis() as u64,
                                };
                                if respond.is_ok() {
                                    cache.lock().unwrap().insert(key.clone(), verdict);
                                } else if let Err(error) = &respond {
                                    eprintln!("judge failed: {error}");
                                    if error.contains("usage limit") {
                                        exhausted.store(true, Ordering::Relaxed);
                                    }
                                }
                                verdicts.lock().unwrap().insert(*index, verdict);
                            }
                        });
                    }
                });
                save(&cache);
                anyhow::ensure!(
                    !exhausted.load(Ordering::Relaxed),
                    "the plan's usage limit was reached; verdicts so far are saved, run again later"
                );
                let verdicts = verdicts.into_inner().unwrap();
                calls += verdicts.len();
                failures += verdicts.values().filter(|v| v.respond.is_none()).count();
                latencies.extend(verdicts.values().map(|verdict| verdict.elapsed_ms));

                let mut outcomes = vec![Outcome::NotJudged; lines.len()];
                let mut last_answer: Option<(u64, String)> = None;
                for &index in &candidates {
                    let line = &lines[index];
                    let verdict = verdicts.get(&index).copied().unwrap_or(Verdict {
                        respond: None,
                        elapsed_ms: VERDICT_WAIT_MS,
                    });
                    let respond = match verdict.respond {
                        Some(respond) if verdict.elapsed_ms <= VERDICT_WAIT_MS => respond,
                        _ => turn_likely_question(&line.text),
                    };
                    let decided_ms = line.end_ms + QUESTION_PAUSE_MS.max(verdict.elapsed_ms);
                    let resumed = lines[index + 1..]
                        .iter()
                        .filter(|later| {
                            later.speaker == Speaker::Them && later.start_ms <= decided_ms
                        })
                        .any(|later| {
                            kept_talking(
                                line.end_ms,
                                line.voice,
                                &HeardSpan {
                                    start_ms: later.start_ms,
                                    end_ms: later.end_ms,
                                    voice: later.voice,
                                    text: Some(&later.text),
                                },
                            )
                        });
                    outcomes[index] = if !respond {
                        Outcome::JudgedNo
                    } else if resumed {
                        Outcome::KeptTalking
                    } else if last_answer.as_ref().is_some_and(|(at, previous)| {
                        decided_ms.saturating_sub(*at) < AUTO_ANSWER_COOLDOWN_MS
                            || utterance_similarity(previous, &line.text) >= 0.6
                    }) {
                        Outcome::Cooldown
                    } else {
                        last_answer = Some((decided_ms, line.text.clone()));
                        Outcome::Answered
                    };
                }

                let mut tally = Tally {
                    minutes: lines
                        .last()
                        .map_or(0.0, |line| line.end_ms as f64 / 60_000.0),
                    ..Tally::default()
                };
                let mut errors = Vec::new();
                for (index, line) in lines.iter().enumerate() {
                    let Some(label) = line.label.as_ref().filter(|label| !label.ambiguous) else {
                        continue;
                    };
                    let answered = outcomes[index] == Outcome::Answered;
                    if answered {
                        tally.answered += 1;
                        tally.aimed_answered += usize::from(label.aimed_at_user);
                    }
                    match (label.worth_answering, answered) {
                        (true, true) => tally.true_positive += 1,
                        (false, true) => {
                            tally.false_positive += 1;
                            errors.push(format!("FP  {} ({})", clip(&line.text), label.why));
                        }
                        (true, false) => {
                            tally.false_negative += 1;
                            errors.push(format!(
                                "FN  {} [{}] ({})",
                                clip(&line.text),
                                outcomes[index].describe(),
                                label.why
                            ));
                        }
                        (false, false) => {}
                    }
                }
                println!(
                    "  {set} / {} #{sample}: TP {} FP {} FN {}",
                    call.meeting, tally.true_positive, tally.false_positive, tally.false_negative
                );
                if sample == 0 {
                    for error in errors {
                        println!("      {error}");
                    }
                }
                set_tally.add(&tally);
            }
        }
        println!("{set}: {}", set_tally.summary());
        overall.add(&set_tally);
    }
    println!("OVERALL: {}", overall.summary());
    latencies.sort_unstable();
    if let Some(median) = latencies.get(latencies.len() / 2) {
        println!("judge calls {calls}, failed {failures}, median {median} ms");
    }
    save(&cache);
    Ok(failures)
}

pub(super) fn label_for(file: &Path) -> String {
    file.file_stem()
        .map_or_else(|| "file".into(), |stem| stem.to_string_lossy().into_owned())
}

pub(super) fn clip(text: &str) -> String {
    let clipped: String = text.chars().take(90).collect();
    if clipped.len() < text.len() {
        format!("\"{clipped}…\"")
    } else {
        format!("\"{clipped}\"")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripted_calls_parse_with_labels_on_the_other_side() {
        let mut calls = load_calls(SCRIPTED).expect("scripted calls parse");
        calls.extend(load_calls(STUDENTS).expect("student calls parse"));
        assert!(calls.len() >= 18);
        for call in &calls {
            let (lines, _) = prepare(call);
            assert!(lines.iter().all(|line| line.end_ms > line.start_ms));
            assert!(
                lines
                    .iter()
                    .filter(|line| line.speaker == Speaker::Them)
                    .all(|line| line.label.is_some()),
                "{} has an unlabelled line from the other side",
                call.meeting
            );
        }
        let positives = calls
            .iter()
            .flat_map(|call| &call.lines)
            .filter(|line| {
                line.label
                    .as_ref()
                    .is_some_and(|label| label.worth_answering)
            })
            .count();
        assert!(positives >= 40, "{positives}");
    }
}
