use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use agentdictate_core::{DictationOptions, JobId};
use agentdictate_runtime::{ExternalError, ExternalFailureKind, RecordingJob};
use base64::Engine as _;
use reqwest::StatusCode;
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

use crate::openai::{SpeechTransport, TranscriptionRequest};

const CHATGPT_TRANSCRIPTION_ENDPOINT: &str = "https://chatgpt.com/backend-api/transcribe";
const CODEX_DESKTOP_ORIGINATOR: &str = "Codex Desktop";
const CODEX_DESKTOP_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36";
const APP_SERVER_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_TIMEOUT: Duration = Duration::from_secs(180);

struct ChatGptAccessToken(String);

impl ChatGptAccessToken {
    fn expose(&self) -> &str {
        &self.0
    }
}

struct ChatGptAccountId(String);

impl ChatGptAccountId {
    fn expose(&self) -> &str {
        &self.0
    }
}

struct ChatGptAuth {
    access_token: ChatGptAccessToken,
    account_id: ChatGptAccountId,
}

trait ChatGptAuthProvider: Send {
    fn load(&mut self, refresh: bool) -> Result<ChatGptAuth, CodexSubscriptionError>;
}

struct CodexAppServerAuthProvider {
    codex_binary: OsString,
}

impl CodexAppServerAuthProvider {
    fn discover() -> Self {
        let binary = std::env::var_os("CODEX_BINARY")
            .map(PathBuf::from)
            .or_else(windows_codex_binary)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".local/bin/codex"))
                    .filter(|path| path.is_file())
            })
            .or_else(|| {
                let bundled = PathBuf::from("/usr/lib/chatgpt/resources/codex");
                bundled.is_file().then_some(bundled)
            })
            .map_or_else(|| OsString::from("codex"), PathBuf::into_os_string);
        Self {
            codex_binary: binary,
        }
    }
}

impl ChatGptAuthProvider for CodexAppServerAuthProvider {
    fn load(&mut self, refresh: bool) -> Result<ChatGptAuth, CodexSubscriptionError> {
        load_auth_from_app_server(&self.codex_binary, refresh)
    }
}

#[derive(Debug, Error)]
enum CodexSubscriptionError {
    #[error("Codex could not be started. Install Codex and sign in with ChatGPT first.")]
    AppServerUnavailable,
    #[error("Codex did not respond while checking the ChatGPT sign-in.")]
    AppServerTimeout,
    #[error("Codex returned an invalid authentication response.")]
    InvalidAuthResponse,
    #[error("Codex is not signed in with ChatGPT.")]
    ChatGptSignInRequired,
    #[error(
        "Codex reports a ChatGPT sign-in but did not provide an access token. Reconnect ChatGPT in AgentDictate or check Codex compatibility."
    )]
    ChatGptTokenUnavailable,
    #[error("The ChatGPT sign-in does not include an account identifier.")]
    MissingAccountId,
    #[error("Could not read the captured recording: {0}")]
    AudioRead(std::io::Error),
    #[error("Could not reach ChatGPT transcription: {0}")]
    Transport(reqwest::Error),
    #[error("ChatGPT sign-in expired. Open Codex and sign in again.")]
    Unauthorized,
    #[error("ChatGPT subscription transcription is unavailable for this account.")]
    NotEntitled,
    #[error("ChatGPT transcription limit reached. Try again later.")]
    RateLimited,
    #[error("ChatGPT transcription is temporarily unavailable.")]
    ServiceUnavailable,
    #[error("ChatGPT transcription rejected the recording.")]
    RequestRejected,
    #[error("ChatGPT returned an invalid transcription response.")]
    InvalidResponse,
    #[error("No speech detected.")]
    EmptyTranscript,
}

impl CodexSubscriptionError {
    /// What the user can do about this failure, for the dictation notice.
    const fn failure_kind(&self) -> ExternalFailureKind {
        match self {
            Self::ChatGptSignInRequired
            | Self::ChatGptTokenUnavailable
            | Self::MissingAccountId
            | Self::Unauthorized => ExternalFailureKind::SignInRequired,
            Self::RateLimited => ExternalFailureKind::LimitReached,
            Self::EmptyTranscript => ExternalFailureKind::NothingRecognized,
            Self::AppServerUnavailable
            | Self::AppServerTimeout
            | Self::InvalidAuthResponse
            | Self::AudioRead(_)
            | Self::Transport(_)
            | Self::NotEntitled
            | Self::ServiceUnavailable
            | Self::RequestRejected
            | Self::InvalidResponse => ExternalFailureKind::Other,
        }
    }
}

fn reqwest_error_kind(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_body() {
        "body"
    } else if error.is_decode() {
        "decode"
    } else if error.is_redirect() {
        "redirect"
    } else if error.is_builder() {
        "request_builder"
    } else if error.is_request() {
        "request"
    } else {
        "transport"
    }
}

/// The request never got a reply because the connection failed. A request
/// that timed out has already waited minutes, so it is not sent again.
fn is_connection_failure(error: &CodexSubscriptionError) -> bool {
    matches!(error, CodexSubscriptionError::Transport(error) if !error.is_timeout())
}

/// A reply worth one more try: the service timed out, was busy, or briefly
/// limited requests. A used-up limit is not, nor any other reply.
fn is_retryable_reply(status: StatusCode, payload: &str) -> bool {
    let retryable = status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error();
    let payload = payload.to_lowercase();
    let limit_reached = [
        "limit reached",
        "limit has been reached",
        "usage_limit",
        "resets_in",
    ]
    .iter()
    .any(|pattern| payload.contains(pattern));
    retryable && !limit_reached
}

/// Between 300 and 700 ms, so dictations that failed together do not all
/// come back at the same moment.
fn retry_delay() -> Duration {
    let jitter = (Uuid::new_v4().as_u128() % 400) as u64;
    Duration::from_millis(300 + jitter)
}

fn subscription_error_kind(error: &CodexSubscriptionError) -> &'static str {
    match error {
        CodexSubscriptionError::Transport(error) => reqwest_error_kind(error),
        _ => "non_transport",
    }
}

pub struct CodexSubscriptionTransport {
    client: reqwest::blocking::Client,
    endpoint: String,
    auth_provider: Arc<Mutex<Box<dyn ChatGptAuthProvider>>>,
    prepared_auth: Option<PreparedAuth>,
}

struct PreparedAuth {
    job_id: Option<JobId>,
    audio_path: PathBuf,
    worker: thread::JoinHandle<Result<ChatGptAuth, CodexSubscriptionError>>,
}

impl CodexSubscriptionTransport {
    #[must_use]
    pub fn new() -> Self {
        Self {
            client: reqwest::blocking::Client::builder()
                .connect_timeout(Duration::from_secs(20))
                .timeout(HTTP_TIMEOUT)
                .build()
                .expect("the rustls HTTP client must be constructible"),
            endpoint: CHATGPT_TRANSCRIPTION_ENDPOINT.to_owned(),
            auth_provider: Arc::new(Mutex::new(Box::new(CodexAppServerAuthProvider::discover()))),
            prepared_auth: None,
        }
    }

    #[cfg(test)]
    fn with_endpoint_and_auth(
        endpoint: impl Into<String>,
        auth_provider: impl ChatGptAuthProvider + 'static,
    ) -> Self {
        Self {
            client: reqwest::blocking::Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(5))
                .build()
                .expect("the test HTTP client must be constructible"),
            endpoint: endpoint.into(),
            auth_provider: Arc::new(Mutex::new(Box::new(auth_provider))),
            prepared_auth: None,
        }
    }

    fn load_auth(&self, refresh: bool) -> Result<ChatGptAuth, CodexSubscriptionError> {
        self.auth_provider
            .lock()
            .map_err(|_| CodexSubscriptionError::InvalidAuthResponse)?
            .load(refresh)
    }

    fn prepare_auth(&mut self, job_id: JobId, audio_path: PathBuf) {
        // Keep at most one worker, even across rapid start/discard cycles.
        if self
            .prepared_auth
            .as_ref()
            .is_some_and(|pending| !pending.worker.is_finished())
        {
            return;
        }
        let provider = Arc::clone(&self.auth_provider);
        match thread::Builder::new()
            .name("dictation-subscription-auth".into())
            .spawn(move || {
                let started = Instant::now();
                let result = provider
                    .lock()
                    .map_err(|_| CodexSubscriptionError::InvalidAuthResponse)?
                    .load(false);
                tracing::info!(
                    %job_id,
                    auth_millis = started.elapsed().as_millis(),
                    success = result.is_ok(),
                    "ChatGPT transcription authentication prepared"
                );
                result
            }) {
            Ok(worker) => {
                self.prepared_auth = Some(PreparedAuth {
                    job_id: Some(job_id),
                    audio_path,
                    worker,
                });
            }
            Err(_) => tracing::warn!("could not prepare ChatGPT authentication during recording"),
        }
    }

    fn recording_auth(
        &mut self,
        audio_path: &std::path::Path,
    ) -> Result<ChatGptAuth, CodexSubscriptionError> {
        if self
            .prepared_auth
            .as_ref()
            .is_some_and(|pending| pending.job_id.is_some() && pending.audio_path == audio_path)
            && let Some(pending) = self.prepared_auth.take()
            && let Ok(Ok(auth)) = pending.worker.join()
        {
            // A long recording can outlive the token obtained at its start.
            let expires = parse_jwt_claims(auth.access_token.expose())
                .ok()
                .and_then(|claims| claims.get("exp").and_then(Value::as_u64));
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            if expires.is_some_and(|expires| expires <= now + 120) {
                return self.load_auth(true);
            }
            return Ok(auth);
        }
        // Recovery, cancelled jobs and failed preparation still use a fresh
        // foreground check. Credentials are never reused across recordings.
        if let Some(pending) = &mut self.prepared_auth {
            pending.job_id = None;
        }
        self.load_auth(false)
    }

    fn send(
        &self,
        auth: &ChatGptAuth,
        body: Vec<u8>,
        boundary: &str,
    ) -> Result<reqwest::blocking::Response, CodexSubscriptionError> {
        self.client
            .post(&self.endpoint)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", auth.access_token.expose()),
            )
            .header("ChatGPT-Account-Id", auth.account_id.expose())
            .header("originator", CODEX_DESKTOP_ORIGINATOR)
            .header(reqwest::header::USER_AGENT, CODEX_DESKTOP_USER_AGENT)
            .header(
                reqwest::header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body)
            .send()
            .map_err(CodexSubscriptionError::Transport)
    }

    fn transcribe(
        &mut self,
        request: &TranscriptionRequest<'_>,
    ) -> Result<String, CodexSubscriptionError> {
        let request_started_at = Instant::now();
        let audio = std::fs::read(request.audio_path).map_err(CodexSubscriptionError::AudioRead)?;
        let audio_read_millis = request_started_at.elapsed().as_millis();
        let boundary = format!("----codex-transcribe-{}", Uuid::new_v4());

        let auth_started = Instant::now();
        let mut auth = self.recording_auth(request.audio_path)?;
        let auth_wait_millis = auth_started.elapsed().as_millis();
        let mut initial_status = None;
        let mut auth_refreshed = false;
        // Each kind of retry happens at most once.
        let mut reconnected = false;
        let mut retried_status = false;
        let (status, payload) = loop {
            let attempt = if auth_refreshed {
                "after_auth_refresh"
            } else if reconnected || retried_status {
                "retry"
            } else {
                "initial"
            };
            let body = multipart_body(&boundary, &audio, request.language);
            let response = match self.send(&auth, body, &boundary) {
                Ok(response) => response,
                Err(error) => {
                    tracing::warn!(
                        attempt,
                        error_kind = subscription_error_kind(&error),
                        audio_bytes = audio.len(),
                        recording_duration_seconds = request.duration_seconds,
                        elapsed_millis = request_started_at.elapsed().as_millis(),
                        "ChatGPT transcription request failed before receiving a response"
                    );
                    // The connection failed before any reply: the service
                    // never saw the recording, so it is sent once more.
                    if !reconnected && is_connection_failure(&error) {
                        reconnected = true;
                        continue;
                    }
                    return Err(error);
                }
            };
            let status = response.status();
            initial_status.get_or_insert(status);
            if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) && !auth_refreshed
            {
                auth_refreshed = true;
                auth = self.load_auth(true)?;
                continue;
            }
            let payload = response
                .text()
                .inspect_err(|error| {
                    tracing::warn!(
                        attempt = "response_body",
                        error_kind = reqwest_error_kind(error),
                        http_status = status.as_u16(),
                        audio_bytes = audio.len(),
                        recording_duration_seconds = request.duration_seconds,
                        elapsed_millis = request_started_at.elapsed().as_millis(),
                        "ChatGPT transcription response body could not be read"
                    );
                })
                .map_err(CodexSubscriptionError::Transport)?;
            if !retried_status && is_retryable_reply(status, &payload) {
                retried_status = true;
                let delay = retry_delay();
                tracing::warn!(
                    http_status = status.as_u16(),
                    delay_millis = delay.as_millis(),
                    "ChatGPT transcription is briefly unavailable; retrying once"
                );
                thread::sleep(delay);
                continue;
            }
            break (status, payload);
        };
        let initial_status = initial_status.unwrap_or(status);
        tracing::info!(
            initial_http_status = initial_status.as_u16(),
            final_http_status = status.as_u16(),
            http_success = status.is_success(),
            auth_refreshed,
            reconnected,
            retried_status,
            audio_read_millis,
            auth_wait_millis,
            audio_bytes = audio.len(),
            recording_duration_seconds = request.duration_seconds,
            elapsed_millis = request_started_at.elapsed().as_millis(),
            "ChatGPT transcription response received"
        );
        if !status.is_success() {
            return Err(match status {
                StatusCode::UNAUTHORIZED => CodexSubscriptionError::Unauthorized,
                StatusCode::FORBIDDEN => CodexSubscriptionError::NotEntitled,
                StatusCode::TOO_MANY_REQUESTS => CodexSubscriptionError::RateLimited,
                StatusCode::REQUEST_TIMEOUT
                | StatusCode::INTERNAL_SERVER_ERROR
                | StatusCode::BAD_GATEWAY
                | StatusCode::SERVICE_UNAVAILABLE
                | StatusCode::GATEWAY_TIMEOUT => CodexSubscriptionError::ServiceUnavailable,
                _ => CodexSubscriptionError::RequestRejected,
            });
        }
        let text = serde_json::from_str::<Value>(&payload)
            .ok()
            .and_then(|payload| {
                payload
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .ok_or(CodexSubscriptionError::InvalidResponse)?;
        if text.trim().is_empty() {
            return Err(CodexSubscriptionError::EmptyTranscript);
        }
        Ok(text.trim().to_owned())
    }
}

impl Default for CodexSubscriptionTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl SpeechTransport for CodexSubscriptionTransport {
    fn begin_recording(&mut self, job: &RecordingJob, options: &DictationOptions) {
        if !options.language.contains(',') {
            self.prepare_auth(job.id, job.audio_path.clone());
        }
    }

    fn cancel_recording(&mut self, id: JobId) {
        if let Some(pending) = &mut self.prepared_auth
            && pending.job_id == Some(id)
        {
            // Let the bounded auth call finish without blocking the shortcut.
            pending.job_id = None;
        }
    }

    fn transcribe_audio(
        &mut self,
        request: TranscriptionRequest<'_>,
    ) -> Result<String, ExternalError> {
        if request.language.contains(',') {
            return Err(ExternalError::new(
                "ChatGPT subscription accepts one language hint; choose one language or automatic detection",
            ));
        }
        self.transcribe(&request).map_err(|error| match error {
            CodexSubscriptionError::EmptyTranscript => ExternalError::NoSpeech,
            error => ExternalError::with_kind(error.failure_kind(), error.to_string()),
        })
    }
}

fn multipart_body(boundary: &str, audio: &[u8], language: &str) -> Vec<u8> {
    let mut body = Vec::with_capacity(audio.len() + 320);
    write!(
        body,
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"codex.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
    )
    .expect("writing to a Vec cannot fail");
    body.extend_from_slice(audio);
    let language = language.trim();
    if language.is_empty() {
        write!(body, "\r\n--{boundary}--\r\n").expect("writing to a Vec cannot fail");
    } else {
        write!(
            body,
            "\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"language\"\r\n\r\n{language}\r\n--{boundary}--\r\n"
        )
        .expect("writing to a Vec cannot fail");
    }
    body
}

fn load_auth_from_app_server(
    codex_binary: &OsString,
    refresh: bool,
) -> Result<ChatGptAuth, CodexSubscriptionError> {
    let result = call_app_server(
        codex_binary,
        "getAuthStatus",
        json!({"includeToken": true, "refreshToken": refresh}),
    )?;
    parse_chatgpt_auth(&result)
}

/// Runs one JSON-RPC request against a short-lived `codex app-server`.
fn call_app_server(
    codex_binary: &OsString,
    method: &str,
    params: Value,
) -> Result<Value, CodexSubscriptionError> {
    let mut command = Command::new(codex_binary);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command
        .args(["app-server", "--stdio"])
        .env_remove("OPENAI_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    if let Some(home) = windows_auth_home() {
        command.env("CODEX_HOME", home);
    }
    let mut child = command
        .spawn()
        .map_err(|_| CodexSubscriptionError::AppServerUnavailable)?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or(CodexSubscriptionError::AppServerUnavailable)?;
    let stdout = child
        .stdout
        .take()
        .ok_or(CodexSubscriptionError::AppServerUnavailable)?;
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Ok(message) = serde_json::from_str::<Value>(&line)
                && sender.send(message).is_err()
            {
                break;
            }
        }
    });

    let result = (|| {
        send_app_server_request(
            &mut stdin,
            1,
            "initialize",
            json!({
                "clientInfo": {
                    "name": "agentdictate",
                    "title": "AgentDictate",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        )?;
        receive_app_server_response(&receiver, 1)?;
        writeln!(stdin, "{}", json!({"method": "initialized", "params": {}}))
            .map_err(|_| CodexSubscriptionError::AppServerUnavailable)?;
        send_app_server_request(&mut stdin, 2, method, params)?;
        receive_app_server_response(&receiver, 2)
    })();

    drop(stdin);
    stop_child(&mut child);
    let _ = reader.join();
    result
}

fn send_app_server_request(
    stdin: &mut impl Write,
    id: u64,
    method: &str,
    params: Value,
) -> Result<(), CodexSubscriptionError> {
    writeln!(
        stdin,
        "{}",
        json!({"id": id, "method": method, "params": params})
    )
    .map_err(|_| CodexSubscriptionError::AppServerUnavailable)
}

fn receive_app_server_response(
    receiver: &Receiver<Value>,
    expected_id: u64,
) -> Result<Value, CodexSubscriptionError> {
    let deadline = Instant::now() + APP_SERVER_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(CodexSubscriptionError::AppServerTimeout);
        }
        let message = receiver
            .recv_timeout(remaining)
            .map_err(|_| CodexSubscriptionError::AppServerTimeout)?;
        if message.get("id").and_then(Value::as_u64) != Some(expected_id) {
            continue;
        }
        if message.get("error").is_some() {
            return Err(CodexSubscriptionError::InvalidAuthResponse);
        }
        return message
            .get("result")
            .cloned()
            .ok_or(CodexSubscriptionError::InvalidAuthResponse);
    }
}

fn parse_chatgpt_auth(result: &Value) -> Result<ChatGptAuth, CodexSubscriptionError> {
    if result.get("authMethod").and_then(Value::as_str) != Some("chatgpt") {
        return Err(CodexSubscriptionError::ChatGptSignInRequired);
    }
    let token = result
        .get("authToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or(CodexSubscriptionError::ChatGptTokenUnavailable)?;
    let claims = parse_jwt_claims(token)?;
    let account_id = claims
        .get("https://api.openai.com/auth")
        .and_then(Value::as_object)
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|account_id| !account_id.is_empty())
        .ok_or(CodexSubscriptionError::MissingAccountId)?;
    Ok(ChatGptAuth {
        access_token: ChatGptAccessToken(token.to_owned()),
        account_id: ChatGptAccountId(account_id.to_owned()),
    })
}

fn parse_jwt_claims(token: &str) -> Result<Value, CodexSubscriptionError> {
    let payload = token
        .split('.')
        .nth(1)
        .ok_or(CodexSubscriptionError::InvalidAuthResponse)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| CodexSubscriptionError::InvalidAuthResponse)?;
    serde_json::from_slice(&decoded).map_err(|_| CodexSubscriptionError::InvalidAuthResponse)
}

fn stop_child(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Long-lived ChatGPT credentials for the meeting copilot. Unlike one-shot
/// dictation, a meeting issues many requests, so the access token is cached
/// in memory until it nears expiry or the service rejects it.
pub(crate) struct ChatGptCredentials {
    provider: Mutex<Box<dyn ChatGptAuthProvider>>,
    codex_binary: OsString,
    cached: Mutex<Option<CachedChatGptAuth>>,
    /// The one load through Codex under way, which every lane waits on
    /// rather than starting its own.
    flight: Mutex<Flight>,
    landed: std::sync::Condvar,
}

/// Loads through Codex take seconds (up to about 20 s when it is slow to
/// start), so lanes that need credentials at once share a single load.
#[derive(Default)]
struct Flight {
    loading: bool,
    /// The load under way refreshes the token.
    refreshing: bool,
    /// Loads finished so far, so a waiter knows when the one it waited
    /// for has landed.
    finished: u64,
    /// The last load's failure, given to callers for a while instead of
    /// starting Codex again for each of them.
    failure: Option<(Instant, String)>,
}

/// How long a failed sign-in check is reported again without retrying.
const FAILURE_REMEMBERED: Duration = Duration::from_secs(30);

/// What a caller needs from [`ChatGptCredentials::obtain`].
#[derive(Clone, Copy)]
enum Wanted<'a> {
    /// Any token that has not expired.
    Current,
    /// A token other than this one, which the service turned down.
    Replacing(&'a str),
    /// A freshly refreshed token.
    Refreshed,
}

struct CachedChatGptAuth {
    token: String,
    account_id: String,
    expires_at_unix: Option<u64>,
}

/// A bearer token and the ChatGPT account it belongs to.
#[derive(Clone)]
pub(crate) struct ChatGptBearer {
    pub token: String,
    pub account_id: String,
}

impl ChatGptCredentials {
    pub(crate) fn discover() -> Self {
        let provider = CodexAppServerAuthProvider::discover();
        Self::with_provider(provider.codex_binary.clone(), Box::new(provider))
    }

    fn with_provider(codex_binary: OsString, provider: Box<dyn ChatGptAuthProvider>) -> Self {
        Self {
            codex_binary,
            provider: Mutex::new(provider),
            cached: Mutex::new(None),
            flight: Mutex::new(Flight::default()),
            landed: std::sync::Condvar::new(),
        }
    }

    fn cached_bearer(&self, now: u64) -> Option<ChatGptBearer> {
        let cached = self.cached.lock().ok()?;
        let cached = cached.as_ref()?;
        cached
            .expires_at_unix
            .is_none_or(|expires| expires > now + 120)
            .then(|| ChatGptBearer {
                token: cached.token.clone(),
                account_id: cached.account_id.clone(),
            })
    }

    /// Returns cached credentials, loading or refreshing them through Codex.
    pub(crate) fn bearer(&self, force_refresh: bool) -> Result<ChatGptBearer, String> {
        self.obtain(if force_refresh {
            Wanted::Refreshed
        } else {
            Wanted::Current
        })
    }

    /// Credentials to retry with after the service turned down `rejected`.
    /// When another lane has already replaced that token, its replacement
    /// is used without refreshing again.
    pub(crate) fn bearer_after_rejection(&self, rejected: &str) -> Result<ChatGptBearer, String> {
        self.obtain(Wanted::Replacing(rejected))
    }

    fn obtain(&self, wanted: Wanted<'_>) -> Result<ChatGptBearer, String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let suitable = |joined_refresh: bool| {
            let cached = self.cached_bearer(now)?;
            match wanted {
                Wanted::Current => Some(cached),
                Wanted::Replacing(rejected) => (cached.token != rejected).then_some(cached),
                Wanted::Refreshed => joined_refresh.then_some(cached),
            }
        };
        if let Some(bearer) = suitable(false) {
            return Ok(bearer);
        }
        let unavailable = || "ChatGPT sign-in state is unavailable".to_owned();
        let mut flight = self.flight.lock().map_err(|_| unavailable())?;
        let mut joined_refresh = false;
        loop {
            if let Some(bearer) = suitable(joined_refresh) {
                return Ok(bearer);
            }
            if let Some((at, error)) = &flight.failure
                && at.elapsed() < FAILURE_REMEMBERED
            {
                return Err(error.clone());
            }
            if !flight.loading {
                break;
            }
            // Another lane is loading: wait for it and use what it gets.
            let seen = flight.finished;
            joined_refresh = flight.refreshing;
            while flight.loading && flight.finished == seen {
                flight = self.landed.wait(flight).map_err(|_| unavailable())?;
            }
        }
        let refresh = !matches!(wanted, Wanted::Current);
        flight.loading = true;
        flight.refreshing = refresh;
        drop(flight);
        let result = self.load(refresh, now);
        let mut flight = self.flight.lock().map_err(|_| unavailable())?;
        flight.loading = false;
        flight.finished += 1;
        flight.failure = result
            .as_ref()
            .err()
            .map(|error| (Instant::now(), error.clone()));
        drop(flight);
        self.landed.notify_all();
        result
    }

    /// Loads credentials through Codex and caches them.
    fn load(&self, force_refresh: bool, now: u64) -> Result<ChatGptBearer, String> {
        let expiry = |auth: &ChatGptAuth| {
            parse_jwt_claims(auth.access_token.expose())
                .ok()
                .and_then(|claims| claims.get("exp").and_then(Value::as_u64))
        };
        let mut provider = self
            .provider
            .lock()
            .map_err(|_| "ChatGPT sign-in state is unavailable".to_owned())?;
        let mut auth = provider
            .load(force_refresh)
            .map_err(|error| error.to_string())?;
        // Codex can hand back a token that is about to lapse; refresh it now
        // rather than failing the next request.
        if !force_refresh && expiry(&auth).is_some_and(|expires| expires <= now + 120) {
            auth = provider.load(true).map_err(|error| error.to_string())?;
        }
        let expires_at_unix = expiry(&auth);
        let bearer = ChatGptBearer {
            token: auth.access_token.expose().to_owned(),
            account_id: auth.account_id.expose().to_owned(),
        };
        if let Ok(mut cached) = self.cached.lock() {
            *cached = Some(CachedChatGptAuth {
                token: bearer.token.clone(),
                account_id: bearer.account_id.clone(),
                expires_at_unix,
            });
        }
        // Publish the cache before releasing the provider to the next lane.
        drop(provider);
        Ok(bearer)
    }

    /// Lists models available to the signed-in account as `(id, label)`.
    pub(crate) fn list_models(&self) -> Result<Vec<(String, String)>, String> {
        let result = call_app_server(&self.codex_binary, "model/list", json!({}))
            .map_err(|error| error.to_string())?;
        Ok(parse_model_list(&result))
    }
}

/// Reads `(id, label)` pairs from a Codex `model/list` result.
pub(crate) fn parse_model_list(result: &Value) -> Vec<(String, String)> {
    let entries = result
        .get("data")
        .or_else(|| result.get("models"))
        .unwrap_or(result);
    entries
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let id = ["model", "id", "slug"]
                        .iter()
                        .find_map(|key| entry.get(*key).and_then(Value::as_str))?
                        .to_owned();
                    let label = ["displayName", "display_name", "name"]
                        .iter()
                        .find_map(|key| entry.get(*key).and_then(Value::as_str))
                        .map_or_else(|| id.clone(), str::to_owned);
                    Some((id, label))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Transcribes one in-memory WAV utterance through the ChatGPT dictation
/// endpoint, refreshing credentials once if they were rejected.
pub(crate) fn transcribe_wav_with_chatgpt(
    client: &reqwest::blocking::Client,
    credentials: &ChatGptCredentials,
    wav: &[u8],
    language: &str,
) -> Result<String, String> {
    let boundary = format!("----codex-transcribe-{}", Uuid::new_v4());
    let mut rejected: Option<String> = None;
    loop {
        let refreshed = rejected.is_some();
        let bearer = match &rejected {
            Some(rejected) => credentials.bearer_after_rejection(rejected)?,
            None => credentials.bearer(false)?,
        };
        let response = client
            .post(CHATGPT_TRANSCRIPTION_ENDPOINT)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", bearer.token),
            )
            .header("ChatGPT-Account-Id", &bearer.account_id)
            .header("originator", CODEX_DESKTOP_ORIGINATOR)
            .header(reqwest::header::USER_AGENT, CODEX_DESKTOP_USER_AGENT)
            .header(
                reqwest::header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(multipart_body(&boundary, wav, language))
            .send()
            .map_err(|error| CodexSubscriptionError::Transport(error).to_string())?;
        let status = response.status();
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) && !refreshed {
            rejected = Some(bearer.token);
            continue;
        }
        let payload = response
            .text()
            .map_err(|error| CodexSubscriptionError::Transport(error).to_string())?;
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(rate_limit_message(&payload));
        }
        if !status.is_success() {
            return Err(match status {
                StatusCode::UNAUTHORIZED => CodexSubscriptionError::Unauthorized,
                StatusCode::FORBIDDEN => CodexSubscriptionError::NotEntitled,
                StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => {
                    CodexSubscriptionError::RequestRejected
                }
                _ => CodexSubscriptionError::ServiceUnavailable,
            }
            .to_string());
        }
        return Ok(serde_json::from_str::<Value>(&payload)
            .ok()
            .and_then(|payload| {
                payload
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_default()
            .trim()
            .to_owned());
    }
}

/// What a rate-limited reply says, so a used-up plan ("resets in about 29
/// days") is told apart from a brief limit worth waiting out.
fn rate_limit_message(body: &str) -> String {
    let value = serde_json::from_str::<Value>(body).ok();
    let detail = value
        .as_ref()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.get("detail"))
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
        })
        .map(|detail| detail.trim().chars().take(240).collect::<String>())
        .filter(|detail| !detail.is_empty());
    let resets = value
        .as_ref()
        .and_then(|value| {
            value
                .pointer("/error/resets_in_seconds")
                .and_then(Value::as_u64)
        })
        .map(reset_label);
    let mut message = "ChatGPT transcription limit reached.".to_owned();
    if let Some(detail) = detail {
        message.push(' ');
        message.push_str(&detail);
        if !detail.ends_with(['.', '!', '?']) {
            message.push('.');
        }
    }
    match resets {
        Some(resets) => message.push_str(&format!(" It resets in about {resets}.")),
        None => message.push_str(" Try again later."),
    }
    message
}

/// "45 min", "3 h", or "29 days" until a limit resets.
fn reset_label(seconds: u64) -> String {
    let minutes = seconds.div_ceil(60).max(1);
    if minutes < 60 {
        format!("{minutes} min")
    } else if minutes < 48 * 60 {
        format!("{} h", minutes.div_ceil(60))
    } else {
        format!("{} days", minutes.div_ceil(24 * 60))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use agentdictate_core::TranscriptionProvider;
    use base64::Engine as _;
    use tempfile::tempdir;

    use super::*;

    struct FixedAuthProvider {
        calls: Arc<Mutex<Vec<bool>>>,
        responses: VecDeque<ChatGptAuth>,
    }

    impl ChatGptAuthProvider for FixedAuthProvider {
        fn load(&mut self, refresh: bool) -> Result<ChatGptAuth, CodexSubscriptionError> {
            self.calls.lock().unwrap().push(refresh);
            self.responses
                .pop_front()
                .ok_or(CodexSubscriptionError::ChatGptSignInRequired)
        }
    }

    struct CallbackAuth<F>(F);

    impl<F> ChatGptAuthProvider for CallbackAuth<F>
    where
        F: FnMut(bool) -> Result<ChatGptAuth, CodexSubscriptionError> + Send,
    {
        fn load(&mut self, refresh: bool) -> Result<ChatGptAuth, CodexSubscriptionError> {
            (self.0)(refresh)
        }
    }

    #[test]
    fn authentication_overlaps_recording_and_is_consumed_only_once() {
        let (entered, started) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let mut calls = 0;
        let provider = CallbackAuth(move |refresh: bool| {
            assert!(!refresh);
            calls += 1;
            if calls == 1 {
                entered.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(3)).unwrap();
                Ok(auth("prepared"))
            } else {
                Ok(auth("fresh"))
            }
        });
        let mut transport =
            CodexSubscriptionTransport::with_endpoint_and_auth("http://unused", provider);
        let path = PathBuf::from("recording.wav");
        transport.prepare_auth(JobId::new(), path.clone());
        started.recv_timeout(Duration::from_secs(3)).unwrap();
        // The caller has control while the auth provider is still blocked.
        release.send(()).unwrap();
        assert_eq!(
            transport
                .recording_auth(&path)
                .unwrap()
                .access_token
                .expose(),
            "prepared"
        );
        assert_eq!(
            transport
                .recording_auth(&path)
                .unwrap()
                .access_token
                .expose(),
            "fresh"
        );
    }

    #[test]
    fn rapid_discards_bound_workers_and_never_reuse_cancelled_credentials() {
        let (entered, started) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let mut calls = 0;
        let provider = CallbackAuth(move |_| {
            calls += 1;
            if calls == 1 {
                entered.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(3)).unwrap();
                Ok(auth("cancelled"))
            } else {
                Ok(auth("fresh"))
            }
        });
        let mut transport =
            CodexSubscriptionTransport::with_endpoint_and_auth("http://unused", provider);
        let path = PathBuf::from("recording.wav");
        let first = JobId::new();
        transport.prepare_auth(first, path.clone());
        started.recv_timeout(Duration::from_secs(3)).unwrap();
        transport.cancel_recording(JobId::new());
        assert_eq!(
            transport.prepared_auth.as_ref().unwrap().job_id,
            Some(first)
        );
        transport.cancel_recording(first);
        for _ in 0..100 {
            transport.prepare_auth(JobId::new(), path.clone());
        }
        assert!(transport.prepared_auth.as_ref().unwrap().job_id.is_none());
        release.send(()).unwrap();
        assert_eq!(
            transport
                .recording_auth(&path)
                .unwrap()
                .access_token
                .expose(),
            "fresh"
        );
    }

    #[test]
    fn failed_preparation_and_unrelated_recovery_check_auth_again() {
        for path_matches in [true, false] {
            let (entered, started) = mpsc::channel();
            let mut calls = 0;
            let provider = CallbackAuth(move |_| {
                calls += 1;
                if calls == 1 {
                    entered.send(()).unwrap();
                }
                if calls == 1 && path_matches {
                    Err(CodexSubscriptionError::AppServerUnavailable)
                } else {
                    Ok(auth(if calls == 1 { "unrelated" } else { "fresh" }))
                }
            });
            let mut transport =
                CodexSubscriptionTransport::with_endpoint_and_auth("http://unused", provider);
            transport.prepare_auth(JobId::new(), "recording.wav".into());
            started.recv_timeout(Duration::from_secs(3)).unwrap();
            let path = if path_matches {
                "recording.wav"
            } else {
                "recovery.wav"
            };
            assert_eq!(
                transport
                    .recording_auth(std::path::Path::new(path))
                    .unwrap()
                    .access_token
                    .expose(),
                "fresh"
            );
        }
    }

    #[test]
    fn a_long_recording_refreshes_prepared_credentials_near_expiry() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"exp":1}"#);
        let provider = FixedAuthProvider {
            calls: calls.clone(),
            responses: VecDeque::from([
                auth(&format!("header.{payload}.signature")),
                auth("refreshed"),
            ]),
        };
        let mut transport =
            CodexSubscriptionTransport::with_endpoint_and_auth("http://unused", provider);
        transport.prepare_auth(JobId::new(), "recording.wav".into());
        assert_eq!(
            transport
                .recording_auth(std::path::Path::new("recording.wav"))
                .unwrap()
                .access_token
                .expose(),
            "refreshed"
        );
        assert_eq!(*calls.lock().unwrap(), vec![false, true]);
    }

    fn auth(token: &str) -> ChatGptAuth {
        ChatGptAuth {
            access_token: ChatGptAccessToken(token.to_owned()),
            account_id: ChatGptAccountId("account-test".to_owned()),
        }
    }

    fn credentials(provider: impl ChatGptAuthProvider + 'static) -> ChatGptCredentials {
        ChatGptCredentials::with_provider("unused-codex".into(), Box::new(provider))
    }

    #[test]
    fn lanes_waiting_on_a_failing_sign_in_check_share_one_failure() {
        use std::sync::{
            Barrier,
            atomic::{AtomicUsize, Ordering},
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let credentials = Arc::new(credentials(CallbackAuth(move |_| {
            counted.fetch_add(1, Ordering::Relaxed);
            thread::sleep(Duration::from_millis(300));
            Err(CodexSubscriptionError::AppServerTimeout)
        })));
        let barrier = Arc::new(Barrier::new(5));
        let started = Instant::now();
        let lanes: Vec<_> = (0..5)
            .map(|_| {
                let credentials = Arc::clone(&credentials);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    credentials.bearer(false).map(|bearer| bearer.token)
                })
            })
            .collect();
        for lane in lanes {
            assert!(lane.join().unwrap().is_err());
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        // Five lanes in turn would have taken 1.5 s.
        assert!(started.elapsed() < Duration::from_millis(900));
        // The failure is remembered for a while rather than retried at once.
        assert!(credentials.bearer(false).is_err());
        assert!(credentials.bearer_after_rejection("any").is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn lanes_turned_down_with_the_same_token_refresh_it_once() {
        use std::sync::{
            Barrier,
            atomic::{AtomicUsize, Ordering},
        };
        let refreshes = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&refreshes);
        let credentials = Arc::new(credentials(CallbackAuth(move |refresh: bool| {
            if !refresh {
                return Ok(auth("stale"));
            }
            counted.fetch_add(1, Ordering::Relaxed);
            thread::sleep(Duration::from_millis(200));
            Ok(auth("fresh"))
        })));
        assert_eq!(credentials.bearer(false).unwrap().token, "stale");
        let barrier = Arc::new(Barrier::new(2));
        let lanes: Vec<_> = (0..2)
            .map(|_| {
                let credentials = Arc::clone(&credentials);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    credentials.bearer_after_rejection("stale").unwrap().token
                })
            })
            .collect();
        for lane in lanes {
            assert_eq!(lane.join().unwrap(), "fresh");
        }
        // A lane whose request was turned down before the refresh landed
        // takes the new token as it is.
        assert_eq!(
            credentials.bearer_after_rejection("stale").unwrap().token,
            "fresh"
        );
        assert_eq!(refreshes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn concurrent_copilot_lanes_share_one_cold_authentication_check() {
        use std::sync::{
            Barrier,
            atomic::{AtomicUsize, Ordering},
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let credentials = Arc::new(credentials(CallbackAuth(move |refresh: bool| {
            assert!(!refresh);
            counted.fetch_add(1, Ordering::Relaxed);
            // Model a slow external sign-in check while all lanes arrive.
            thread::sleep(Duration::from_millis(25));
            Ok(auth("shared-token"))
        })));
        let barrier = Arc::new(Barrier::new(8));
        let lanes: Vec<_> = (0..8)
            .map(|_| {
                let credentials = Arc::clone(&credentials);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    credentials.bearer(false).unwrap().token
                })
            })
            .collect();
        for lane in lanes {
            assert_eq!(lane.join().unwrap(), "shared-token");
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn cached_copilot_credentials_still_refresh_when_expiring_or_rejected() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"exp":1}"#);
        let credentials = credentials(FixedAuthProvider {
            calls: Arc::clone(&calls),
            responses: VecDeque::from([
                auth(&format!("header.{payload}.signature")),
                auth("fresh"),
                auth("replaced-after-rejection"),
            ]),
        });
        assert_eq!(credentials.bearer(false).unwrap().token, "fresh");
        assert_eq!(credentials.bearer(false).unwrap().token, "fresh");
        assert_eq!(
            credentials.bearer(true).unwrap().token,
            "replaced-after-rejection"
        );
        assert_eq!(
            credentials.bearer(false).unwrap().token,
            "replaced-after-rejection"
        );
        assert_eq!(*calls.lock().unwrap(), [false, true, true]);
    }

    fn read_http_request(stream: &mut std::net::TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let read = stream.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
            let headers_end = bytes
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|position| position + 4);
            if let Some(headers_end) = headers_end {
                let headers = String::from_utf8_lossy(&bytes[..headers_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|value| value.parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if bytes.len() >= headers_end + content_length {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[test]
    fn wav_request_matches_the_captured_chatgpt_contract() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            let body = r#"{"text":"Front, center."}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            request
        });
        let calls = Arc::new(Mutex::new(Vec::new()));
        let provider = FixedAuthProvider {
            calls: calls.clone(),
            responses: VecDeque::from([auth("plan-token")]),
        };
        let directory = tempdir().unwrap();
        let audio_path = directory.path().join("recording.wav");
        std::fs::write(&audio_path, b"RIFFrecorded speech").unwrap();
        let mut transport = CodexSubscriptionTransport::with_endpoint_and_auth(
            format!("http://{address}/backend-api/transcribe"),
            provider,
        );

        transport.prepare_auth(JobId::new(), audio_path.clone());

        let text = transport
            .transcribe_audio(TranscriptionRequest {
                keywords: &[],
                audio_path: &audio_path,
                provider: TranscriptionProvider::ChatGptSubscription,
                model: "gpt-transcribe",
                language: "en",
                prompt: "ignored by subscription transcription",
                duration_seconds: 1.4,
            })
            .unwrap();

        let request = server.join().unwrap();
        assert_eq!(text, "Front, center.");
        assert!(request.starts_with("POST /backend-api/transcribe HTTP/1.1"));
        assert!(request.contains("authorization: Bearer plan-token"));
        assert!(request.contains("chatgpt-account-id: account-test"));
        assert!(request.contains("originator: Codex Desktop"));
        assert!(request.contains("name=\"file\"; filename=\"codex.wav\""));
        assert!(request.contains("Content-Type: audio/wav"));
        assert!(request.contains("RIFFrecorded speech"));
        assert!(request.contains("name=\"language\"\r\n\r\nen"));
        assert_eq!(*calls.lock().unwrap(), vec![false]);
    }

    #[test]
    fn unauthorized_request_refreshes_once_without_api_fallback() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, body) in [
                ("401 Unauthorized", r#"{"detail":"expired"}"#),
                ("200 OK", r#"{"text":"Recovered."}"#),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                requests.push(read_http_request(&mut stream));
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
            requests
        });
        let calls = Arc::new(Mutex::new(Vec::new()));
        let provider = FixedAuthProvider {
            calls: calls.clone(),
            responses: VecDeque::from([auth("expired-token"), auth("fresh-token")]),
        };
        let directory = tempdir().unwrap();
        let audio_path = directory.path().join("recording.wav");
        std::fs::write(&audio_path, b"RIFFspeech").unwrap();
        let mut transport = CodexSubscriptionTransport::with_endpoint_and_auth(
            format!("http://{address}/backend-api/transcribe"),
            provider,
        );

        transport.prepare_auth(JobId::new(), audio_path.clone());

        let text = transport
            .transcribe_audio(TranscriptionRequest {
                keywords: &[],
                audio_path: &audio_path,
                provider: TranscriptionProvider::ChatGptSubscription,
                model: "gpt-transcribe",
                language: "",
                prompt: "",
                duration_seconds: 1.0,
            })
            .unwrap();

        let requests = server.join().unwrap();
        assert_eq!(text, "Recovered.");
        assert!(requests[0].contains("authorization: Bearer expired-token"));
        assert!(requests[1].contains("authorization: Bearer fresh-token"));
        assert_eq!(*calls.lock().unwrap(), vec![false, true]);
    }

    /// Serves one scripted reply per request (`None` closes the connection
    /// without one), then counts any further requests for a moment.
    fn scripted_endpoint(
        replies: Vec<Option<(&'static str, &'static str)>>,
    ) -> (String, thread::JoinHandle<(usize, usize)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!(
            "http://{}/backend-api/transcribe",
            listener.local_addr().unwrap()
        );
        let server = thread::spawn(move || {
            let scripted = replies.len();
            for reply in replies {
                let (mut stream, _) = listener.accept().unwrap();
                read_http_request(&mut stream);
                if let Some((status, body)) = reply {
                    write!(
                        stream,
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .unwrap();
                }
            }
            listener.set_nonblocking(true).unwrap();
            let mut extra = 0;
            let until = Instant::now() + Duration::from_millis(300);
            while Instant::now() < until {
                if listener.accept().is_ok() {
                    extra += 1;
                }
                thread::sleep(Duration::from_millis(10));
            }
            (scripted, extra)
        });
        (endpoint, server)
    }

    fn transcribe_through(endpoint: String) -> Result<String, ExternalError> {
        let provider = CallbackAuth(|_| Ok(auth("plan-token")));
        let directory = tempdir().unwrap();
        let audio_path = directory.path().join("recording.wav");
        std::fs::write(&audio_path, b"RIFFspeech").unwrap();
        let mut transport = CodexSubscriptionTransport::with_endpoint_and_auth(endpoint, provider);
        transport.transcribe_audio(TranscriptionRequest {
            keywords: &[],
            audio_path: &audio_path,
            provider: TranscriptionProvider::ChatGptSubscription,
            model: "gpt-transcribe",
            language: "",
            prompt: "",
            duration_seconds: 1.0,
        })
    }

    #[test]
    fn dictation_is_sent_again_once_after_a_dropped_connection_or_a_busy_service() {
        for first in [
            None,
            Some(("503 Service Unavailable", r#"{"detail":"busy"}"#)),
        ] {
            let (endpoint, server) =
                scripted_endpoint(vec![first, Some(("200 OK", r#"{"text":"Made it."}"#))]);
            assert_eq!(transcribe_through(endpoint).unwrap(), "Made it.");
            assert_eq!(server.join().unwrap(), (2, 0));
        }
    }

    #[test]
    fn dictation_is_not_sent_again_after_a_rejection_or_a_used_up_limit() {
        for reply in [
            ("400 Bad Request", r#"{"detail":"bad audio"}"#),
            (
                "429 Too Many Requests",
                r#"{"error":{"message":"The usage limit has been reached","resets_in_seconds":86400}}"#,
            ),
        ] {
            let (endpoint, server) = scripted_endpoint(vec![Some(reply)]);
            assert!(transcribe_through(endpoint).is_err());
            assert_eq!(server.join().unwrap(), (1, 0), "{}", reply.0);
        }
        // A second busy reply is the last.
        let busy = Some(("502 Bad Gateway", "{}"));
        let (endpoint, server) = scripted_endpoint(vec![busy, busy]);
        assert!(transcribe_through(endpoint).is_err());
        assert_eq!(server.join().unwrap(), (2, 0));
    }

    #[test]
    fn a_rate_limit_says_when_it_resets() {
        assert_eq!(
            rate_limit_message(""),
            "ChatGPT transcription limit reached. Try again later."
        );
        assert_eq!(
            rate_limit_message(
                r#"{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached","resets_in_seconds":2505600}}"#
            ),
            "ChatGPT transcription limit reached. The usage limit has been reached. It resets in about 29 days."
        );
        assert_eq!(
            rate_limit_message(r#"{"error":{"resets_in_seconds":90}}"#),
            "ChatGPT transcription limit reached. It resets in about 2 min."
        );
    }

    #[test]
    fn app_server_auth_requires_chatgpt_and_reads_the_nested_account_claim() {
        let claims = json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": "account-123"}
        });
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).unwrap());
        let token = format!("header.{payload}.signature");

        let auth = parse_chatgpt_auth(&json!({
            "authMethod": "chatgpt",
            "authToken": token
        }))
        .unwrap();

        assert_eq!(auth.account_id.expose(), "account-123");
        assert!(matches!(
            parse_chatgpt_auth(&json!({"authMethod": "apikey", "authToken": "secret"})),
            Err(CodexSubscriptionError::ChatGptSignInRequired)
        ));
        assert!(matches!(
            parse_chatgpt_auth(&json!({"authMethod": "chatgpt", "authToken": null})),
            Err(CodexSubscriptionError::ChatGptTokenUnavailable)
        ));
    }
}

#[cfg(not(windows))]
fn windows_codex_binary() -> Option<PathBuf> {
    None
}
#[cfg(windows)]
fn windows_codex_binary() -> Option<PathBuf> {
    // Prefer an executable distributed with the installed desktop app; never read credentials.
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let exe = dir.join("codex.exe");
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        for suffix in [
            "Programs/OpenAI/Codex/bin/codex.exe",
            "Programs/Codex/resources/codex.exe",
            "Programs/codex/resources/codex.exe",
        ] {
            let exe = PathBuf::from(&local).join(suffix);
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    // AppX inventory returns installation paths only, without touching the Codex login cache.
    let mut command = Command::new("powershell.exe");
    use std::os::windows::process::CommandExt;
    let output = command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-AppxPackage *Codex* | Select-Object -ExpandProperty InstallLocation",
        ])
        .creation_flags(0x08000000)
        .output()
        .ok()?;
    for location in String::from_utf8_lossy(&output.stdout).lines() {
        for suffix in [
            "app/resources/codex.exe",
            "resources/codex.exe",
            "app/resources/bin/codex.exe",
        ] {
            let exe = PathBuf::from(location.trim()).join(suffix);
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

#[cfg(windows)]
fn windows_auth_home() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("AGENTDICTATE_CODEX_HOME") {
        return Some(PathBuf::from(home));
    }
    let home = crate::AppPaths::from_environment()
        .ok()?
        .config_file
        .parent()?
        .join("codex-login");
    home.is_dir().then_some(home)
}
#[cfg(windows)]
pub fn sign_in_with_chatgpt() -> anyhow::Result<()> {
    use std::os::windows::process::CommandExt;
    let paths = crate::AppPaths::from_environment()?;
    let home = std::env::var_os("AGENTDICTATE_CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or(
            paths
                .config_file
                .parent()
                .ok_or_else(|| anyhow::anyhow!("configuration directory is missing"))?
                .join("codex-login"),
        );
    std::fs::create_dir_all(&home)?;
    agentdictate_runtime::restrict_path(&home)?;
    let provider = CodexAppServerAuthProvider::discover();
    let status = Command::new(provider.codex_binary)
        .arg("login")
        .env("CODEX_HOME", home)
        .env_remove("OPENAI_API_KEY")
        .env_remove("CODEX_API_KEY")
        .creation_flags(0x08000000)
        .status()?;
    anyhow::ensure!(status.success(), "ChatGPT sign-in did not complete");
    Ok(())
}
