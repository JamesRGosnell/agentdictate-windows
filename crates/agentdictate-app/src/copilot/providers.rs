//! Streaming answer providers for the meeting copilot.

use std::{
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use agentdictate_core::{CopilotEffort, CopilotProvider, CopilotSettings};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::StatusCode;
use serde_json::{Value, json};

use super::stream::{
    AnthropicStream, ClaudeCodeStream, StreamEvent, next_sse_message, parse_responses_event,
};
use crate::codex_subscription::ChatGptCredentials;

const CHATGPT_RESPONSES_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";
const OPENAI_RESPONSES_ENDPOINT: &str = "https://api.openai.com/v1/responses";
const OPENAI_DECISIONS_ENDPOINT: &str = "https://api.openai.com/v1/decisions";
const OPENAI_TRANSCRIPTION_ENDPOINT: &str = "https://api.openai.com/v1/audio/transcriptions";
const ANTHROPIC_MESSAGES_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";

/// One answer request, independent of provider.
#[derive(Clone, Debug)]
pub(crate) struct AnswerRequest {
    pub system: String,
    pub user: String,
    pub image_jpeg: Option<Arc<Vec<u8>>>,
    pub model: String,
    pub effort: CopilotEffort,
    pub web_search: bool,
    pub max_output_tokens: u32,
    /// Stable per-session key that lets backends reuse cached prefixes.
    pub cache_key: String,
}

/// A yes/no condition for a service that decides rather than writes.
#[derive(Clone, Debug)]
pub(crate) struct Decision {
    /// How to decide; the same for every request, so it can be cached.
    pub rules: String,
    /// What the condition is about.
    pub evidence: String,
    pub question: String,
    pub cache_key: String,
}

pub(crate) trait AnswerProvider: Send + Sync {
    /// Streams the answer into `sink`. Returns once the answer completes,
    /// fails, or `cancel` is observed.
    fn stream(
        &self,
        request: &AnswerRequest,
        cancel: &AtomicBool,
        sink: &mut dyn FnMut(StreamEvent),
    ) -> Result<(), String>;

    /// The probability that `decision` holds, from a service built to decide
    /// without writing. `None` when this provider has no such service.
    fn decide(&self, _decision: &Decision) -> Option<Result<f32, String>> {
        None
    }
}

/// The turn judge's verdict.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Judged {
    pub respond: Option<bool>,
    /// How likely the line calls for an answer, when a decision service said.
    pub probability: Option<f32>,
}

/// Judges a turn with the provider's decision service, or by asking its
/// model when it has none or the service fails.
pub(crate) fn judge_turn(
    provider: &dyn AnswerProvider,
    request: &AnswerRequest,
    cancel: &AtomicBool,
) -> Result<Judged, String> {
    let decision = Decision {
        rules: agentdictate_core::turn_judge_rules().to_owned(),
        evidence: request.user.clone(),
        question: agentdictate_core::TURN_JUDGE_QUESTION.to_owned(),
        cache_key: request.cache_key.clone(),
    };
    match provider.decide(&decision) {
        Some(Ok(probability)) => {
            return Ok(Judged {
                respond: Some(probability >= agentdictate_core::TURN_JUDGE_THRESHOLD),
                probability: Some(probability),
            });
        }
        Some(Err(error)) => tracing::warn!(%error, "copilot decision failed; asking the model"),
        None => {}
    }
    let text = complete(provider, request, cancel)?;
    Ok(Judged {
        respond: agentdictate_core::parse_turn_verdict(&text),
        probability: None,
    })
}

/// The process-wide HTTP client. Sharing it keeps connections open between
/// requests, so answers after the first skip the TLS handshake. Idle
/// connections are let go after a minute: one kept longer is more likely
/// to have been dropped by the network without either end knowing.
pub(crate) fn http_client() -> reqwest::blocking::Client {
    static CLIENT: std::sync::OnceLock<reqwest::blocking::Client> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::blocking::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(240))
                .pool_idle_timeout(Duration::from_secs(60))
                .build()
                .expect("the rustls HTTP client must be constructible")
        })
        .clone()
}

/// A client that opens a new connection for every request, for the one
/// retry of a request that got no reply on a kept-open connection.
fn fresh_client() -> reqwest::blocking::Client {
    static CLIENT: std::sync::OnceLock<reqwest::blocking::Client> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::blocking::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(240))
                .pool_max_idle_per_host(0)
                .build()
                .expect("the rustls HTTP client must be constructible")
        })
        .clone()
}

/// How long a model request may go without a word from the service before
/// it is given up on, rather than leaving its lane stuck for minutes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Deadlines {
    /// From sending the request to the reply's headers.
    pub first_byte: Duration,
    /// Between any two pieces of a streamed reply.
    pub idle: Duration,
}

/// Judgments, instant drafts and other quick side jobs: their services
/// reply in well under a second.
const QUICK_DEADLINES: Deadlines = Deadlines {
    first_byte: Duration::from_secs(5),
    idle: Duration::from_secs(20),
};
/// Researched answers and summaries, which may search the web or reason
/// for a while between streamed pieces.
const RESEARCH_DEADLINES: Deadlines = Deadlines {
    first_byte: Duration::from_secs(15),
    idle: Duration::from_secs(45),
};

/// A request carrying a screenshot also gets the longer wait, for the
/// upload.
fn deadlines_for(request: &AnswerRequest) -> Deadlines {
    let research = request.web_search
        || request.image_jpeg.is_some()
        || matches!(request.effort, CopilotEffort::Medium | CopilotEffort::High)
        || request.max_output_tokens >= 2_048;
    if research {
        RESEARCH_DEADLINES
    } else {
        QUICK_DEADLINES
    }
}

/// How often a request waiting on the network looks for a cancel.
const CANCEL_POLL: Duration = Duration::from_millis(25);

/// Why a request got no reply.
enum NoReply {
    Failed(reqwest::Error),
    TimedOut(Duration),
}

impl NoReply {
    fn describe(&self, service: &str) -> String {
        match self {
            Self::Failed(error) => format!("Could not reach {service}: {error}"),
            Self::TimedOut(after) => format!(
                "Could not reach {service}: no reply within {} s",
                after.as_secs_f32()
            ),
        }
    }
}

/// Sends on a thread of its own, so a service that never answers is given
/// up on after `deadline` and a cancel is seen at once. `Ok(None)` when
/// cancelled.
fn send_within(
    send: impl FnOnce() -> reqwest::Result<reqwest::blocking::Response> + Send + 'static,
    deadline: Duration,
    cancel: &AtomicBool,
) -> Result<Option<reqwest::blocking::Response>, NoReply> {
    let (replied, reply) = mpsc::sync_channel(1);
    // A reply that comes after the deadline is dropped with its thread.
    std::thread::Builder::new()
        .name("copilot-request".into())
        .spawn(move || {
            let _ = replied.send(send());
        })
        .map_err(|_| NoReply::TimedOut(Duration::ZERO))?;
    let until = Instant::now() + deadline;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(NoReply::TimedOut(deadline));
        }
        match reply.recv_timeout(left.min(CANCEL_POLL)) {
            Ok(Ok(response)) => return Ok(Some(response)),
            Ok(Err(error)) => return Err(NoReply::Failed(error)),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(NoReply::TimedOut(deadline));
            }
        }
    }
}

/// Sends a request, trying once more on a new connection when nothing at
/// all came back: a connection kept open between answers can go dead
/// without either end knowing. `Ok(None)` when cancelled.
fn send_request(
    builder: reqwest::blocking::RequestBuilder,
    deadline: Duration,
    cancel: &AtomicBool,
    service: &str,
) -> Result<Option<reqwest::blocking::Response>, String> {
    let retry = builder.try_clone().and_then(|retry| retry.build().ok());
    let error = match send_within(move || builder.send(), deadline, cancel) {
        Ok(response) => return Ok(response),
        Err(error) => error,
    };
    let Some(request) = retry else {
        return Err(error.describe(service));
    };
    if cancel.load(Ordering::Relaxed) {
        return Ok(None);
    }
    tracing::warn!(
        service,
        error = %error.describe(service),
        "no reply from the model service; trying once more on a new connection"
    );
    let client = fresh_client();
    send_within(move || client.execute(request), deadline, cancel)
        .map_err(|error| error.describe(service))
}

/// A reader that notes when it last received anything.
struct Heard<R> {
    inner: R,
    started: Instant,
    last_ms: Arc<AtomicU64>,
}

impl<R: std::io::Read> std::io::Read for Heard<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        if read > 0 {
            self.last_ms
                .store(self.started.elapsed().as_millis() as u64, Ordering::Relaxed);
        }
        Ok(read)
    }
}

/// Builds the provider selected in settings.
pub(crate) fn provider_for(
    settings: &CopilotSettings,
    chatgpt: &Arc<ChatGptCredentials>,
    fallback_openai_key: &str,
    chatgpt_models: &[String],
) -> Result<Box<dyn AnswerProvider>, String> {
    match settings.provider {
        CopilotProvider::ChatgptSubscription => Ok(Box::new(ResponsesProvider {
            client: http_client(),
            endpoint: CHATGPT_RESPONSES_ENDPOINT,
            auth: ResponsesAuth::ChatGpt(Arc::clone(chatgpt)),
            fallback_models: chatgpt_models.to_vec(),
            deadlines: None,
        })),
        CopilotProvider::OpenaiApi => {
            let key = openai_key(settings, fallback_openai_key);
            if key.is_empty() {
                return Err("Add an OpenAI API key in Copilot settings.".to_owned());
            }
            Ok(Box::new(ResponsesProvider {
                client: http_client(),
                endpoint: OPENAI_RESPONSES_ENDPOINT,
                auth: ResponsesAuth::ApiKey(key.to_owned()),
                fallback_models: Vec::new(),
                deadlines: None,
            }))
        }
        CopilotProvider::AnthropicApi => {
            let key = settings.anthropic_api_key.trim();
            if key.is_empty() {
                return Err(
                    "Add an Anthropic API key in Copilot settings to use Claude API.".to_owned(),
                );
            }
            // OpenAI's decision service judges turns in a fraction of the
            // time Claude takes to write a verdict, so it judges whenever an
            // OpenAI key is saved; Claude still writes every answer.
            Ok(Box::new(AnthropicProvider {
                client: http_client(),
                api_key: key.to_owned(),
                decider: openai_decider(settings, fallback_openai_key),
                deadlines: None,
            }))
        }
        CopilotProvider::ClaudeCode => {
            let binary = claude_code_binary().ok_or_else(|| {
                "Claude Code was not found. Install it or choose another provider.".to_owned()
            })?;
            Ok(Box::new(ClaudeCodeProvider { binary }))
        }
    }
}

/// OpenAI's decision service on the saved OpenAI key, if there is one.
fn openai_decider(settings: &CopilotSettings, fallback: &str) -> Option<ResponsesProvider> {
    let key = openai_key(settings, fallback);
    (!key.is_empty()).then(|| ResponsesProvider {
        client: http_client(),
        endpoint: OPENAI_RESPONSES_ENDPOINT,
        auth: ResponsesAuth::ApiKey(key.to_owned()),
        fallback_models: Vec::new(),
        deadlines: None,
    })
}

/// The copilot's own OpenAI key, else the one dictation has.
fn openai_key<'a>(settings: &'a CopilotSettings, fallback: &'a str) -> &'a str {
    if settings.openai_api_key.trim().is_empty() {
        fallback.trim()
    } else {
        settings.openai_api_key.trim()
    }
}

enum ResponsesAuth {
    ChatGpt(Arc<ChatGptCredentials>),
    ApiKey(String),
}

/// OpenAI Responses API, used both with API keys and through the ChatGPT
/// subscription backend that Codex uses.
struct ResponsesProvider {
    client: reqwest::blocking::Client,
    endpoint: &'static str,
    auth: ResponsesAuth,
    fallback_models: Vec<String>,
    /// Set in tests; otherwise chosen by the request.
    deadlines: Option<Deadlines>,
}

fn responses_body(request: &AnswerRequest, model: &str) -> Value {
    let mut content = Vec::new();
    if let Some(image) = &request.image_jpeg {
        content.push(json!({
            "type": "input_image",
            "image_url": format!("data:image/jpeg;base64,{}", STANDARD.encode(image.as_slice())),
        }));
    }
    content.push(json!({"type": "input_text", "text": request.user}));
    let tools = if request.web_search {
        vec![json!({"type": "web_search"})]
    } else {
        Vec::new()
    };
    // "gpt-6-sol#fast" asks for that model in OpenAI's Fast mode.
    let (model_id, fast) = match model.strip_suffix(FAST_MODE_SUFFIX) {
        Some(model_id) => (model_id, true),
        None => (model, false),
    };
    let mut body = json!({
        "model": model_id,
        "instructions": request.system,
        "input": [{"type": "message", "role": "user", "content": content}],
        "tools": tools,
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "reasoning": {"effort": effort_for(model, request.effort).as_str(), "summary": "auto"},
        "store": false,
        "stream": true,
        "include": [],
        "prompt_cache_key": cache_key(&request.cache_key),
    });
    if fast {
        body["service_tier"] = json!("fast");
    }
    body
}

/// Ends a model id that should run in OpenAI's Fast mode, which answers
/// sooner at a higher price per token.
pub(crate) const FAST_MODE_SUFFIX: &str = "#fast";

/// Models that refused to answer without reasoning in this process.
static MUST_REASON: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The effort to ask `model` for: the least it accepts that is not below
/// `effort`.
fn effort_for(model: &str, effort: CopilotEffort) -> CopilotEffort {
    let must_reason = MUST_REASON
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if must_reason.iter().any(|known| known == model) {
        effort.at_least_low()
    } else {
        effort
    }
}

/// True when the service refused the request's reasoning effort.
fn is_effort_rejection(status: StatusCode, body: &str) -> bool {
    status == StatusCode::BAD_REQUEST && {
        let body = body.to_lowercase();
        body.contains("unsupported value")
            && (body.contains("'none'") || body.contains("'minimal'"))
    }
}

/// The OpenAI API rejects cache keys over 64 characters; longer keys are
/// sent as their SHA-256, which keeps them distinct and stable.
fn cache_key(key: &str) -> String {
    use sha2::{Digest, Sha256};
    if key.len() <= 64 {
        return key.to_owned();
    }
    Sha256::digest(key.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Models the service rejected during this process, tried last afterwards
/// so every later request does not pay a failed round trip first.
static REJECTED_MODELS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Moves models the service already rejected to the end of `models`.
fn prefer_accepted(models: &mut [String]) {
    let rejected = REJECTED_MODELS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    models.sort_by_key(|model| rejected.contains(model));
}

/// Rejected models and the model that answered in their place.
static REPLACEMENTS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// The model that answered when `model` was rejected in this process.
pub(crate) fn replacement_for(model: &str) -> Option<String> {
    REPLACEMENTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .find(|(rejected, _)| rejected == model)
        .map(|(_, working)| working.clone())
}

pub(crate) fn remember_replacements(rejected: &[String], working: &str) {
    let mut replacements = REPLACEMENTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for model in rejected {
        if !replacements.iter().any(|(known, _)| known == model) {
            replacements.push((model.clone(), working.to_owned()));
        }
    }
}

fn remember_rejected(model: &str) {
    let mut rejected = REJECTED_MODELS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !rejected.iter().any(|known| known == model) {
        rejected.push(model.to_owned());
    }
}

fn is_model_rejection(status: StatusCode, body: &str) -> bool {
    matches!(status, StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND)
        && body.to_lowercase().contains("model")
}

fn http_failure(service: &str, status: StatusCode, body: &str) -> String {
    let detail = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.get("detail"))
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();
    let summary = match status {
        StatusCode::UNAUTHORIZED => format!("{service} rejected the sign-in or key."),
        StatusCode::FORBIDDEN => format!("{service} access is not available for this account."),
        StatusCode::TOO_MANY_REQUESTS => {
            format!("{service} rate limit reached. Try again shortly.")
        }
        status if status.is_server_error() => format!("{service} is temporarily unavailable."),
        status => format!("{service} returned HTTP {}.", status.as_u16()),
    };
    let resets = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/resets_in_seconds")
                .and_then(Value::as_u64)
        })
        .map(|seconds| format!(" It resets in about {}.", wait_label(seconds)))
        .unwrap_or_default();
    let mut detail: String = detail.chars().take(240).collect();
    if !resets.is_empty() && !detail.is_empty() && !detail.ends_with(['.', '!', '?']) {
        detail.push('.');
    }
    if detail.is_empty() {
        format!("{summary}{resets}")
    } else {
        format!("{summary} {detail}{resets}")
    }
}

/// "45 min", "3 h 20 min", or "29 days" for a usage-limit reset.
fn wait_label(seconds: u64) -> String {
    let minutes = seconds.div_ceil(60).max(1);
    if minutes < 60 {
        format!("{minutes} min")
    } else if minutes < 48 * 60 {
        match minutes % 60 {
            0 => format!("{} h", minutes / 60),
            rest => format!("{} h {rest} min", minutes / 60),
        }
    } else {
        format!("{} days", minutes.div_ceil(24 * 60))
    }
}

/// Reads an SSE body, forwarding mapped events until completion. The body
/// is read on a thread of its own, so a stream that goes quiet for `idle`
/// fails instead of hanging, and a cancel is seen at once.
fn pump_sse(
    response: reqwest::blocking::Response,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(StreamEvent),
    idle: Duration,
    mut map: impl FnMut(&Value) -> Vec<StreamEvent>,
) -> Result<(), String> {
    let started = Instant::now();
    let last_ms = Arc::new(AtomicU64::new(0));
    let (messages, incoming) = mpsc::sync_channel(64);
    {
        let last_ms = Arc::clone(&last_ms);
        // Left behind on a stall, it ends with the client's own timeout.
        std::thread::Builder::new()
            .name("copilot-stream".into())
            .spawn(move || {
                let mut reader = BufReader::new(Heard {
                    inner: response,
                    started,
                    last_ms,
                });
                loop {
                    let message = next_sse_message(&mut reader);
                    let more = matches!(message, Ok(Some(_)));
                    if messages.send(message).is_err() || !more {
                        return;
                    }
                }
            })
            .map_err(|error| format!("Stream interrupted: {error}"))?;
    }
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let message = match incoming.recv_timeout(CANCEL_POLL) {
            Ok(message) => message,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let heard = Duration::from_millis(last_ms.load(Ordering::Relaxed));
                if started.elapsed().saturating_sub(heard) >= idle {
                    return Err(format!(
                        "Stream stalled: nothing arrived for {} s.",
                        idle.as_secs_f32()
                    ));
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("Stream interrupted.".to_owned());
            }
        };
        let Some(message) = message.map_err(|error| format!("Stream interrupted: {error}"))? else {
            sink(StreamEvent::Done);
            return Ok(());
        };
        if message.data == "[DONE]" {
            sink(StreamEvent::Done);
            return Ok(());
        }
        let Ok(value) = serde_json::from_str::<Value>(&message.data) else {
            continue;
        };
        for event in map(&value) {
            match event {
                StreamEvent::Error(message) => return Err(message),
                StreamEvent::Done => {
                    sink(StreamEvent::Done);
                    return Ok(());
                }
                event => sink(event),
            }
        }
    }
}

impl AnswerProvider for ResponsesProvider {
    fn stream(
        &self,
        request: &AnswerRequest,
        cancel: &AtomicBool,
        sink: &mut dyn FnMut(StreamEvent),
    ) -> Result<(), String> {
        let mut models = vec![request.model.clone()];
        for model in &self.fallback_models {
            if !models.contains(model) {
                models.push(model.clone());
            }
        }
        if matches!(self.auth, ResponsesAuth::ChatGpt(_)) && !models.iter().any(|m| m == "gpt-5.5")
        {
            models.push("gpt-5.5".to_owned());
        }
        prefer_accepted(&mut models);
        let service = match self.auth {
            ResponsesAuth::ChatGpt(_) => "ChatGPT",
            ResponsesAuth::ApiKey(_) => "OpenAI",
        };
        let deadlines = self.deadlines.unwrap_or_else(|| deadlines_for(request));
        let mut refreshed = false;
        let mut index = 0;
        let mut rejected_here: Vec<String> = Vec::new();
        while index < models.len() {
            if cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            let model = &models[index];
            let mut builder = self
                .client
                .post(self.endpoint)
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .json(&responses_body(request, model));
            builder = match &self.auth {
                ResponsesAuth::ChatGpt(credentials) => {
                    let bearer = credentials.bearer(refreshed)?;
                    builder
                        .bearer_auth(bearer.token)
                        .header("chatgpt-account-id", bearer.account_id)
                        .header("OpenAI-Beta", "responses=experimental")
                        .header("originator", "codex_cli_rs")
                        .header("session_id", &request.cache_key)
                }
                ResponsesAuth::ApiKey(key) => builder.bearer_auth(key),
            };
            if cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            let Some(response) = send_request(builder, deadlines.first_byte, cancel, service)?
            else {
                return Ok(());
            };
            let status = response.status();
            if status.is_success() {
                remember_replacements(&rejected_here, model);
                return pump_sse(
                    response,
                    cancel,
                    sink,
                    deadlines.idle,
                    parse_responses_event,
                );
            }
            let body = response.text().unwrap_or_default();
            if status == StatusCode::UNAUTHORIZED
                && matches!(self.auth, ResponsesAuth::ChatGpt(_))
                && !refreshed
            {
                refreshed = true;
                continue;
            }
            // Some models cannot skip reasoning; ask them again for a little.
            if is_effort_rejection(status, &body)
                && effort_for(model, request.effort) != request.effort.at_least_low()
            {
                tracing::warn!(model, "copilot model must reason; using low effort");
                let mut must_reason = MUST_REASON
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                must_reason.push(model.clone());
                continue;
            }
            if is_model_rejection(status, &body) && index + 1 < models.len() {
                remember_rejected(model);
                rejected_here.push(model.clone());
                tracing::warn!(
                    model,
                    status = status.as_u16(),
                    "copilot model rejected; trying fallback"
                );
                index += 1;
                continue;
            }
            return Err(http_failure(service, status, &body));
        }
        Err(format!("{service} has no usable model."))
    }

    fn decide(&self, decision: &Decision) -> Option<Result<f32, String>> {
        let ResponsesAuth::ApiKey(key) = &self.auth else {
            return None;
        };
        // Only OpenAI's own API has the service; a key for another host
        // never goes there.
        if self.endpoint != OPENAI_RESPONSES_ENDPOINT {
            return None;
        }
        if DECISIONS_UNAVAILABLE.load(Ordering::Relaxed) {
            return None;
        }
        let sent = self
            .client
            .post(OPENAI_DECISIONS_ENDPOINT)
            .bearer_auth(key)
            .timeout(DECISION_TIMEOUT)
            .json(&decision_body(decision))
            .send();
        let response = match sent {
            Ok(response) => response,
            Err(error) => return Some(Err(format!("Could not reach OpenAI: {error}"))),
        };
        let status = response.status();
        let body = response.text().unwrap_or_default();
        if !status.is_success() {
            // An account or key without the service would fail every time.
            if matches!(
                status,
                StatusCode::NOT_FOUND | StatusCode::FORBIDDEN | StatusCode::BAD_REQUEST
            ) {
                DECISIONS_UNAVAILABLE.store(true, Ordering::Relaxed);
            }
            return Some(Err(http_failure("OpenAI", status, &body)));
        }
        Some(parse_decision(&body))
    }
}

/// The only model the decision service runs.
const DECISIONS_MODEL: &str = "gpt-6-luna";
/// A decision slower than this is no faster than asking the model.
const DECISION_TIMEOUT: Duration = Duration::from_secs(3);
/// Set once the service refuses this key, so later judgments skip it.
static DECISIONS_UNAVAILABLE: AtomicBool = AtomicBool::new(false);

fn decision_body(decision: &Decision) -> Value {
    json!({
        "model": DECISIONS_MODEL,
        "input": [
            {"role": "developer", "content": [{"type": "input_text", "text": decision.rules}]},
            {"role": "user", "content": [{"type": "input_text", "text": decision.evidence}]},
        ],
        "questions": [{
            "type": "predicate",
            "name": "answer",
            "instructions": decision.question,
        }],
        "prompt_cache_key": cache_key(&decision.cache_key),
    })
}

/// The probability in a decision response with one yes/no question.
fn parse_decision(body: &str) -> Result<f32, String> {
    let value: Value =
        serde_json::from_str(body).map_err(|error| format!("Unreadable decision: {error}"))?;
    let answer = value
        .pointer("/answers/0")
        .ok_or_else(|| "The decision has no answer.".to_owned())?;
    answer
        .get("probability")
        .and_then(Value::as_f64)
        .map(|probability| probability as f32)
        .ok_or_else(|| {
            format!(
                "The decision was not a probability ({}).",
                answer
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            )
        })
}

struct AnthropicProvider {
    client: reqwest::blocking::Client,
    api_key: String,
    /// OpenAI's decision service, when an OpenAI key is saved.
    decider: Option<ResponsesProvider>,
    /// Set in tests; otherwise chosen by the request.
    deadlines: Option<Deadlines>,
}

/// Claude's basic web search. The newer dynamic-filtering search runs code
/// behind the scenes and measured 10-16 s to first words on a one-fact
/// question against 4-5 s for the basic one, which a live call cannot wait
/// for; answers here need a fact or two, not filtered pages.
fn anthropic_search_tool(_model: &str) -> Value {
    json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 3})
}

fn anthropic_body(request: &AnswerRequest, full: bool) -> Value {
    let mut content = Vec::new();
    if let Some(image) = &request.image_jpeg {
        content.push(json!({
            "type": "image",
            "source": {"type": "base64", "media_type": "image/jpeg", "data": STANDARD.encode(image.as_slice())},
        }));
    }
    content.push(json!({"type": "text", "text": request.user}));
    let mut body = json!({
        "model": request.model,
        "max_tokens": request.max_output_tokens,
        "stream": true,
        "system": [{"type": "text", "text": request.system, "cache_control": {"type": "ephemeral"}}],
        "messages": [{"role": "user", "content": content}],
    });
    if request.web_search {
        body["tools"] = json!([anthropic_search_tool(&request.model)]);
    }
    if full {
        body["output_config"] = json!({"effort": request.effort.at_least_low().as_str()});
        if let Some(thinking) = thinking_off(&request.model, request.effort) {
            body["thinking"] = thinking;
        }
        if supports_server_fallback(&request.model) {
            body["fallbacks"] = json!("default");
        }
    }
    body
}

/// Claude thinks by default, which delays the first words of an answer
/// asked for with no reasoning. Haiku 5.5 can switch thinking off and
/// Sonnet 5.5 only between tool calls; Opus and Fable always think, so low
/// effort is their least.
fn thinking_off(model: &str, effort: CopilotEffort) -> Option<serde_json::Value> {
    if !matches!(effort, CopilotEffort::None | CopilotEffort::Minimal) {
        return None;
    }
    if model.starts_with("claude-haiku-5") {
        Some(json!({"type": "disabled"}))
    } else if model == "claude-sonnet-5-5" {
        Some(json!({"type": "between_tools"}))
    } else {
        None
    }
}

fn supports_server_fallback(model: &str) -> bool {
    [
        "claude-opus-5-5",
        "claude-opus-5",
        "claude-fable-5-2",
        "claude-fable-5-1",
        "claude-sonnet-5-5",
    ]
    .contains(&model)
}

impl AnswerProvider for AnthropicProvider {
    fn decide(&self, decision: &Decision) -> Option<Result<f32, String>> {
        self.decider.as_ref()?.decide(decision)
    }

    fn stream(
        &self,
        request: &AnswerRequest,
        cancel: &AtomicBool,
        sink: &mut dyn FnMut(StreamEvent),
    ) -> Result<(), String> {
        let deadlines = self.deadlines.unwrap_or_else(|| deadlines_for(request));
        // The first attempt opts into effort control and server-side refusal
        // fallbacks; a plain request is retried if the account rejects them.
        for full in [true, false] {
            if cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            let mut builder = self
                .client
                .post(ANTHROPIC_MESSAGES_ENDPOINT)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .json(&anthropic_body(request, full));
            if full && supports_server_fallback(&request.model) {
                builder = builder.header("anthropic-beta", "server-side-fallback-2026-07-01");
            }
            if cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            let Some(response) = send_request(builder, deadlines.first_byte, cancel, "Anthropic")?
            else {
                return Ok(());
            };
            let status = response.status();
            if status.is_success() {
                let mut stream = AnthropicStream::default();
                return pump_sse(response, cancel, sink, deadlines.idle, |value| {
                    stream.parse(value)
                });
            }
            let body = response.text().unwrap_or_default();
            if full && status == StatusCode::BAD_REQUEST {
                tracing::warn!("Anthropic rejected optional request fields; retrying plainly");
                continue;
            }
            return Err(http_failure("Anthropic", status, &body));
        }
        Err("Anthropic rejected the request.".to_owned())
    }
}

/// Longest system prompt passed on Claude Code's command line.
const SYSTEM_PROMPT_ARGUMENT_CHARS: usize = 8_000;

/// Claude Code in headless mode, which answers with the user's Claude
/// subscription and its built-in web tools.
struct ClaudeCodeProvider {
    binary: PathBuf,
}

pub(crate) fn claude_code_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("AGENTDICTATE_CLAUDE_BINARY") {
        return Some(PathBuf::from(path));
    }
    let names: &[&str] = if cfg!(windows) {
        &["claude.exe"]
    } else {
        &["claude"]
    };
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            for name in names {
                let candidate = directory.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })?;
    let candidate = PathBuf::from(home).join(".local/bin").join(names[0]);
    candidate.is_file().then_some(candidate)
}

fn claude_code_workspace() -> PathBuf {
    let base = crate::AppPaths::from_environment()
        .map(|paths| paths.cache.join("copilot-claude"))
        .unwrap_or_else(|_| std::env::temp_dir().join("agentdictate-copilot-claude"));
    let _ = std::fs::create_dir_all(&base);
    base
}

impl AnswerProvider for ClaudeCodeProvider {
    fn stream(
        &self,
        request: &AnswerRequest,
        cancel: &AtomicBool,
        sink: &mut dyn FnMut(StreamEvent),
    ) -> Result<(), String> {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let tools = if request.web_search {
            "WebSearch,WebFetch"
        } else {
            ""
        };
        let mut command = Command::new(&self.binary);
        command
            .args([
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
                "--no-session-persistence",
                "--strict-mcp-config",
                "--setting-sources",
                "",
                "--permission-prompts",
                "none",
                "--model",
                &request.model,
                "--effort",
                request.effort.at_least_low().as_str(),
                "--tools",
                tools,
                "--allowedTools",
                tools,
            ])
            .current_dir(claude_code_workspace())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // Instructions primed with the user's knowledge outgrow a command
        // line (32,767 characters on Windows), so they go in a file.
        if request.system.len() > SYSTEM_PROMPT_ARGUMENT_CHARS {
            let mut hasher = std::hash::DefaultHasher::new();
            std::hash::Hash::hash(&request.system, &mut hasher);
            let file = claude_code_workspace().join(format!(
                "system-{:016x}.md",
                std::hash::Hasher::finish(&hasher)
            ));
            std::fs::write(&file, &request.system)
                .map_err(|error| format!("Could not prepare Claude Code: {error}"))?;
            command.arg("--system-prompt-file").arg(file);
        } else {
            command.args(["--system-prompt", &request.system]);
        }
        for (name, _) in std::env::vars_os() {
            let name = name.to_string_lossy().to_string();
            if name.starts_with("CLAUDE_CODE_")
                || name == "CLAUDECODE"
                || name == "ANTHROPIC_BASE_URL"
                || name == "ANTHROPIC_AUTH_TOKEN"
            {
                command.env_remove(name);
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("Could not start Claude Code: {error}"))?;
        let mut content = Vec::new();
        if let Some(image) = &request.image_jpeg {
            content.push(json!({
                "type": "image",
                "source": {"type": "base64", "media_type": "image/jpeg", "data": STANDARD.encode(image.as_slice())},
            }));
        }
        content.push(json!({"type": "text", "text": request.user}));
        let message = json!({"type": "user", "message": {"role": "user", "content": content}});
        if let Some(mut stdin) = child.stdin.take() {
            let written = writeln!(stdin, "{message}");
            drop(stdin);
            if let Err(error) = written {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "Could not send the request to Claude Code: {error}"
                ));
            }
        }
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "Claude Code output is unavailable".to_owned())?;
        let (lines, incoming) = mpsc::channel();
        std::thread::Builder::new()
            .name("copilot-claude-code".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    if lines.send(line).is_err() {
                        break;
                    }
                }
            })
            .map_err(|error| error.to_string())?;
        let mut parser = ClaudeCodeStream::default();
        let result = loop {
            if cancel.load(Ordering::Relaxed) {
                break Ok(());
            }
            match incoming.recv_timeout(Duration::from_millis(100)) {
                Ok(line) => {
                    let Ok(value) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    let mut finished = None;
                    for event in parser.parse(&value) {
                        match event {
                            StreamEvent::Error(message) => finished = Some(Err(message)),
                            StreamEvent::Done => {
                                sink(StreamEvent::Done);
                                finished = Some(Ok(()));
                            }
                            event => sink(event),
                        }
                    }
                    if let Some(result) = finished {
                        break result;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break Err("Claude Code ended without an answer.".to_owned());
                }
            }
        };
        let _ = child.kill();
        let _ = child.wait();
        result
    }
}

/// Transcribes one WAV utterance with the OpenAI API.
pub(crate) fn transcribe_wav_with_openai(
    client: &reqwest::blocking::Client,
    api_key: &str,
    wav: Vec<u8>,
    language: &str,
) -> Result<String, String> {
    let file = reqwest::blocking::multipart::Part::bytes(wav)
        .file_name("utterance.wav")
        .mime_str("audio/wav")
        .map_err(|error| error.to_string())?;
    let mut form = reqwest::blocking::multipart::Form::new()
        .part("file", file)
        .text("model", "gpt-transcribe")
        .text("response_format", "json");
    if !language.trim().is_empty() {
        form = form.text("language", language.trim().to_owned());
    }
    let response = client
        .post(OPENAI_TRANSCRIPTION_ENDPOINT)
        .bearer_auth(api_key)
        .multipart(form)
        .send()
        .map_err(|error| format!("Could not reach OpenAI transcription: {error}"))?;
    let status = response.status();
    let body = response.text().unwrap_or_default();
    if !status.is_success() {
        return Err(http_failure("OpenAI transcription", status, &body));
    }
    Ok(serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|value| value.get("text").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_default()
        .trim()
        .to_owned())
}

/// OpenRouter's Responses endpoint, which reaches models from many hosts.
const OPENROUTER_RESPONSES_ENDPOINT: &str = "https://openrouter.ai/api/v1/responses";

/// Answers from a model on OpenRouter, for comparing models' speed.
pub(crate) fn openrouter_provider(key: &str) -> Box<dyn AnswerProvider> {
    Box::new(ResponsesProvider {
        client: http_client(),
        endpoint: OPENROUTER_RESPONSES_ENDPOINT,
        auth: ResponsesAuth::ApiKey(key.to_owned()),
        fallback_models: Vec::new(),
        deadlines: None,
    })
}

/// The prefix of an OpenRouter API key.
pub(crate) const OPENROUTER_KEY_PREFIX: &str = "sk-or-";

/// Collects a whole non-interactive answer (insights, notes).
pub(crate) fn complete(
    provider: &dyn AnswerProvider,
    request: &AnswerRequest,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let mut text = String::new();
    provider.stream(request, cancel, &mut |event| {
        if let StreamEvent::Text(delta) = event {
            text.push_str(&delta);
        }
    })?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_model_that_must_reason_is_asked_for_low_effort_from_then_on() {
        let refusal = "{\"error\":{\"message\":\"Unsupported value: 'none' is not supported with the 'test-thinker' model. Supported values are: 'low', 'medium'.\"}}";
        assert!(is_effort_rejection(StatusCode::BAD_REQUEST, refusal));
        assert!(!is_effort_rejection(
            StatusCode::BAD_REQUEST,
            "{\"error\":{\"message\":\"The model does not exist\"}}"
        ));
        assert!(!is_effort_rejection(StatusCode::TOO_MANY_REQUESTS, refusal));

        let mut request = request();
        request.effort = CopilotEffort::None;
        assert_eq!(
            responses_body(&request, "test-thinker")["reasoning"]["effort"],
            "none"
        );
        MUST_REASON.lock().unwrap().push("test-thinker".into());
        assert_eq!(
            responses_body(&request, "test-thinker")["reasoning"]["effort"],
            "low"
        );
        assert_eq!(
            responses_body(&request, "test-quick")["reasoning"]["effort"],
            "none"
        );
        // Claude's least effort is low.
        assert_eq!(
            anthropic_body(&request, true)["output_config"]["effort"],
            "low"
        );
    }

    #[test]
    fn instant_claude_answers_skip_thinking_where_the_model_allows() {
        let mut request = request();
        request.effort = CopilotEffort::None;
        let thinking = |request: &AnswerRequest| anthropic_body(request, true)["thinking"].clone();
        request.model = "claude-haiku-5-5".into();
        assert_eq!(thinking(&request)["type"], "disabled");
        request.model = "claude-sonnet-5-5".into();
        assert_eq!(thinking(&request)["type"], "between_tools");
        // Opus cannot stop thinking, and the plain retry sends no thinking.
        request.model = "claude-opus-5-5".into();
        assert!(thinking(&request).is_null());
        request.model = "claude-haiku-5-5".into();
        assert!(anthropic_body(&request, false)["thinking"].is_null());
        // Full answers keep thinking.
        request.effort = CopilotEffort::Low;
        assert!(thinking(&request).is_null());
    }

    #[test]
    fn a_decision_asks_one_yes_or_no_question_and_reads_its_probability() {
        let body = decision_body(&Decision {
            rules: "Rules".into(),
            evidence: "Transcript".into(),
            question: "Answer it?".into(),
            cache_key: "session-turns".into(),
        });
        assert_eq!(body["model"], "gpt-6-luna");
        assert_eq!(body["input"][0]["role"], "developer");
        assert_eq!(body["input"][0]["content"][0]["text"], "Rules");
        assert_eq!(body["input"][1]["content"][0]["text"], "Transcript");
        assert_eq!(body["questions"][0]["type"], "predicate");
        assert_eq!(body["questions"][0]["instructions"], "Answer it?");

        let probability = parse_decision(
            r#"{"answers":[{"type":"predicate","name":"answer","probability":0.92}]}"#,
        );
        assert!((probability.unwrap() - 0.92).abs() < 1e-6);
        assert!(
            parse_decision(r#"{"answers":[{"type":"refusal","name":"answer"}]}"#)
                .unwrap_err()
                .contains("refusal")
        );
        assert!(parse_decision("{}").is_err());
    }

    #[test]
    fn a_turn_is_judged_by_the_decision_service_when_there_is_one() {
        struct Decider(Option<Result<f32, String>>);
        impl AnswerProvider for Decider {
            fn stream(
                &self,
                _request: &AnswerRequest,
                _cancel: &AtomicBool,
                sink: &mut dyn FnMut(StreamEvent),
            ) -> Result<(), String> {
                sink(StreamEvent::Text(r#"{"answer": false}"#.into()));
                Ok(())
            }
            fn decide(&self, decision: &Decision) -> Option<Result<f32, String>> {
                assert!(!decision.rules.contains("Respond with ONLY"));
                self.0.clone()
            }
        }
        let cancel = AtomicBool::new(false);
        let judged = |decided| judge_turn(&Decider(decided), &request(), &cancel).unwrap();
        assert_eq!(judged(Some(Ok(0.9))).respond, Some(true));
        assert_eq!(judged(Some(Ok(0.1))).respond, Some(false));
        // Without the service, or when it fails, the model is asked.
        let asked = judged(None);
        assert_eq!((asked.respond, asked.probability), (Some(false), None));
        assert_eq!(judged(Some(Err("down".into()))).respond, Some(false));
    }

    #[test]
    fn a_rejected_model_maps_to_the_one_that_answered() {
        remember_replacements(&["test-gone-model".to_owned()], "test-working-model");
        assert_eq!(
            replacement_for("test-gone-model").as_deref(),
            Some("test-working-model")
        );
        assert_eq!(replacement_for("test-other-model"), None);
    }

    #[test]
    fn rejected_models_are_tried_last() {
        remember_rejected("test-rejected-model");
        let mut models = vec![
            "test-rejected-model".to_owned(),
            "test-fallback-a".to_owned(),
            "test-fallback-b".to_owned(),
        ];
        prefer_accepted(&mut models);
        assert_eq!(
            models,
            ["test-fallback-a", "test-fallback-b", "test-rejected-model"]
        );
    }

    /// Runs the signed-in claude CLI: `cargo test -p agentdictate-app --lib
    /// long_system_prompts_reach_claude_code -- --ignored`.
    #[test]
    #[ignore = "needs a signed-in claude CLI"]
    fn long_system_prompts_reach_claude_code() {
        let provider = ClaudeCodeProvider {
            binary: claude_code_binary().expect("claude CLI"),
        };
        let mut system = "Answer with the codeword from the notes below.\n".to_owned();
        system.push_str(&"Filler note about nothing in particular. ".repeat(400));
        system.push_str("\nThe codeword is PERIWINKLE.");
        assert!(system.len() > SYSTEM_PROMPT_ARGUMENT_CHARS);
        let request = AnswerRequest {
            system,
            user: "What is the codeword?".into(),
            image_jpeg: None,
            model: "haiku".into(),
            effort: CopilotEffort::Low,
            web_search: false,
            max_output_tokens: 64,
            cache_key: "test".into(),
        };
        let cancel = AtomicBool::new(false);
        let text = complete(&provider, &request, &cancel).unwrap();
        assert!(text.to_uppercase().contains("PERIWINKLE"), "{text}");
    }

    fn request() -> AnswerRequest {
        AnswerRequest {
            system: "system".into(),
            user: "user".into(),
            image_jpeg: Some(Arc::new(vec![1, 2, 3])),
            model: "claude-opus-5-5".into(),
            effort: CopilotEffort::Low,
            web_search: true,
            max_output_tokens: 2048,
            cache_key: "session".into(),
        }
    }

    /// A local service that takes requests and answers each with
    /// `reply`, then holds the connection open without another byte.
    fn stalling_service(reply: &'static str) -> (&'static str, mpsc::Receiver<()>) {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint: &'static str = Box::leak(
            format!("http://{}/v1/responses", listener.local_addr().unwrap()).into_boxed_str(),
        );
        let (accepted, connections) = mpsc::channel();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming().take(4) {
                let Ok(mut stream) = stream else { return };
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => request.extend_from_slice(&buffer[..read]),
                    }
                }
                let _ = stream.write_all(reply.as_bytes());
                let _ = accepted.send(());
                held.push(stream);
            }
            std::thread::sleep(Duration::from_secs(5));
        });
        (endpoint, connections)
    }

    fn stalling_provider(endpoint: &'static str, deadlines: Deadlines) -> ResponsesProvider {
        ResponsesProvider {
            client: http_client(),
            endpoint,
            auth: ResponsesAuth::ApiKey("test-key".into()),
            fallback_models: vec![],
            deadlines: Some(deadlines),
        }
    }

    const STREAM_HEADERS: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\ndata: {\"type\":\"response.created\"}\n\n";

    #[test]
    fn a_stream_that_stalls_after_its_headers_fails_within_the_idle_deadline() {
        let (endpoint, _connections) = stalling_service(STREAM_HEADERS);
        let provider = stalling_provider(
            endpoint,
            Deadlines {
                first_byte: Duration::from_secs(2),
                idle: Duration::from_millis(300),
            },
        );
        let started = Instant::now();
        let result = provider.stream(&request(), &AtomicBool::new(false), &mut |_| {});
        let error = result.expect_err("a stalled stream fails");
        assert!(error.contains("stalled"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_service_that_never_replies_is_tried_once_more_then_given_up() {
        let (endpoint, connections) = stalling_service("");
        let provider = stalling_provider(
            endpoint,
            Deadlines {
                first_byte: Duration::from_millis(300),
                idle: Duration::from_secs(5),
            },
        );
        let started = Instant::now();
        let error = provider
            .stream(&request(), &AtomicBool::new(false), &mut |_| {})
            .expect_err("no reply fails");
        assert!(error.starts_with("Could not reach OpenAI"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        // The retry went out on a connection of its own.
        for _ in 0..2 {
            connections.recv_timeout(Duration::from_secs(2)).unwrap();
        }
    }

    #[test]
    fn cancelling_a_stalled_stream_returns_at_once() {
        let (endpoint, _connections) = stalling_service(STREAM_HEADERS);
        let provider = stalling_provider(
            endpoint,
            Deadlines {
                first_byte: Duration::from_secs(5),
                idle: Duration::from_secs(30),
            },
        );
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let cancel = Arc::clone(&cancel);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                cancel.store(true, Ordering::Relaxed);
            });
        }
        let started = Instant::now();
        provider.stream(&request(), &cancel, &mut |_| {}).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn researched_answers_get_longer_deadlines_than_quick_ones() {
        let mut quick = request();
        quick.web_search = false;
        quick.image_jpeg = None;
        quick.max_output_tokens = 200;
        assert_eq!(deadlines_for(&quick), QUICK_DEADLINES);
        quick.image_jpeg = request().image_jpeg;
        assert_eq!(
            deadlines_for(&quick),
            RESEARCH_DEADLINES,
            "a screenshot uploads"
        );
        assert_eq!(deadlines_for(&request()), RESEARCH_DEADLINES);
    }

    #[test]
    fn cancelled_requests_do_not_start_http_or_cli_work() {
        let providers: Vec<Box<dyn AnswerProvider>> = vec![
            Box::new(ResponsesProvider {
                client: http_client(),
                endpoint: "http://127.0.0.1:1",
                auth: ResponsesAuth::ApiKey("unused".into()),
                fallback_models: vec![],
                deadlines: None,
            }),
            Box::new(AnthropicProvider {
                client: http_client(),
                api_key: "unused".into(),
                decider: None,
                deadlines: None,
            }),
            Box::new(ClaudeCodeProvider {
                binary: "missing-claude-must-not-start".into(),
            }),
        ];
        for provider in providers {
            provider
                .stream(&request(), &AtomicBool::new(true), &mut |_| {
                    panic!("cancelled request emitted an event")
                })
                .unwrap();
        }
    }

    #[test]
    fn claude_judges_with_openai_decisions_only_when_an_openai_key_is_saved() {
        let mut settings = CopilotSettings {
            provider: CopilotProvider::AnthropicApi,
            ..CopilotSettings::default()
        };
        assert!(openai_decider(&settings, "").is_none());
        // Dictation's key serves too.
        assert!(openai_decider(&settings, "sk-dictation").is_some());
        settings.openai_api_key = "sk-copilot".into();
        let decider = openai_decider(&settings, "").unwrap();
        assert_eq!(decider.endpoint, OPENAI_RESPONSES_ENDPOINT);
        assert!(matches!(decider.auth, ResponsesAuth::ApiKey(ref key) if key == "sk-copilot"));
    }

    #[test]
    fn responses_body_carries_image_search_and_effort() {
        let body = responses_body(&request(), "gpt-6.1-sol");
        assert_eq!(body["model"], "gpt-6.1-sol");
        assert_eq!(body["instructions"], "system");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_image");
        assert!(
            body["input"][0]["content"][0]["image_url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/jpeg;base64,AQID")
        );
        assert_eq!(body["input"][0]["content"][1]["text"], "user");
        assert_eq!(body["tools"][0]["type"], "web_search");
        assert_eq!(body["reasoning"]["effort"], "low");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn anthropic_body_uses_basic_search_and_optional_fields() {
        let full = anthropic_body(&request(), true);
        assert_eq!(full["tools"][0]["type"], "web_search_20250305");
        assert_eq!(full["output_config"]["effort"], "low");
        assert_eq!(full["fallbacks"], "default");
        assert_eq!(
            full["messages"][0]["content"][0]["source"]["media_type"],
            "image/jpeg"
        );
        let plain = anthropic_body(&request(), false);
        assert!(plain.get("output_config").is_none());
        assert!(plain.get("fallbacks").is_none());
        let mut haiku = request();
        haiku.model = "claude-haiku-5-5".into();
        assert_eq!(
            anthropic_body(&haiku, true)["tools"][0]["type"],
            "web_search_20250305"
        );
        assert!(anthropic_body(&haiku, true).get("fallbacks").is_none());
    }

    #[test]
    fn model_rejections_are_recognized_for_fallback() {
        assert!(is_model_rejection(
            StatusCode::BAD_REQUEST,
            r#"{"detail":"The 'gpt-x' model is not supported"}"#
        ));
        assert!(!is_model_rejection(StatusCode::BAD_REQUEST, "bad image"));
        assert!(!is_model_rejection(StatusCode::TOO_MANY_REQUESTS, "model"));
    }

    #[test]
    fn cache_keys_fit_the_services_limit() {
        assert_eq!(cache_key("copilot-short"), "copilot-short");
        let long = cache_key(&"question-eval-".repeat(10));
        assert_eq!(long.len(), 64);
        assert_eq!(long, cache_key(&"question-eval-".repeat(10)));
        assert_ne!(long, cache_key(&"question-eval-".repeat(11)));
    }

    #[test]
    fn http_failures_summarize_status_and_detail() {
        let message = http_failure(
            "ChatGPT",
            StatusCode::TOO_MANY_REQUESTS,
            r#"{"error":{"message":"slow down"}}"#,
        );
        assert_eq!(
            message,
            "ChatGPT rate limit reached. Try again shortly. slow down"
        );
        let exhausted = http_failure(
            "ChatGPT",
            StatusCode::TOO_MANY_REQUESTS,
            r#"{"error":{"message":"The usage limit has been reached","resets_in_seconds":12000}}"#,
        );
        assert!(
            exhausted.ends_with("It resets in about 3 h 20 min."),
            "{exhausted}"
        );
    }
}
