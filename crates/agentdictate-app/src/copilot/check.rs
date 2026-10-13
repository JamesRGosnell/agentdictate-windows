//! `agentdictate copilot --check`: verifies every copilot dependency without
//! opening the overlay. Optionally replays a speech recording as the other
//! side of a call to exercise segmentation, transcription, and answering.

use std::{
    path::Path,
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::{Duration, Instant},
};

use agentdictate_core::{
    CopilotEffort, CopilotSettings, QuickAction, Speaker, TranscriptLine, answer_system_prompt,
    answer_user_prompt, detected_question_instruction, insights_system_prompt,
    insights_user_prompt, looks_like_question, parse_insights, render_transcript,
};

use super::{
    engine::{AudioMsg, Platform},
    providers::{self, AnswerRequest},
    store,
    stream::StreamEvent,
    vad::{SegmentEvent, Segmenter, SegmenterConfig, wav_bytes},
    win::{self, replay::read_speech},
};
use crate::codex_subscription::{ChatGptCredentials, transcribe_wav_with_chatgpt};

struct Report {
    failures: usize,
}

impl Report {
    fn pass(&self, step: &str, detail: impl AsRef<str>) {
        println!("PASS  {step}: {}", detail.as_ref());
    }

    fn fail(&mut self, step: &str, detail: impl AsRef<str>) {
        self.failures += 1;
        println!("FAIL  {step}: {}", detail.as_ref());
    }

    fn info(&self, detail: impl AsRef<str>) {
        println!("      {}", detail.as_ref());
    }
}

/// Runs the checks and returns the number of failures.
pub(crate) fn run(speech: Option<&Path>, latency: bool) -> anyhow::Result<usize> {
    let mut report = Report { failures: 0 };
    let paths = crate::AppPaths::from_environment()?;
    let settings = store::load(&store::settings_path(&paths.config_file));
    println!(
        "AgentDictate copilot check \u{2014} answers: {} ({}), insights: {}",
        settings.provider.label(),
        settings.answer_model(),
        settings.fast_model()
    );
    let chatgpt = Arc::new(ChatGptCredentials::discover());
    let started = Instant::now();
    match chatgpt.bearer(false) {
        Ok(_) => report.pass(
            "ChatGPT sign-in",
            format!("available in {} ms", started.elapsed().as_millis()),
        ),
        Err(error) => report.fail("ChatGPT sign-in", error),
    }
    let mut catalog = Vec::new();
    match chatgpt.list_models() {
        Ok(models) => {
            catalog = models.iter().map(|(id, _)| id.clone()).collect();
            report.pass("ChatGPT models", catalog.join(", "));
        }
        Err(error) => report.fail("ChatGPT models", error),
    }

    let client = providers::http_client();
    let mut transcript: Vec<TranscriptLine> = Vec::new();
    if let Some(path) = speech {
        match read_speech(path) {
            Ok(samples) => {
                let mut segmenter = Segmenter::new(SegmenterConfig::system_audio());
                let mut events = Vec::new();
                for chunk in samples.chunks(480) {
                    events.extend(segmenter.push(chunk));
                }
                events.extend(segmenter.push_silence(16_000));
                let utterances: Vec<_> = events
                    .into_iter()
                    .filter_map(|event| match event {
                        SegmentEvent::Utterance(utterance) => Some(utterance),
                        _ => None,
                    })
                    .collect();
                if utterances.is_empty() {
                    report.fail("Speech segmentation", "no utterances were detected");
                } else {
                    report.pass(
                        "Speech segmentation",
                        format!(
                            "{} utterance(s) from {:.1}s of audio",
                            utterances.len(),
                            samples.len() as f32 / 16_000.0
                        ),
                    );
                }
                for (index, utterance) in utterances.into_iter().enumerate() {
                    let started = Instant::now();
                    match transcribe_wav_with_chatgpt(
                        &client,
                        &chatgpt,
                        &wav_bytes(&utterance.samples),
                        &settings.language,
                    ) {
                        Ok(text) => {
                            report.pass(
                                "Transcription",
                                format!(
                                    "{} ms for {} ms of speech: \u{201c}{text}\u{201d}",
                                    started.elapsed().as_millis(),
                                    utterance.duration_ms()
                                ),
                            );
                            transcript.push(TranscriptLine {
                                id: index as u64 + 1,
                                speaker: Speaker::Them,
                                text,
                                at_ms: utterance.start_ms(),
                            });
                        }
                        Err(error) => report.fail("Transcription", error),
                    }
                }
            }
            Err(error) => report.fail("Speech file", error),
        }
    }

    let question = transcript
        .iter()
        .rev()
        .find(|line| looks_like_question(&line.text))
        .map(|line| line.text.clone());
    if speech.is_some() {
        match &question {
            Some(question) => report.pass("Question detection", question),
            None => report.fail("Question detection", "no question was recognized"),
        }
    }
    let instruction = question.as_deref().map_or_else(
        || QuickAction::WhatToSay.instruction().to_owned(),
        detected_question_instruction,
    );
    let fallbacks: Vec<String> = catalog.iter().take(2).cloned().collect();
    let answer =
        |settings: &CopilotSettings, request: &AnswerRequest, label: &str, report: &mut Report| {
            let provider = match providers::provider_for(settings, &chatgpt, "", &fallbacks) {
                Ok(provider) => provider,
                Err(error) => {
                    report.fail(label, error);
                    return None;
                }
            };
            let started = Instant::now();
            let first_text = Mutex::new(None);
            let searches = Mutex::new(Vec::new());
            let sources = Mutex::new(Vec::new());
            let mut text = String::new();
            let cancel = AtomicBool::new(false);
            let result = provider.stream(request, &cancel, &mut |event| match event {
                StreamEvent::Text(delta) => {
                    first_text
                        .lock()
                        .unwrap()
                        .get_or_insert(started.elapsed().as_millis());
                    text.push_str(&delta);
                }
                StreamEvent::Searching(query) => searches.lock().unwrap().push(query),
                StreamEvent::Source(source) => sources.lock().unwrap().push(source.url),
                StreamEvent::Done | StreamEvent::Error(_) => {}
            });
            match result {
                Ok(()) if !text.trim().is_empty() => {
                    report.pass(
                        label,
                        format!(
                            "first text {} ms, complete {} ms, {} chars, searches {:?}, sources {}",
                            first_text.lock().unwrap().unwrap_or_default(),
                            started.elapsed().as_millis(),
                            text.len(),
                            searches.lock().unwrap(),
                            sources.lock().unwrap().len()
                        ),
                    );
                    Some(text)
                }
                Ok(()) => {
                    report.fail(label, "empty answer");
                    None
                }
                Err(error) => {
                    report.fail(label, error);
                    None
                }
            }
        };
    let transcript_text = render_transcript(&transcript, 24_000);
    let request = AnswerRequest {
        system: answer_system_prompt(&settings),
        user: answer_user_prompt(&transcript_text, &instruction, false),
        image_jpeg: None,
        model: settings.answer_model(),
        effort: settings.effort,
        web_search: settings.web_search,
        max_output_tokens: 4096,
        cache_key: uuid::Uuid::new_v4().to_string(),
    };
    if let Some(text) = answer(
        &settings,
        &request,
        "Answer (answer model, web search)",
        &mut report,
    ) {
        for line in text.lines().filter(|line| !line.trim().is_empty()).take(8) {
            report.info(line);
        }
    }
    let mut fast = settings.clone();
    fast.set_answer_model(settings.fast_model());
    if latency {
        for (model_settings, model_label) in [(&settings, "answer model"), (&fast, "fast model")] {
            for web_search in [false, true] {
                let mut timed = request.clone();
                timed.model = model_settings.answer_model();
                timed.web_search = web_search;
                timed.cache_key = uuid::Uuid::new_v4().to_string();
                let label = format!(
                    "Latency {} ({}), web search {}",
                    model_label,
                    timed.model,
                    if web_search { "on" } else { "off" }
                );
                let _ = answer(model_settings, &timed, &label, &mut report);
            }
        }
    }
    if !transcript.is_empty() {
        let insights_request = AnswerRequest {
            system: insights_system_prompt().to_owned(),
            user: insights_user_prompt(&transcript_text),
            image_jpeg: None,
            model: fast.answer_model(),
            effort: CopilotEffort::Low,
            web_search: false,
            max_output_tokens: 800,
            cache_key: uuid::Uuid::new_v4().to_string(),
        };
        let notes_request = AnswerRequest {
            system: agentdictate_core::notes_system_prompt().to_owned(),
            user: format!("<transcript>\n{transcript_text}\n</transcript>"),
            image_jpeg: None,
            model: fast.answer_model(),
            effort: CopilotEffort::Low,
            web_search: false,
            max_output_tokens: 2048,
            cache_key: uuid::Uuid::new_v4().to_string(),
        };
        if let Some(text) = answer(&fast, &notes_request, "Meeting notes summary", &mut report)
            && let Some(first) = text.lines().find(|line| !line.trim().is_empty())
        {
            report.info(first);
        }
        if let Some(text) = answer(
            &fast,
            &insights_request,
            "Live insights (fast model)",
            &mut report,
        ) {
            match parse_insights(&text) {
                Some(insights) => report.info(format!(
                    "topic \u{201c}{}\u{201d}, question {:?}, suggestions {:?}",
                    insights.topic, insights.question, insights.suggestions
                )),
                None => report.fail("Live insights", "response was not valid JSON"),
            }
        }
    }

    let platform = win::WindowsPlatform;
    let started = Instant::now();
    match platform.capture_screen() {
        Ok(jpeg) => report.pass(
            "Screen capture",
            format!(
                "{} KB JPEG in {} ms",
                jpeg.len() / 1024,
                started.elapsed().as_millis()
            ),
        ),
        Err(error) => report.fail("Screen capture", error),
    }
    report.pass("Audio devices", win::audio::describe_devices());
    let messages = Arc::new(Mutex::new(Vec::new()));
    let sink_messages = Arc::clone(&messages);
    match platform.start_audio(
        settings.capture_microphone,
        settings.capture_system_audio,
        Arc::new(move |message| sink_messages.lock().unwrap().push(message)),
    ) {
        Ok(audio) => {
            std::thread::sleep(Duration::from_secs(2));
            // While capturing, this process itself is a microphone user, which
            // exercises the same registry the meeting detector reads.
            let active = win::signals::active_microphone_apps();
            let titles = win::signals::window_titles();
            let signal = super::meeting::classify(&active, &titles);
            let names: Vec<String> = active
                .iter()
                .map(|app| app.rsplit(['#', '\\']).next().unwrap_or(app).to_owned())
                .collect();
            if names
                .iter()
                .any(|name| name.to_lowercase().starts_with("agentdictate"))
            {
                report.pass(
                    "Meeting detection",
                    format!(
                        "microphone users {names:?}; {}",
                        if signal.in_meeting {
                            format!("meeting detected ({})", signal.reason)
                        } else {
                            "no meeting detected".to_owned()
                        }
                    ),
                );
            } else {
                report.fail(
                    "Meeting detection",
                    format!("this process was not listed as a microphone user: {names:?}"),
                );
            }
            audio.stop();
            let messages = messages.lock().unwrap();
            let levels = |speaker: Speaker| {
                messages
                    .iter()
                    .filter(|message| matches!(message, AudioMsg::Level { speaker: source, .. } if *source == speaker))
                    .count()
            };
            let problems: Vec<_> = messages
                .iter()
                .filter_map(|message| match message {
                    AudioMsg::Problem(problem) => Some(problem.clone()),
                    _ => None,
                })
                .collect();
            report.pass(
                "Audio capture",
                format!(
                    "microphone meter updates {}, system audio {}",
                    levels(Speaker::Me),
                    match levels(Speaker::Them) {
                        // WASAPI loopback delivers no packets during silence.
                        0 => "connected (nothing is playing)".to_owned(),
                        updates => format!("meter updates {updates}"),
                    }
                ),
            );
            for problem in problems {
                report.info(format!("audio note: {problem}"));
            }
            for status in messages.iter().filter_map(|message| match message {
                AudioMsg::Status(status) => Some(status.clone()),
                _ => None,
            }) {
                if status.contains("echo cancellation") {
                    report.pass("Echo cancellation", status);
                } else {
                    report.info(format!(
                        "{status} (the copilot's own echo gate removes speaker echo)"
                    ));
                }
            }
        }
        Err(error) => report.fail("Audio capture", error),
    }
    println!(
        "{} \u{2014} {} failure(s)",
        if report.failures == 0 {
            "Copilot ready"
        } else {
            "Copilot needs attention"
        },
        report.failures
    );
    Ok(report.failures)
}

/// Plays `speech` through the default speakers while the copilot captures,
/// then reports whether any of it leaked into the microphone as "You".
/// Run it in a quiet room without speaking. Returns the number of failures.
pub(crate) fn echo_test(speech: &Path) -> anyhow::Result<usize> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let samples = read_speech(speech).map_err(anyhow::Error::msg)?;
    let device = cpal::default_host()
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("no playback device"))?;
    let config: cpal::StreamConfig = device.default_output_config()?.into();
    let channels = usize::from(config.channels.max(1));
    let step = 16_000.0 / config.sample_rate.0 as f64;
    let position = Arc::new(Mutex::new(0.0_f64));
    let playing = Arc::clone(&position);
    let source = Arc::new(samples);
    let played = Arc::clone(&source);
    let messages = Arc::new(Mutex::new(Vec::new()));
    let sink_messages = Arc::clone(&messages);
    let platform = win::WindowsPlatform;
    let audio = platform
        .start_audio(
            true,
            true,
            Arc::new(move |message| sink_messages.lock().unwrap().push(message)),
        )
        .map_err(anyhow::Error::msg)?;
    // Let echo cancellation and the echo gate start before the call audio.
    std::thread::sleep(Duration::from_millis(1_500));
    let stream = device.build_output_stream(
        &config,
        move |data: &mut [f32], _| {
            let mut position = playing.lock().unwrap();
            for frame in data.chunks_mut(channels) {
                let index = *position as usize;
                let sample = played.get(index).copied().unwrap_or(0.0) * 0.8;
                frame.fill(sample);
                *position += step;
            }
        },
        |error| eprintln!("playback error: {error}"),
        None,
    )?;
    println!(
        "Playing {:.1}s of speech through the speakers. Stay quiet.",
        source.len() as f32 / 16_000.0
    );
    stream.play()?;
    std::thread::sleep(Duration::from_secs_f32(
        source.len() as f32 / 16_000.0 + 2.5,
    ));
    drop(stream);
    audio.stop();
    let messages = messages.lock().unwrap();
    let utterances = |speaker: Speaker| {
        messages
            .iter()
            .filter_map(|message| match message {
                AudioMsg::Utterance {
                    speaker: source,
                    utterance,
                } if *source == speaker => Some(utterance.duration_ms()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    for status in messages.iter().filter_map(|message| match message {
        AudioMsg::Status(status) => Some(status),
        _ => None,
    }) {
        println!("      {status}");
    }
    let them = utterances(Speaker::Them);
    let mine = utterances(Speaker::Me);
    println!(
        "      system audio heard {} utterance(s): {them:?} ms",
        them.len()
    );
    let mut failures = 0;
    if them.is_empty() {
        failures += 1;
        println!("FAIL  Echo test: the speakers' audio was not captured as \u{201c}Them\u{201d}");
    }
    if mine.is_empty() {
        println!("PASS  Echo test: nothing leaked into \u{201c}You\u{201d}");
    } else {
        failures += 1;
        println!(
            "FAIL  Echo test: {} microphone utterance(s) {mine:?} ms (speaker echo or room noise)",
            mine.len()
        );
    }
    Ok(failures)
}
