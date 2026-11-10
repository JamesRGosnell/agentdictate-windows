//! `agentdictate copilot --answers`: grades the instant drafts the copilot
//! writes for the questions in labelled calls.
//!
//! Every line labelled worth answering is replayed with the transcript up to
//! it, the same prompts the engine sends, and the live draft model. A
//! stronger model then grades each draft on whether the user could say it
//! straight away, invents nothing about them, helps, and reads at a glance.
//! The eval runs with an empty profile and no documents, so any specific
//! claim about the user's business or experience counts as invented.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::Instant,
};

use agentdictate_core::{
    CopilotEffort, CopilotSettings, INSTANT_GUIDANCE, Speaker, TranscriptLine,
    answer_system_prompt, answer_user_prompt, detected_question_instruction, render_transcript,
};
use serde::{Deserialize, Serialize};

use super::{
    providers::{self, AnswerRequest},
    question_eval::{
        Line, PARALLEL_JUDGES, RETRY_PAUSES_MS, SCRIPTED, STUDENTS, cache_key, clip, label_for,
        load_calls, prepare,
    },
    store,
};
use crate::codex_subscription::ChatGptCredentials;

/// Matches the engine's answer transcript budget.
const TRANSCRIPT_CHARS: usize = 24_000;

const GRADER_SYSTEM: &str = "You grade a live meeting copilot. The user (\"Me\") is mid-call, glances at the \
copilot's draft for a second or two, and says it aloud. The user's profile and documents are EMPTY, so the copilot \
knows nothing about Me, Me's company, product, prices, policies, certifications, or personal experiences beyond what \
the transcript says. Score the draft from 1 (bad) to 5 (excellent) on each criterion:\n\
- direct: it opens with a natural line Me can say right away that addresses exactly what was asked, with no preamble.\n\
- grounded: it states no specific fact about Me or Me's business that is not in the transcript, and its facts about the \
world are accurate. Where the answer depends on facts only Me knows, it gives Me a way to answer honestly (a bridge, a \
fill-in, a clarifying question) instead of inventing them. Any invented price, policy, certification, metric, or \
personal story scores 1.\n\
- useful: it makes Me sound sharper than Me would alone: specific to this conversation and this question, with real \
substance. Generic advice anyone could give scores low.\n\
- glanceable: the line to say comes first and the whole draft reads in a few seconds; padding, long bullets, or headings \
score low.\n\
Respond with ONLY one JSON object: {\"direct\": n, \"grounded\": n, \"useful\": n, \"glanceable\": n, \"issue\": \
\"the single biggest problem in at most 15 words, or an empty string\"}";

#[derive(Serialize, Deserialize, Clone, Default)]
struct Grade {
    direct: f64,
    grounded: f64,
    useful: f64,
    glanceable: f64,
    #[serde(default)]
    issue: String,
}

impl Grade {
    fn mean(&self) -> f64 {
        (self.direct + self.grounded + self.useful + self.glanceable) / 4.0
    }
}

#[derive(Serialize, Deserialize, Default)]
struct Cache {
    drafts: HashMap<String, Draft>,
    grades: HashMap<String, Grade>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Draft {
    text: String,
    elapsed_ms: u64,
}

struct Job {
    set: String,
    meeting: String,
    question: String,
    sample: usize,
    system: String,
    user: String,
    transcript: String,
}

#[derive(Default)]
struct Totals {
    count: usize,
    direct: f64,
    grounded: f64,
    useful: f64,
    glanceable: f64,
    words: usize,
}

impl Totals {
    fn add(&mut self, grade: &Grade, words: usize) {
        self.count += 1;
        self.direct += grade.direct;
        self.grounded += grade.grounded;
        self.useful += grade.useful;
        self.glanceable += grade.glanceable;
        self.words += words;
    }

    fn summary(&self) -> String {
        let n = self.count.max(1) as f64;
        format!(
            "overall {:.2} | direct {:.2} grounded {:.2} useful {:.2} glanceable {:.2} | {:.0} words avg | {} drafts",
            (self.direct + self.grounded + self.useful + self.glanceable) / (4.0 * n),
            self.direct / n,
            self.grounded / n,
            self.useful / n,
            self.glanceable / n,
            self.words as f64 / n,
            self.count
        )
    }
}

fn parse_grade(text: &str) -> Option<Grade> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    serde_json::from_str(text.get(start..=end)?).ok()
}

/// Calls the provider, retrying rate limits.
fn ask(
    settings: &CopilotSettings,
    chatgpt: &Arc<ChatGptCredentials>,
    request: &AnswerRequest,
) -> Result<(String, u64), String> {
    let mut started = Instant::now();
    let attempt = || {
        // Saved models the plan rejects fall back to the provider's defaults,
        // as the live copilot does once it learns which models work.
        let fallbacks = [
            settings.provider.default_instant_model().to_owned(),
            settings.provider.default_fast_model().to_owned(),
        ];
        providers::provider_for(settings, chatgpt, "", &fallbacks).and_then(|provider| {
            providers::complete(provider.as_ref(), request, &AtomicBool::new(false))
        })
    };
    let mut result = attempt();
    for pause in RETRY_PAUSES_MS {
        if !result
            .as_ref()
            .is_err_and(|error| error.contains("rate limit"))
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(pause));
        started = Instant::now();
        result = attempt();
    }
    result.map(|text| (text, started.elapsed().as_millis() as u64))
}

fn transcript_until(lines: &[Line], names: &agentdictate_core::VoiceNames, index: usize) -> String {
    let transcript: Vec<TranscriptLine> = lines[..=index]
        .iter()
        .enumerate()
        .map(|(id, line)| TranscriptLine {
            id: id as u64,
            speaker: line.speaker,
            text: line.text.clone(),
            at_ms: line.start_ms,
            voice: line.voice,
        })
        .collect();
    render_transcript(&transcript, names, TRANSCRIPT_CHARS)
}

/// Drafts and grades answers for every question worth answering. Returns the
/// number of failed calls.
pub(crate) fn run(files: &[PathBuf], fresh: bool, samples: usize) -> anyhow::Result<usize> {
    let samples = samples.max(1);
    let paths = crate::AppPaths::from_environment()?;
    let user_settings = store::load(&store::settings_path(&paths.config_file));
    // A neutral user: the eval measures the prompts, not this machine's profile.
    let mut base = CopilotSettings {
        provider: user_settings.provider,
        ..CopilotSettings::default()
    };
    let draft_model = user_settings.fast_model();
    let grader_model = user_settings.answer_model();
    base.set_answer_model(draft_model.clone());
    let mut grader_settings = base.clone();
    grader_settings.set_answer_model(grader_model.clone());
    let chatgpt = Arc::new(ChatGptCredentials::discover());
    let cache_path = paths.cache.join("answer-eval.json");
    let cache: Cache = if fresh {
        Cache::default()
    } else {
        std::fs::read_to_string(&cache_path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    };
    let cache = Mutex::new(cache);

    let mut sets = vec![
        ("scripted".to_owned(), load_calls(SCRIPTED)?),
        ("students".to_owned(), load_calls(STUDENTS)?),
    ];
    for file in files {
        let text = std::fs::read_to_string(file)
            .map_err(|error| anyhow::anyhow!("{}: {error}", file.display()))?;
        sets.push((label_for(file), load_calls(&text)?));
    }
    println!(
        "Answer evaluation: drafts by {draft_model}, graded by {grader_model}, {samples} sample(s) per question{}",
        if fresh { " (fresh)" } else { "" }
    );

    let mut jobs = Vec::new();
    for (set, calls) in &sets {
        for call in calls {
            let (lines, names) = prepare(call);
            let mut settings = base.clone();
            settings.mode = call.mode;
            let system = answer_system_prompt(&settings, "");
            for (index, line) in lines.iter().enumerate() {
                let worth = line
                    .label
                    .as_ref()
                    .is_some_and(|label| label.worth_answering && !label.ambiguous);
                if line.speaker != Speaker::Them || !worth {
                    continue;
                }
                let transcript = transcript_until(&lines, &names, index);
                let user = answer_user_prompt(
                    &transcript,
                    &detected_question_instruction(&line.text),
                    false,
                    "",
                );
                for sample in 0..samples {
                    jobs.push(Job {
                        set: set.clone(),
                        meeting: call.meeting.clone(),
                        question: line.text.clone(),
                        sample,
                        system: system.clone(),
                        user: format!("{user}\n\n{INSTANT_GUIDANCE}"),
                        transcript: transcript.clone(),
                    });
                }
            }
        }
    }

    let results: Mutex<Vec<Option<(Draft, Grade)>>> = Mutex::new(vec![None; jobs.len()]);
    let failures = Mutex::new(0_usize);
    let next = Mutex::new(0_usize);
    std::thread::scope(|scope| {
        for _ in 0..PARALLEL_JUDGES {
            scope.spawn(|| {
                loop {
                    let index = {
                        let mut next = next.lock().unwrap();
                        let index = *next;
                        *next += 1;
                        index
                    };
                    let Some(job) = jobs.get(index) else { break };
                    let draft_key = format!(
                        "{}#{}",
                        cache_key(&draft_model, &job.system, &job.user),
                        job.sample
                    );
                    let cached = cache.lock().unwrap().drafts.get(&draft_key).cloned();
                    let draft = match cached {
                        Some(draft) => draft,
                        None => {
                            let request = AnswerRequest {
                                system: job.system.clone(),
                                user: job.user.clone(),
                                image_jpeg: None,
                                model: draft_model.clone(),
                                effort: CopilotEffort::Low,
                                web_search: false,
                                max_output_tokens: 1024,
                                cache_key: format!("answer-eval-{}", job.meeting),
                            };
                            match ask(&base, &chatgpt, &request) {
                                Ok((text, elapsed_ms)) => {
                                    // Graded as the user sees it, without
                                    // the marker that says where its facts
                                    // come from.
                                    let text = agentdictate_core::split_grounding(&text).0;
                                    let draft = Draft { text, elapsed_ms };
                                    cache
                                        .lock()
                                        .unwrap()
                                        .drafts
                                        .insert(draft_key, draft.clone());
                                    draft
                                }
                                Err(error) => {
                                    eprintln!("draft failed: {error}");
                                    *failures.lock().unwrap() += 1;
                                    continue;
                                }
                            }
                        }
                    };
                    let grader_user = format!(
                        "<transcript>\n{}\n</transcript>\n\nThe question Me must answer: \"{}\"\n\n<draft>\n{}\n</draft>",
                        job.transcript,
                        job.question,
                        draft.text.trim()
                    );
                    let grade_key = cache_key(&grader_model, GRADER_SYSTEM, &grader_user);
                    let cached = cache.lock().unwrap().grades.get(&grade_key).cloned();
                    let grade = match cached {
                        Some(grade) => grade,
                        None => {
                            let request = AnswerRequest {
                                system: GRADER_SYSTEM.to_owned(),
                                user: grader_user,
                                image_jpeg: None,
                                model: grader_model.clone(),
                                effort: CopilotEffort::Low,
                                web_search: false,
                                max_output_tokens: 400,
                                cache_key: "answer-eval-grader".into(),
                            };
                            match ask(&grader_settings, &chatgpt, &request)
                                .map(|(text, _)| parse_grade(&text))
                            {
                                Ok(Some(grade)) => {
                                    cache
                                        .lock()
                                        .unwrap()
                                        .grades
                                        .insert(grade_key, grade.clone());
                                    grade
                                }
                                Ok(None) | Err(_) => {
                                    eprintln!("grade failed for {}", clip(&job.question));
                                    *failures.lock().unwrap() += 1;
                                    continue;
                                }
                            }
                        }
                    };
                    results.lock().unwrap()[index] = Some((draft, grade));
                }
            });
        }
    });

    let results = results.into_inner().unwrap();
    let mut overall = Totals::default();
    let mut by_set: Vec<(String, Totals)> = Vec::new();
    let mut latencies = Vec::new();
    for (job, result) in jobs.iter().zip(&results) {
        let Some((draft, grade)) = result else {
            continue;
        };
        let words = draft.text.split_whitespace().count();
        overall.add(grade, words);
        latencies.push(draft.elapsed_ms);
        match by_set.iter_mut().find(|(set, _)| *set == job.set) {
            Some((_, totals)) => totals.add(grade, words),
            None => {
                let mut totals = Totals::default();
                totals.add(grade, words);
                by_set.push((job.set.clone(), totals));
            }
        }
        if job.sample == 0 {
            println!(
                "\n[{:.2}] {} / {}: {}\n  d{} g{} u{} l{} {}",
                grade.mean(),
                job.set,
                job.meeting,
                clip(&job.question),
                grade.direct,
                grade.grounded,
                grade.useful,
                grade.glanceable,
                if grade.issue.is_empty() {
                    String::new()
                } else {
                    format!("- {}", grade.issue)
                }
            );
            for line in draft
                .text
                .trim()
                .lines()
                .filter(|line| !line.trim().is_empty())
            {
                println!("    | {line}");
            }
        }
    }
    println!();
    for (set, totals) in &by_set {
        println!("{set}: {}", totals.summary());
    }
    println!("OVERALL: {}", overall.summary());
    latencies.sort_unstable();
    if let Some(median) = latencies.get(latencies.len() / 2) {
        println!("median draft {median} ms");
    }
    if let Ok(text) = serde_json::to_string(&cache.into_inner().unwrap()) {
        let _ = std::fs::write(&cache_path, text);
    }
    Ok(failures.into_inner().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grades_parse_from_fenced_json() {
        let grade = parse_grade(
            "```json\n{\"direct\": 5, \"grounded\": 2, \"useful\": 4, \"glanceable\": 3, \"issue\": \"invents a price\"}\n```",
        )
        .expect("grade parses");
        assert_eq!(grade.mean(), 3.5);
        assert_eq!(grade.issue, "invents a price");
        assert!(parse_grade("no json here").is_none());
    }
}
