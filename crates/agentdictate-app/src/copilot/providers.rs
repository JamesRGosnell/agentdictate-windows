//! Streaming answer providers for the meeting copilot.

use std::{
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
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

pub(crate) trait AnswerProvider: Send + Sync {
    /// Streams the answer into `sink`. Returns once the answer completes,
    /// fails, or `cancel` is observed.
    fn stream(
        &self,
        request: &AnswerRequest,
        cancel: &AtomicBool,
        sink: &mut dyn FnMut(StreamEvent),
    ) -> Result<(), String>;
}

pub(crate) fn http_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(240))
        .build()
        .expect("the rustls HTTP client must be constructible")
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
        })),
        CopilotProvider::OpenaiApi => {
            let key = if settings.openai_api_key.trim().is_empty() {
                fallback_openai_key.trim()
            } else {
                settings.openai_api_key.trim()
            };
            if key.is_empty() {
                return Err("Add an OpenAI API key in Copilot settings.".to_owned());
            }
            Ok(Box::new(ResponsesProvider {
                client: http_client(),
                endpoint: OPENAI_RESPONSES_ENDPOINT,
                auth: ResponsesAuth::ApiKey(key.to_owned()),
                fallback_models: Vec::new(),
            }))
        }
        CopilotProvider::AnthropicApi => {
            let key = settings.anthropic_api_key.trim();
            if key.is_empty() {
                return Err(
                    "Add an Anthropic API key in Copilot settings to use Claude API.".to_owned(),
                );
            }
            Ok(Box::new(AnthropicProvider {
                client: http_client(),
                api_key: key.to_owned(),
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
    json!({
        "model": model,
        "instructions": request.system,
        "input": [{"type": "message", "role": "user", "content": content}],
        "tools": tools,
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "reasoning": {"effort": request.effort.as_str(), "summary": "auto"},
        "store": false,
        "stream": true,
        "include": [],
        "prompt_cache_key": request.cache_key,
    })
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
    if detail.is_empty() {
        summary
    } else {
        format!("{summary} {}", detail.chars().take(240).collect::<String>())
    }
}

/// Reads an SSE body, forwarding mapped events until completion.
fn pump_sse(
    response: reqwest::blocking::Response,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(StreamEvent),
    mut map: impl FnMut(&Value) -> Vec<StreamEvent>,
) -> Result<(), String> {
    let mut reader = BufReader::new(response);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let Some(message) = next_sse_message(&mut reader)
            .map_err(|error| format!("Stream interrupted: {error}"))?
        else {
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
        let service = match self.auth {
            ResponsesAuth::ChatGpt(_) => "ChatGPT",
            ResponsesAuth::ApiKey(_) => "OpenAI",
        };
        let mut refreshed = false;
        let mut index = 0;
        while index < models.len() {
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
            let response = builder
                .send()
                .map_err(|error| format!("Could not reach {service}: {error}"))?;
            let status = response.status();
            if status.is_success() {
                return pump_sse(response, cancel, sink, parse_responses_event);
            }
            let body = response.text().unwrap_or_default();
            if status == StatusCode::UNAUTHORIZED
                && matches!(self.auth, ResponsesAuth::ChatGpt(_))
                && !refreshed
            {
                refreshed = true;
                continue;
            }
            if is_model_rejection(status, &body) && index + 1 < models.len() {
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
}

struct AnthropicProvider {
    client: reqwest::blocking::Client,
    api_key: String,
}

fn anthropic_search_tool(model: &str) -> Value {
    let dynamic = [
        "opus-5",
        "opus-4-6",
        "opus-4-7",
        "opus-4-8",
        "sonnet-5",
        "sonnet-4-6",
    ]
    .iter()
    .any(|family| model.contains(family));
    let kind = if dynamic {
        "web_search_20260209"
    } else {
        "web_search_20250305"
    };
    json!({"type": kind, "name": "web_search", "max_uses": 3})
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
        body["output_config"] = json!({"effort": request.effort.as_str()});
        if supports_server_fallback(&request.model) {
            body["fallbacks"] = json!("default");
        }
    }
    body
}

fn supports_server_fallback(model: &str) -> bool {
    [
        "claude-opus-5-5",
        "claude-opus-5",
        "claude-fable-5-1",
        "claude-sonnet-5-5",
    ]
    .contains(&model)
}

impl AnswerProvider for AnthropicProvider {
    fn stream(
        &self,
        request: &AnswerRequest,
        cancel: &AtomicBool,
        sink: &mut dyn FnMut(StreamEvent),
    ) -> Result<(), String> {
        // The first attempt opts into effort control and server-side refusal
        // fallbacks; a plain request is retried if the account rejects them.
        for full in [true, false] {
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
            let response = builder
                .send()
                .map_err(|error| format!("Could not reach Anthropic: {error}"))?;
            let status = response.status();
            if status.is_success() {
                let mut stream = AnthropicStream::default();
                return pump_sse(response, cancel, sink, |value| stream.parse(value));
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
                request.effort.as_str(),
                "--tools",
                tools,
                "--allowedTools",
                tools,
                "--system-prompt",
                &request.system,
            ])
            .current_dir(claude_code_workspace())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
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
        .text("model", "gpt-4o-transcribe")
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
    fn anthropic_body_uses_dynamic_search_and_optional_fields() {
        let full = anthropic_body(&request(), true);
        assert_eq!(full["tools"][0]["type"], "web_search_20260209");
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
    }
}
