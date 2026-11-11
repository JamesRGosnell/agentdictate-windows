//! Streaming transcription of one side of a call over the OpenAI realtime
//! API, so words appear while people speak.
//!
//! The engine owns when speech starts and ends: it appends speech the
//! segmenter released (never silence or noise) and commits at each pause it
//! wants transcribed. The worker connects on the first speech, idles the
//! connection after a long quiet, and reports partial text, each commit's
//! final text, or a failure after which the engine transcribes by upload.

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc,
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    time::{Duration, Instant},
};

use agentdictate_core::Speaker;
use serde_json::{Value, json};
use tungstenite::Message;

use crate::live_transcription::{
    RealtimeSocket, Resampler, append_audio, connect_realtime, read_event,
};

/// The realtime transcription endpoint.
pub(crate) const REALTIME_URL: &str = "wss://api.openai.com/v1/realtime?intent=transcription";
/// How long a session may take to be ready.
const SETUP_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a commit may take to come back transcribed.
const COMMIT_TIMEOUT: Duration = Duration::from_secs(10);
/// Quiet after which the connection is closed until speech resumes.
const IDLE_CLOSE: Duration = Duration::from_secs(45);
/// How often the worker checks for server events while connected.
const POLL: Duration = Duration::from_millis(20);

pub(crate) enum LiveCommand {
    /// 16 kHz mono speech to add to the input buffer.
    Append(Vec<i16>),
    /// Transcribe everything appended since the last commit; reported back
    /// with this tag.
    Commit { tag: u64 },
    /// Drop audio appended since the last commit.
    Clear,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LiveEvent {
    /// Text so far for speech not yet transcribed in full.
    Partial(String),
    /// The service judged the speaker's turn complete: the text since the
    /// last commit, final. Only services that detect turns report it.
    TurnEnded(String),
    Committed {
        tag: u64,
        text: String,
    },
    /// The stream stopped working; nothing more will be reported.
    Failed(String),
}

pub(crate) type LiveEvents = Box<dyn Fn(LiveEvent) + Send>;
/// Opens a stream for one side of the call.
pub(crate) type LiveFactory = Arc<dyn Fn(Speaker, LiveEvents) -> LiveStream + Send + Sync>;

/// The engine's handle on a streaming worker.
pub(crate) struct LiveStream {
    commands: Sender<LiveCommand>,
}

impl LiveStream {
    pub(crate) fn new(commands: Sender<LiveCommand>) -> Self {
        Self { commands }
    }

    /// False when the worker has stopped.
    pub(crate) fn send(&self, command: LiveCommand) -> bool {
        self.commands.send(command).is_ok()
    }
}

/// Starts a worker that streams to `url` with `key`, transcribing with
/// `transcription` (the session's transcription settings).
pub(crate) fn spawn(
    url: String,
    key: String,
    transcription: Value,
    speaker: Speaker,
    events: LiveEvents,
) -> LiveStream {
    let (commands, queue) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name(format!("copilot-live-{speaker:?}").to_lowercase())
        .spawn(move || {
            let mut worker = Worker {
                url,
                key,
                transcription,
                events,
                connection: None,
            };
            if let Err(error) = worker.run(&queue) {
                (worker.events)(LiveEvent::Failed(error.to_string()));
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "could not start streaming transcription");
    }
    LiveStream::new(commands)
}

/// The transcription settings for a call in `language` (empty: automatic).
pub(crate) fn transcription_settings(language: &str) -> Value {
    let mut settings = json!({"model": "gpt-live-transcribe", "delay": "minimal"});
    let language = language.trim();
    if !language.is_empty() {
        settings["languages"] = json!([language]);
    }
    settings
}

enum Outgoing {
    Audio(Vec<u8>),
    Commit(u64),
    Clear,
}

struct Connection {
    socket: RealtimeSocket,
    opened: Instant,
    ready: bool,
    resampler: Resampler,
    /// Sent before the session was ready.
    outbox: VecDeque<Outgoing>,
    /// Commits sent and not yet acknowledged, oldest first.
    sent_commits: VecDeque<(u64, Instant)>,
    /// Acknowledged commits by item, with their deadline.
    items: HashMap<String, (u64, Instant)>,
    /// Finished text that arrived before its commit was acknowledged.
    finished: HashMap<String, String>,
    deltas: HashMap<String, String>,
    last_audio: Instant,
}

struct Worker {
    url: String,
    key: String,
    transcription: Value,
    events: LiveEvents,
    connection: Option<Connection>,
}

impl Worker {
    fn run(&mut self, queue: &Receiver<LiveCommand>) -> anyhow::Result<()> {
        loop {
            let command = if self.connection.is_some() {
                match queue.recv_timeout(POLL) {
                    Ok(command) => Some(command),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            } else {
                // Closed while quiet: wait for speech without polling.
                match queue.recv() {
                    Ok(command) => Some(command),
                    Err(_) => break,
                }
            };
            if let Some(command) = command {
                self.handle(command)?;
                // Take whatever else is queued before reading events.
                while let Ok(command) = queue.try_recv() {
                    self.handle(command)?;
                }
            }
            self.pump()?;
        }
        if let Some(mut connection) = self.connection.take() {
            let _ = connection.socket.close(None);
        }
        Ok(())
    }

    fn connection(&mut self) -> anyhow::Result<&mut Connection> {
        if self.connection.is_none() {
            let socket = connect_realtime(&self.url, &self.key, self.transcription.clone())?;
            tracing::info!("streaming transcription connected");
            self.connection = Some(Connection {
                socket,
                opened: Instant::now(),
                ready: false,
                resampler: Resampler::default(),
                outbox: VecDeque::new(),
                sent_commits: VecDeque::new(),
                items: HashMap::new(),
                finished: HashMap::new(),
                deltas: HashMap::new(),
                last_audio: Instant::now(),
            });
        }
        Ok(self.connection.as_mut().expect("connected"))
    }

    fn handle(&mut self, command: LiveCommand) -> anyhow::Result<()> {
        let outgoing = match command {
            LiveCommand::Append(samples) => {
                let connection = self.connection()?;
                connection.last_audio = Instant::now();
                let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
                Outgoing::Audio(connection.resampler.push(&bytes))
            }
            LiveCommand::Commit { tag } => Outgoing::Commit(tag),
            LiveCommand::Clear => Outgoing::Clear,
        };
        let Some(connection) = self.connection.as_mut() else {
            // Nothing was appended on this connection; nothing to commit.
            return match outgoing {
                Outgoing::Commit(_) => anyhow::bail!("commit without audio"),
                _ => Ok(()),
            };
        };
        connection.outbox.push_back(outgoing);
        if connection.ready {
            connection.flush()?;
        }
        Ok(())
    }

    /// Reads server events, enforces deadlines, and idles a quiet connection.
    fn pump(&mut self) -> anyhow::Result<()> {
        let Some(connection) = self.connection.as_mut() else {
            return Ok(());
        };
        for _ in 0..32 {
            let Some(event) = read_event(&mut connection.socket)? else {
                break;
            };
            for event in connection.receive(&event)? {
                (self.events)(event);
            }
        }
        let now = Instant::now();
        anyhow::ensure!(
            connection.ready || now.duration_since(connection.opened) < SETUP_TIMEOUT,
            "streaming transcription did not start"
        );
        anyhow::ensure!(
            connection
                .sent_commits
                .iter()
                .map(|(_, sent)| sent)
                .chain(connection.items.values().map(|(_, sent)| sent))
                .all(|sent| now.duration_since(*sent) < COMMIT_TIMEOUT),
            "streaming transcription timed out"
        );
        let idle = connection.ready
            && connection.outbox.is_empty()
            && connection.sent_commits.is_empty()
            && connection.items.is_empty()
            && now.duration_since(connection.last_audio) >= IDLE_CLOSE;
        if idle {
            let _ = connection.socket.close(None);
            self.connection = None;
            tracing::info!("streaming transcription idle; closed until speech resumes");
        }
        Ok(())
    }
}

impl Connection {
    fn flush(&mut self) -> anyhow::Result<()> {
        while let Some(outgoing) = self.outbox.pop_front() {
            match outgoing {
                Outgoing::Audio(pcm) if pcm.is_empty() => {}
                Outgoing::Audio(pcm) => append_audio(&mut self.socket, &pcm)?,
                Outgoing::Commit(tag) => {
                    self.socket.send(Message::text(
                        json!({"type":"input_audio_buffer.commit"}).to_string(),
                    ))?;
                    self.sent_commits.push_back((tag, Instant::now()));
                }
                Outgoing::Clear => {
                    self.socket.send(Message::text(
                        json!({"type":"input_audio_buffer.clear"}).to_string(),
                    ))?;
                }
            }
        }
        Ok(())
    }

    /// Applies one server event, returning what to report.
    fn receive(&mut self, event: &Value) -> anyhow::Result<Vec<LiveEvent>> {
        let item = || {
            event
                .get("item_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let mut reports = Vec::new();
        match event.get("type").and_then(Value::as_str) {
            Some("session.updated" | "transcription_session.updated") => {
                self.ready = true;
                self.flush()?;
            }
            Some("error" | "conversation.item.input_audio_transcription.failed") => {
                let message = event
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("the service rejected streaming transcription");
                anyhow::bail!("{message}");
            }
            Some("input_audio_buffer.committed") => {
                let (tag, sent) = self
                    .sent_commits
                    .pop_front()
                    .ok_or_else(|| anyhow::anyhow!("unexpected commit"))?;
                let item = item().ok_or_else(|| anyhow::anyhow!("commit without item"))?;
                match self.finished.remove(&item) {
                    Some(text) => reports.push(LiveEvent::Committed { tag, text }),
                    None => {
                        self.items.insert(item, (tag, sent));
                    }
                }
            }
            Some("conversation.item.input_audio_transcription.delta") => {
                if let (Some(item), Some(delta)) =
                    (item(), event.get("delta").and_then(Value::as_str))
                {
                    let text = self.deltas.entry(item).or_default();
                    text.push_str(delta);
                    reports.push(LiveEvent::Partial(text.trim().to_owned()));
                }
            }
            Some("conversation.item.input_audio_transcription.completed") => {
                let item = item().ok_or_else(|| anyhow::anyhow!("transcript without item"))?;
                let text = event
                    .get("transcript")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                self.deltas.remove(&item);
                match self.items.remove(&item) {
                    Some((tag, _)) => reports.push(LiveEvent::Committed { tag, text }),
                    None => {
                        anyhow::ensure!(self.finished.len() < 16, "unexpected transcripts");
                        self.finished.insert(item, text);
                    }
                }
            }
            _ => {}
        }
        Ok(reports)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use std::sync::Mutex;

    fn events() -> (LiveEvents, Arc<Mutex<Vec<LiveEvent>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        (
            Box::new(move |event| sink.lock().unwrap().push(event)),
            seen,
        )
    }

    fn wait_for(seen: &Mutex<Vec<LiveEvent>>, done: impl Fn(&[LiveEvent]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done(&seen.lock().unwrap()) {
            assert!(Instant::now() < deadline, "{:?}", seen.lock().unwrap());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn read(ws: &mut tungstenite::WebSocket<std::net::TcpStream>) -> Value {
        serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap()
    }

    #[test]
    fn appended_speech_streams_and_each_commit_reports_its_text() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut ws = tungstenite::accept(tcp).unwrap();
            let session = read(&mut ws);
            assert_eq!(
                session["session"]["audio"]["input"]["transcription"]["model"],
                "gpt-live-transcribe"
            );
            assert!(session["session"]["audio"]["input"]["turn_detection"].is_null());
            ws.send(Message::text(json!({"type":"session.updated"}).to_string()))
                .unwrap();
            let mut bytes = 0;
            for item in ["a", "b"] {
                loop {
                    let event = read(&mut ws);
                    if event["type"] == "input_audio_buffer.commit" {
                        break;
                    }
                    assert_eq!(event["type"], "input_audio_buffer.append");
                    bytes += STANDARD
                        .decode(event["audio"].as_str().unwrap())
                        .unwrap()
                        .len();
                }
                let text = if item == "a" { "What is" } else { "the price?" };
                for event in [
                    json!({"type":"conversation.item.input_audio_transcription.delta","item_id":item,"delta":"What"}),
                    json!({"type":"input_audio_buffer.committed","item_id":item}),
                    json!({"type":"conversation.item.input_audio_transcription.completed","item_id":item,"transcript":text}),
                ] {
                    ws.send(Message::text(event.to_string())).unwrap();
                }
            }
            // 2 × 1,600 samples at 16 kHz arrive as 24 kHz PCM.
            assert!((9_590..=9_600).contains(&bytes), "{bytes}");
        });
        let (sink, seen) = events();
        let stream = spawn(
            format!("ws://{address}"),
            "test".into(),
            transcription_settings("en"),
            Speaker::Them,
            sink,
        );
        assert!(stream.send(LiveCommand::Append(vec![100; 1_600])));
        assert!(stream.send(LiveCommand::Commit { tag: 7 }));
        assert!(stream.send(LiveCommand::Append(vec![100; 1_600])));
        assert!(stream.send(LiveCommand::Commit { tag: 8 }));
        wait_for(&seen, |events| {
            events
                .iter()
                .filter(|event| matches!(event, LiveEvent::Committed { .. }))
                .count()
                == 2
        });
        server.join().unwrap();
        let seen = seen.lock().unwrap();
        assert!(seen.contains(&LiveEvent::Partial("What".into())));
        assert!(seen.contains(&LiveEvent::Committed {
            tag: 7,
            text: "What is".into()
        }));
        assert!(seen.contains(&LiveEvent::Committed {
            tag: 8,
            text: "the price?".into()
        }));
    }

    #[test]
    fn a_refused_connection_reports_a_failure() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let (sink, seen) = events();
        let stream = spawn(
            format!("ws://{address}"),
            "test".into(),
            transcription_settings(""),
            Speaker::Me,
            sink,
        );
        stream.send(LiveCommand::Append(vec![0; 1_600]));
        wait_for(&seen, |events| {
            events
                .iter()
                .any(|event| matches!(event, LiveEvent::Failed(_)))
        });
    }

    #[test]
    fn a_service_error_mid_stream_reports_a_failure() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(tcp).unwrap();
            read(&mut ws);
            ws.send(Message::text(json!({"type":"session.updated"}).to_string()))
                .unwrap();
            read(&mut ws);
            ws.send(Message::text(
                json!({"type":"error","error":{"message":"quota exceeded"}}).to_string(),
            ))
            .unwrap();
            std::thread::sleep(Duration::from_millis(300));
        });
        let (sink, seen) = events();
        let stream = spawn(
            format!("ws://{address}"),
            "test".into(),
            transcription_settings(""),
            Speaker::Them,
            sink,
        );
        stream.send(LiveCommand::Append(vec![0; 1_600]));
        wait_for(&seen, |events| {
            events.contains(&LiveEvent::Failed("quota exceeded".into()))
        });
        server.join().unwrap();
    }

    #[test]
    fn transcription_settings_name_a_language_only_when_one_is_set() {
        assert!(transcription_settings("").get("languages").is_none());
        assert_eq!(transcription_settings("fr")["languages"], json!(["fr"]));
    }
}
