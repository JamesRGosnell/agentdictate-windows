//! Streaming transcription of one side of a call with AssemblyAI, whose
//! model also judges when a speaker has finished a turn.
//!
//! The engine drives it as it drives the OpenAI stream: it appends the
//! speech the segmenter released and commits at each pause it wants
//! transcribed. The service ends a turn by itself a moment after the words
//! read as complete, well before the engine's own pause, and that is
//! reported at once as [`LiveEvent::TurnEnded`] so an answer can start. A
//! commit then returns the turns since the last one; if a turn is still
//! open, the service is told to end it.

use std::{
    collections::VecDeque,
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    time::{Duration, Instant},
};

use agentdictate_core::Speaker;
use serde_json::{Value, json};
use tungstenite::Message;

use super::live::{LiveCommand, LiveEvent, LiveEvents, LiveStream};
use crate::live_transcription::{RealtimeSocket, open_socket, read_event};

/// The streaming endpoint; it routes to the nearest region.
pub(crate) const STREAMING_URL: &str = "wss://streaming.assemblyai.com/v3/ws";
/// How long a session may take to begin.
const SETUP_TIMEOUT: Duration = Duration::from_secs(5);
/// How long an open turn may take to end once told to.
const COMMIT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a commit waits for words the service has not reported yet.
const UNHEARD_WAIT: Duration = Duration::from_millis(600);
/// Audio sent since the last turn ended that may hold words not yet
/// reported: less than this is the silence that ended the turn.
const UNHEARD_AUDIO: Duration = Duration::from_millis(700);
/// Quiet after which the session is closed until speech resumes; an open
/// session is billed by the minute.
const IDLE_CLOSE: Duration = Duration::from_secs(45);
/// How often the worker checks for server events while connected.
const POLL: Duration = Duration::from_millis(20);
/// 16 kHz, 16-bit mono.
const BYTES_PER_MS: usize = 32;
/// Audio goes out in frames of this length; the service asks for 50 ms.
const FRAME_BYTES: usize = 50 * BYTES_PER_MS;
/// The shortest frame the service accepts.
const MIN_FRAME_BYTES: usize = 20 * BYTES_PER_MS;

/// The session address for a call in `language` (empty: any language).
pub(crate) fn session_url(base: &str, language: &str) -> String {
    // The quickest preset: a turn is checked for its end after 128 ms of
    // silence and the first words are reported without delay.
    let mut url = format!(
        "{base}?sample_rate=16000&encoding=pcm_s16le&speech_model=universal-3-6-pro&mode=min_latency"
    );
    let language = language.trim();
    if !language.is_empty()
        && language
            .chars()
            .all(|letter| letter.is_ascii_alphabetic() || letter == '-')
    {
        // A JSON list, percent-encoded: ["fr"].
        url.push_str(&format!("&language_codes=%5B%22{language}%22%5D"));
    }
    url
}

/// Whether the service accepts `key`: opens a session and ends it as soon
/// as it begins.
pub(crate) fn check_key(url: &str, key: &str) -> Result<(), String> {
    let mut socket = open_socket(url, key).map_err(|error| error.to_string())?;
    let deadline = Instant::now() + SETUP_TIMEOUT;
    let result = loop {
        match read_event(&mut socket) {
            Ok(Some(event)) => match event.get("type").and_then(Value::as_str) {
                Some("Begin") => break Ok(()),
                Some("Error") => {
                    break Err(event
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("the service refused the session")
                        .to_owned());
                }
                _ => {}
            },
            Ok(None) if Instant::now() >= deadline => {
                break Err("the session did not begin".to_owned());
            }
            Ok(None) => {}
            Err(error) => break Err(error.to_string()),
        }
    };
    // An open session is billed until it is ended.
    let _ = socket.send(Message::text(json!({"type": "Terminate"}).to_string()));
    let _ = socket.close(None);
    result
}

/// Starts a worker that streams to `url` with `key`.
pub(crate) fn spawn(url: String, key: String, speaker: Speaker, events: LiveEvents) -> LiveStream {
    let (commands, queue) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name(format!("copilot-assembly-{speaker:?}").to_lowercase())
        .spawn(move || {
            let mut worker = Worker {
                url,
                key,
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

enum Outgoing {
    Audio(Vec<u8>),
    Commit(u64),
    Clear,
}

/// What the service has said since the last commit.
#[derive(Default)]
struct Heard {
    /// Turns the service ended, oldest first.
    turns: Vec<String>,
    /// The turn still open, as last reported.
    partial: String,
    /// A turn is open: words were reported and their turn has not ended.
    open: bool,
    /// Commits waiting for their text, oldest first, with when each was made.
    commits: VecDeque<(u64, Instant)>,
    /// Turns told to end whose text is to be dropped.
    discard: usize,
    /// Audio sent since the last turn ended.
    unheard_bytes: usize,
}

impl Heard {
    fn text(&self) -> String {
        self.turns
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(self.partial.as_str()))
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn take(&mut self) -> String {
        self.partial.clear();
        let text = self.text();
        self.turns.clear();
        text
    }

    /// Applies one server message, returning what to report.
    fn receive(&mut self, event: &Value) -> anyhow::Result<Vec<LiveEvent>> {
        let mut reports = Vec::new();
        match event.get("type").and_then(Value::as_str) {
            Some("Error") => {
                let message = event
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("the service rejected streaming transcription");
                anyhow::bail!("{message}");
            }
            Some("SpeechStarted") => self.open = true,
            Some("Turn") => {
                let transcript = event
                    .get("transcript")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim();
                let ended = event
                    .get("end_of_turn")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if !ended {
                    self.partial = transcript.to_owned();
                    self.open = true;
                    if self.discard == 0 {
                        reports.push(LiveEvent::Partial(self.text()));
                    }
                    return Ok(reports);
                }
                self.open = false;
                self.unheard_bytes = 0;
                self.partial.clear();
                if self.discard > 0 {
                    self.discard -= 1;
                    return Ok(reports);
                }
                if !transcript.is_empty() {
                    self.turns.push(transcript.to_owned());
                }
                match self.commits.pop_front() {
                    Some((tag, _)) => reports.push(LiveEvent::Committed {
                        tag,
                        text: self.take(),
                    }),
                    None if !transcript.is_empty() => {
                        reports.push(LiveEvent::TurnEnded(self.text()));
                    }
                    None => {}
                }
            }
            _ => {}
        }
        Ok(reports)
    }

    /// Commits that need wait no longer: nothing more is coming for them.
    fn settle(&mut self, now: Instant) -> anyhow::Result<Vec<LiveEvent>> {
        let mut reports = Vec::new();
        while let Some((tag, made)) = self.commits.front().copied() {
            let waited = now.duration_since(made);
            if self.open {
                anyhow::ensure!(waited < COMMIT_TIMEOUT, "streaming transcription timed out");
                break;
            }
            // Audio sent since the last turn may hold words not reported yet.
            let unheard = self.unheard_bytes > UNHEARD_AUDIO.as_millis() as usize * BYTES_PER_MS;
            if (unheard || self.turns.is_empty()) && waited < UNHEARD_WAIT {
                break;
            }
            self.commits.pop_front();
            self.unheard_bytes = 0;
            reports.push(LiveEvent::Committed {
                tag,
                text: self.take(),
            });
        }
        Ok(reports)
    }
}

struct Connection {
    socket: RealtimeSocket,
    opened: Instant,
    ready: bool,
    /// Sent before the session began.
    outbox: VecDeque<Outgoing>,
    /// Audio waiting to fill a frame.
    frame: Vec<u8>,
    heard: Heard,
    last_audio: Instant,
}

struct Worker {
    url: String,
    key: String,
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
                while let Ok(command) = queue.try_recv() {
                    self.handle(command)?;
                }
            }
            self.pump()?;
        }
        self.close();
        Ok(())
    }

    /// Ends the session; one left open is billed until the service closes it.
    fn close(&mut self) {
        if let Some(mut connection) = self.connection.take() {
            let _ = connection
                .socket
                .send(Message::text(json!({"type": "Terminate"}).to_string()));
            let _ = connection.socket.close(None);
        }
    }

    fn handle(&mut self, command: LiveCommand) -> anyhow::Result<()> {
        let outgoing = match command {
            LiveCommand::Append(samples) => {
                if self.connection.is_none() {
                    let socket = open_socket(&self.url, &self.key)?;
                    tracing::info!("AssemblyAI streaming transcription connected");
                    self.connection = Some(Connection {
                        socket,
                        opened: Instant::now(),
                        ready: false,
                        outbox: VecDeque::new(),
                        frame: Vec::new(),
                        heard: Heard::default(),
                        last_audio: Instant::now(),
                    });
                }
                Outgoing::Audio(samples.iter().flat_map(|s| s.to_le_bytes()).collect())
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
        if matches!(outgoing, Outgoing::Audio(_)) {
            connection.last_audio = Instant::now();
        }
        connection.outbox.push_back(outgoing);
        if connection.ready {
            connection.flush()?;
        }
        Ok(())
    }

    /// Reads server messages, settles commits, and idles a quiet session.
    fn pump(&mut self) -> anyhow::Result<()> {
        let Some(connection) = self.connection.as_mut() else {
            return Ok(());
        };
        for _ in 0..32 {
            let Some(event) = read_event(&mut connection.socket)? else {
                break;
            };
            if event.get("type").and_then(Value::as_str) == Some("Begin") {
                let configuration = event
                    .get("configuration")
                    .map(ToString::to_string)
                    .unwrap_or_default();
                tracing::info!(%configuration, "AssemblyAI session began");
                connection.ready = true;
                connection.flush()?;
                continue;
            }
            for report in connection.heard.receive(&event)? {
                (self.events)(report);
            }
        }
        let now = Instant::now();
        anyhow::ensure!(
            connection.ready || now.duration_since(connection.opened) < SETUP_TIMEOUT,
            "streaming transcription did not start"
        );
        for report in connection.heard.settle(now)? {
            (self.events)(report);
        }
        let idle = connection.ready
            && connection.outbox.is_empty()
            && connection.heard.commits.is_empty()
            && now.duration_since(connection.last_audio) >= IDLE_CLOSE;
        if idle {
            self.close();
            tracing::info!("streaming transcription idle; closed until speech resumes");
        }
        Ok(())
    }
}

impl Connection {
    fn flush(&mut self) -> anyhow::Result<()> {
        while let Some(outgoing) = self.outbox.pop_front() {
            match outgoing {
                Outgoing::Audio(pcm) => {
                    self.frame.extend_from_slice(&pcm);
                    while self.frame.len() >= FRAME_BYTES {
                        let rest = self.frame.split_off(FRAME_BYTES);
                        let frame = std::mem::replace(&mut self.frame, rest);
                        self.send_audio(frame)?;
                    }
                }
                Outgoing::Commit(tag) => {
                    self.send_rest()?;
                    // An open turn is told to end; one the service already
                    // ended is settled from what it reported.
                    if self.heard.open
                        || self.heard.turns.is_empty()
                        || self.heard.unheard_bytes > 0
                    {
                        self.force_endpoint()?;
                    }
                    self.heard.commits.push_back((tag, Instant::now()));
                }
                Outgoing::Clear => {
                    self.frame.clear();
                    if self.heard.open {
                        self.force_endpoint()?;
                        self.heard.discard += 1;
                    }
                    self.heard.turns.clear();
                    self.heard.partial.clear();
                    self.heard.unheard_bytes = 0;
                }
            }
        }
        Ok(())
    }

    /// Sends what is left of a frame, padded with silence to the shortest
    /// frame the service accepts.
    fn send_rest(&mut self) -> anyhow::Result<()> {
        if self.frame.is_empty() {
            return Ok(());
        }
        let mut frame = std::mem::take(&mut self.frame);
        if frame.len() < MIN_FRAME_BYTES {
            frame.resize(MIN_FRAME_BYTES, 0);
        }
        self.send_audio(frame)
    }

    fn send_audio(&mut self, frame: Vec<u8>) -> anyhow::Result<()> {
        self.heard.unheard_bytes += frame.len();
        self.socket.send(Message::binary(frame))?;
        Ok(())
    }

    fn force_endpoint(&mut self) -> anyhow::Result<()> {
        self.socket
            .send(Message::text(json!({"type": "ForceEndpoint"}).to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn turn(transcript: &str, ended: bool) -> Value {
        json!({"type": "Turn", "transcript": transcript, "end_of_turn": ended, "turn_is_formatted": ended})
    }

    #[test]
    fn the_session_asks_for_the_quickest_preset_and_the_call_language() {
        let url = session_url(STREAMING_URL, "");
        assert!(url.starts_with("wss://streaming.assemblyai.com/v3/ws?sample_rate=16000"));
        assert!(url.contains("speech_model=universal-3-6-pro"));
        assert!(url.contains("mode=min_latency"));
        assert!(!url.contains("language_codes"));
        assert!(session_url(STREAMING_URL, " fr ").ends_with("&language_codes=%5B%22fr%22%5D"));
        // Anything that is not a language code is left out.
        assert!(!session_url(STREAMING_URL, "fr&mode=x").contains("language_codes"));
    }

    #[test]
    fn a_key_is_accepted_when_a_session_begins_and_refused_on_an_error() {
        let serve = |reply: Value| {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            std::thread::spawn(move || {
                let (tcp, _) = listener.accept().unwrap();
                tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut ws = tungstenite::accept(tcp).unwrap();
                ws.send(Message::text(reply.to_string())).unwrap();
                // The check ends the session it opened.
                while let Ok(message) = ws.read() {
                    if message.is_text() {
                        break;
                    }
                }
            });
            session_url(&format!("ws://{address}/v3/ws"), "")
        };
        assert_eq!(
            check_key(&serve(json!({"type": "Begin", "id": "s"})), "k"),
            Ok(())
        );
        assert_eq!(
            check_key(
                &serve(
                    json!({"type": "Error", "error": "Unauthorized Connection: Invalid API key"})
                ),
                "k"
            ),
            Err("Unauthorized Connection: Invalid API key".to_owned())
        );
    }

    #[test]
    fn a_turn_the_service_ends_is_reported_at_once_and_settles_the_next_commit() {
        let mut heard = Heard::default();
        assert_eq!(
            heard.receive(&turn("what does it", false)).unwrap(),
            [LiveEvent::Partial("what does it".into())]
        );
        assert!(heard.open);
        assert_eq!(
            heard.receive(&turn("What does it cost?", true)).unwrap(),
            [LiveEvent::TurnEnded("What does it cost?".into())]
        );
        assert!(!heard.open);
        // The engine's commit comes later: the text is already there.
        heard.commits.push_back((7, Instant::now()));
        assert_eq!(
            heard.settle(Instant::now()).unwrap(),
            [LiveEvent::Committed {
                tag: 7,
                text: "What does it cost?".into()
            }]
        );
        assert!(heard.turns.is_empty());
    }

    #[test]
    fn a_commit_during_an_open_turn_waits_for_it_to_end() {
        let mut heard = Heard::default();
        heard.receive(&turn("Sure.", true)).unwrap();
        heard.receive(&turn("so the thing is", false)).unwrap();
        heard.commits.push_back((3, Instant::now()));
        assert!(heard.settle(Instant::now()).unwrap().is_empty());
        assert_eq!(
            heard.receive(&turn("So the thing is,", true)).unwrap(),
            [LiveEvent::Committed {
                tag: 3,
                text: "Sure. So the thing is,".into()
            }]
        );
        // One that never ends is a failure, so speech is uploaded instead.
        heard.receive(&turn("and", false)).unwrap();
        heard
            .commits
            .push_back((4, Instant::now() - COMMIT_TIMEOUT));
        assert!(heard.settle(Instant::now()).is_err());
    }

    #[test]
    fn a_commit_with_nothing_heard_yet_waits_briefly_for_words() {
        let mut heard = Heard::default();
        let made = Instant::now();
        heard.commits.push_back((1, made));
        assert!(
            heard
                .settle(made + Duration::from_millis(100))
                .unwrap()
                .is_empty()
        );
        // Nothing came: the speech had no words.
        assert_eq!(
            heard.settle(made + UNHEARD_WAIT).unwrap(),
            [LiveEvent::Committed {
                tag: 1,
                text: String::new()
            }]
        );
        // Speech sent after a turn ended may still be on its way.
        heard.receive(&turn("First.", true)).unwrap();
        heard.unheard_bytes = 1_000 * BYTES_PER_MS;
        heard.commits.push_back((2, made));
        assert!(
            heard
                .settle(made + Duration::from_millis(100))
                .unwrap()
                .is_empty()
        );
        assert_eq!(heard.settle(made + UNHEARD_WAIT).unwrap().len(), 1);
    }

    #[test]
    fn cleared_speech_is_never_reported() {
        let mut heard = Heard::default();
        heard.receive(&turn("um", false)).unwrap();
        // As a clear does while a turn is open.
        heard.discard += 1;
        heard.partial.clear();
        assert!(heard.receive(&turn("um", false)).unwrap().is_empty());
        assert!(heard.receive(&turn("Um.", true)).unwrap().is_empty());
        assert_eq!(heard.text(), "");
        assert!(
            heard
                .receive(&json!({"type": "Error", "error": "bad key"}))
                .is_err()
        );
    }

    #[test]
    fn speech_streams_in_frames_and_a_commit_ends_the_open_turn() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut ws = tungstenite::accept_hdr(
                tcp,
                |request: &tungstenite::handshake::server::Request, response| {
                    assert_eq!(request.headers()["Authorization"], "test-key");
                    assert!(request.uri().query().unwrap().contains("mode=min_latency"));
                    Ok(response)
                },
            )
            .unwrap();
            ws.send(Message::text(
                json!({"type": "Begin", "id": "s", "configuration": {"model": "universal-3-6-pro"}})
                    .to_string(),
            ))
            .unwrap();
            let mut audio = 0;
            let mut frames = Vec::new();
            loop {
                match ws.read().unwrap() {
                    Message::Binary(frame) => {
                        audio += frame.len();
                        frames.push(frame.len());
                        if audio == 4_800 {
                            ws.send(Message::text(turn("how much", false).to_string()))
                                .unwrap();
                        }
                    }
                    Message::Text(text) => {
                        let message: Value = serde_json::from_str(&text).unwrap();
                        assert_eq!(message["type"], "ForceEndpoint");
                        ws.send(Message::text(turn("How much?", true).to_string()))
                            .unwrap();
                        break;
                    }
                    _ => {}
                }
            }
            // 150 ms of speech in 50 ms frames, then the padded remainder.
            assert_eq!(frames, [1_600, 1_600, 1_600, 640]);
            // The session is ended, not just dropped.
            loop {
                match ws.read() {
                    Ok(Message::Text(text)) => {
                        let message: Value = serde_json::from_str(&text).unwrap();
                        assert_eq!(message["type"], "Terminate");
                        break;
                    }
                    Ok(_) => {}
                    Err(error) => panic!("no Terminate: {error}"),
                }
            }
        });

        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let stream = spawn(
            session_url(&format!("ws://{address}/v3/ws"), ""),
            "test-key".into(),
            Speaker::Them,
            Box::new(move |event| sink.lock().unwrap().push(event)),
        );
        assert!(stream.send(LiveCommand::Append(vec![0; 2_400])));
        let wait = |done: &dyn Fn(&[LiveEvent]) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !done(&seen.lock().unwrap()) {
                assert!(Instant::now() < deadline, "{:?}", seen.lock().unwrap());
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        wait(&|events| events.contains(&LiveEvent::Partial("how much".into())));
        assert!(stream.send(LiveCommand::Append(vec![0; 100])));
        assert!(stream.send(LiveCommand::Commit { tag: 9 }));
        wait(&|events| {
            events.contains(&LiveEvent::Committed {
                tag: 9,
                text: "How much?".into(),
            })
        });
        drop(stream);
        server.join().unwrap();
    }
}
