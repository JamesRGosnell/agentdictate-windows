//! Stopped dictations are processed on the daemon's worker: the daemon
//! keeps answering and recording while one transcribes, and pastes in the
//! order the dictations were stopped.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentdictate_app::{
    AppPaths, CapturedRecording, Daemon, DaemonError, Notice, ProcessingTranscriber,
    RecordingController, command_for_hotkey,
};
use agentdictate_core::{
    ClientCommand, ClientCommandKind, DictationNotice, JobId, JobStage, ProcessingStage,
    ServerMessage, ServerMessageKind, Settings, WorkflowPhase,
};
use agentdictate_linux::hotkey::HotkeySignal;
use agentdictate_runtime::{
    Deliverer, DeliveryDisposition, ExternalError, ExternalFailureKind, HistoryQuery, IpcClient,
    IpcHandler, IpcServer, Recorder, RecordingJob, Runtime, Transcriber, Transcript,
};
use tempfile::{TempDir, tempdir};

const WAIT: Duration = Duration::from_secs(10);

struct WavRecorder;

impl Recorder for WavRecorder {
    fn start(&mut self, job: &RecordingJob) -> Result<(), ExternalError> {
        std::fs::write(&job.audio_path, b"RIFFdictation audio").unwrap();
        Ok(())
    }
}

impl RecordingController for WavRecorder {
    fn finish(&mut self, _: &RecordingJob) -> Result<CapturedRecording, ExternalError> {
        Ok(CapturedRecording {
            duration_seconds: 3.0,
        })
    }
}

/// How the test lets the held transcription end.
enum Release {
    Transcribe,
    Fail,
}

/// Announces every transcription, and holds the first one until the test
/// releases it.
#[derive(Clone)]
struct HeldTranscriber {
    started: Sender<JobId>,
    held: Arc<Mutex<Option<Receiver<Release>>>>,
}

impl HeldTranscriber {
    fn new() -> (Self, Receiver<JobId>, Sender<Release>) {
        let (started, transcribing) = mpsc::channel();
        let (release, held) = mpsc::channel();
        (
            Self {
                started,
                held: Arc::new(Mutex::new(Some(held))),
            },
            transcribing,
            release,
        )
    }
}

impl Transcriber for HeldTranscriber {
    fn transcribe(&mut self, job: &RecordingJob) -> Result<Transcript, ExternalError> {
        let _ = self.started.send(job.id);
        let held = self.held.lock().unwrap().take();
        if let Some(release) = held {
            match release.recv() {
                Ok(Release::Transcribe) => {}
                Ok(Release::Fail) => {
                    return Err(ExternalError::with_kind(
                        ExternalFailureKind::LimitReached,
                        "ChatGPT transcription limit reached. Try again later.",
                    ));
                }
                Err(_) => return Err(ExternalError::new("the test ended")),
            }
        }
        Ok(Transcript {
            raw: format!("text of {}", job.id),
            final_text: format!("Text of {}.", job.id),
            cleaned_text: None,
            cleanup_error: None,
        })
    }
}

impl ProcessingTranscriber for HeldTranscriber {
    type Job = Self;

    fn for_job(&mut self, _: &RecordingJob) -> Self {
        self.clone()
    }
}

/// Records the order dictations were pasted in.
#[derive(Default)]
struct OrderedDelivery {
    pasted: Vec<JobId>,
}

impl Deliverer for OrderedDelivery {
    fn deliver(&mut self, job: &RecordingJob) -> Result<DeliveryDisposition, ExternalError> {
        self.pasted.push(job.id);
        Ok(DeliveryDisposition::Submitted {
            copied_to_clipboard: true,
            paste_triggered: true,
        })
    }
}

type TestDaemon = Daemon<WavRecorder, HeldTranscriber, OrderedDelivery>;

struct Fixture {
    daemon: TestDaemon,
    transcribing: Receiver<JobId>,
    release: Sender<Release>,
    notices: Arc<Mutex<Vec<Notice>>>,
    paths: AppPaths,
    directory: TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self::with_settings(Settings::default())
    }

    fn with_settings(settings: Settings) -> Self {
        let directory = tempdir().unwrap();
        let root = directory.path();
        let paths = AppPaths::from_roots(
            root.join("config"),
            root.join("data"),
            root.join("state"),
            root.join("cache"),
            root.join("runtime"),
        );
        std::fs::create_dir_all(paths.database_file.parent().unwrap()).unwrap();
        let runtime = Runtime::open(&paths.database_file).unwrap();
        let (transcriber, transcribing, release) = HeldTranscriber::new();
        let mut daemon = Daemon::new(
            runtime,
            settings,
            paths.clone(),
            WavRecorder,
            transcriber,
            OrderedDelivery::default(),
        );
        let notices = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&notices);
        daemon.set_notice_sink(Arc::new(move |notice: &Notice| {
            sink.lock().unwrap().push(notice.clone());
        }));
        Self {
            daemon,
            transcribing,
            release,
            notices,
            paths,
            directory,
        }
    }

    /// Starts and stops a dictation; the stop returns before it is pasted.
    fn dictate(&mut self) -> RecordingJob {
        self.daemon.start_recording().unwrap();
        self.daemon.stop_recording().unwrap()
    }

    fn observer(&self) -> Runtime {
        Runtime::open_observer(&self.paths.database_file).unwrap()
    }
}

fn phase(daemon: &TestDaemon) -> WorkflowPhase {
    daemon.snapshot().workflow.phase
}

const fn transcribing_phase(job_id: JobId) -> WorkflowPhase {
    WorkflowPhase::Processing {
        job_id,
        stage: ProcessingStage::Transcribing,
    }
}

#[test]
fn the_next_dictation_starts_while_one_transcribes_and_they_paste_in_stop_order() {
    let mut fixture = Fixture::new();
    let first = fixture.dictate();
    assert_eq!(first.stage, JobStage::Captured);
    assert_eq!(fixture.transcribing.recv_timeout(WAIT), Ok(first.id));
    assert_eq!(phase(&fixture.daemon), transcribing_phase(first.id));
    // A dictation being processed is not offered for Recovery.
    assert_eq!(fixture.daemon.snapshot().recoverable_count, 0);
    assert!(
        fixture
            .daemon
            .workspace_snapshot()
            .unwrap()
            .recoveries
            .is_empty()
    );
    assert!(matches!(
        fixture.daemon.retry_transcription(first.id),
        Err(DaemonError::StillProcessing)
    ));
    assert!(matches!(
        fixture.daemon.delete_recovery(first.id),
        Err(DaemonError::StillProcessing)
    ));

    // Escape ends a recording, never a dictation being processed.
    let escaped = fixture.daemon.start_recording().unwrap();
    assert_eq!(
        phase(&fixture.daemon),
        WorkflowPhase::Recording { job_id: escaped.id }
    );
    fixture.daemon.discard_recording().unwrap();
    assert_eq!(phase(&fixture.daemon), transcribing_phase(first.id));
    assert!(matches!(
        fixture.daemon.discard_recording(),
        Err(DaemonError::NotRecording)
    ));
    assert!(
        command_for_hotkey("toggle", HotkeySignal::Cancelled, phase(&fixture.daemon), 1).is_none()
    );

    let second = fixture.daemon.start_recording().unwrap();
    assert_eq!(
        phase(&fixture.daemon),
        WorkflowPhase::Recording { job_id: second.id }
    );
    fixture.daemon.stop_recording().unwrap();
    assert_eq!(phase(&fixture.daemon), transcribing_phase(second.id));
    // The later dictation waits for the earlier one.
    assert!(
        fixture
            .transcribing
            .recv_timeout(Duration::from_millis(200))
            .is_err()
    );
    assert!(fixture.daemon.deliverer().pasted.is_empty());

    fixture.release.send(Release::Transcribe).unwrap();

    assert!(fixture.daemon.wait_for_processing(WAIT));
    assert_eq!(fixture.transcribing.try_recv(), Ok(second.id));
    assert_eq!(fixture.daemon.deliverer().pasted, vec![first.id, second.id]);
    assert_eq!(phase(&fixture.daemon), WorkflowPhase::Ready);
    assert_eq!(
        fixture.daemon.snapshot().last_transcript,
        Some(format!("Text of {}.", second.id))
    );
    let history = fixture
        .observer()
        .list_history(HistoryQuery::default())
        .unwrap();
    assert_eq!(history.len(), 2);
    assert!(!first.audio_path.exists() && !second.audio_path.exists());
    assert!(fixture.notices.lock().unwrap().is_empty());
}

#[test]
fn a_dictation_that_fails_while_the_next_one_records_still_needs_attention() {
    let mut fixture = Fixture::new();
    let failed = fixture.dictate();
    assert_eq!(fixture.transcribing.recv_timeout(WAIT), Ok(failed.id));
    let recording = fixture.daemon.start_recording().unwrap();

    fixture.release.send(Release::Fail).unwrap();
    let outcome = fixture.daemon.processed(failed.id, WAIT).unwrap();

    assert!(outcome.unwrap_err().to_string().contains("limit reached"));
    assert_eq!(
        fixture.observer().job(failed.id).unwrap().unwrap().stage,
        JobStage::Failed
    );
    // The recording keeps the pill; the failure is announced and counted.
    assert_eq!(
        phase(&fixture.daemon),
        WorkflowPhase::Recording {
            job_id: recording.id
        }
    );
    assert_eq!(fixture.daemon.snapshot().recoverable_count, 1);
    let notices = std::mem::take(&mut *fixture.notices.lock().unwrap());
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].notice, DictationNotice::LimitReached);

    let delivered = fixture.daemon.stop_recording_and_wait().unwrap();
    assert_eq!(delivered.stage, JobStage::Delivered);
    assert_eq!(fixture.daemon.deliverer().pasted, vec![recording.id]);
    assert_eq!(fixture.daemon.snapshot().recoverable_count, 1);
    let recoveries = fixture.daemon.workspace_snapshot().unwrap().recoveries;
    assert_eq!(recoveries.len(), 1);
    assert_eq!(recoveries[0].job_id, failed.id);
}

#[test]
fn quitting_during_processing_leaves_unpasted_dictations_in_recovery_and_pastes_none_twice() {
    let mut fixture = Fixture::new();
    fixture
        .daemon
        .set_shutdown_grace(Duration::from_millis(100));
    let running = fixture.dictate();
    assert_eq!(fixture.transcribing.recv_timeout(WAIT), Ok(running.id));
    let waiting = fixture.dictate();

    let started = Instant::now();
    fixture.daemon.shutdown().unwrap();
    assert!(started.elapsed() >= Duration::from_millis(100));

    let observer = fixture.observer();
    let interrupted = observer.job(waiting.id).unwrap().unwrap();
    assert_eq!(interrupted.stage, JobStage::Interrupted);
    assert!(
        interrupted
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("shut down"))
    );
    assert!(interrupted.audio_path.is_file());
    assert_eq!(
        observer.job(running.id).unwrap().unwrap().stage,
        JobStage::Transcribing
    );

    // The transcription in progress ends, but nothing is pasted any more,
    // and no notice interrupts the quit.
    fixture.release.send(Release::Transcribe).unwrap();
    assert!(fixture.daemon.wait_for_processing(WAIT));
    assert!(fixture.daemon.deliverer().pasted.is_empty());
    assert!(fixture.notices.lock().unwrap().is_empty());
    assert_eq!(
        fixture
            .transcribing
            .recv_timeout(Duration::from_millis(200)),
        Err(mpsc::RecvTimeoutError::Timeout)
    );
    let saved = observer.job(running.id).unwrap().unwrap();
    assert_eq!(saved.stage, JobStage::ReadyToDeliver);
    assert_eq!(saved.final_text, format!("Text of {}.", running.id));
    drop(observer);

    // The next start offers both, and pastes neither by itself.
    drop(fixture.daemon);
    let restarted = Runtime::open(&fixture.paths.database_file).unwrap();
    let mut recoverable = restarted
        .recovery_entries()
        .unwrap()
        .into_iter()
        .map(|entry| (entry.job_id, entry.stage))
        .collect::<Vec<_>>();
    recoverable.sort_by_key(|(id, _)| *id != running.id);
    assert_eq!(
        recoverable,
        vec![
            (running.id, JobStage::ReadyToDeliver),
            (waiting.id, JobStage::Interrupted),
        ]
    );
    drop(fixture.directory);
}

#[test]
fn quitting_waits_for_a_dictation_that_pastes_within_the_grace() {
    let mut fixture = Fixture::new();
    let stopped = fixture.dictate();
    assert_eq!(fixture.transcribing.recv_timeout(WAIT), Ok(stopped.id));
    let release = fixture.release.clone();
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        release.send(Release::Transcribe).unwrap();
    });

    fixture.daemon.shutdown().unwrap();

    releaser.join().unwrap();
    assert_eq!(fixture.daemon.deliverer().pasted, vec![stopped.id]);
    assert!(fixture.daemon.wait_for_processing(Duration::ZERO));
    assert!(fixture.observer().job(stopped.id).unwrap().is_none());
    assert_eq!(
        fixture
            .observer()
            .list_history(HistoryQuery::default())
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn in_hold_mode_a_dictation_pastes_once_the_next_recordings_shortcut_is_released() {
    let mut fixture = Fixture::with_settings(Settings {
        recording_mode: "hold".into(),
        ..Settings::default()
    });
    let first = fixture.dictate();
    assert_eq!(fixture.transcribing.recv_timeout(WAIT), Ok(first.id));
    let second = fixture.daemon.start_recording().unwrap();

    fixture.release.send(Release::Transcribe).unwrap();

    // The shortcut is held down for the second recording; the first
    // dictation's paste waits for it instead of timing out.
    assert!(
        fixture
            .daemon
            .processed(first.id, Duration::from_millis(300))
            .is_none()
    );
    assert!(fixture.daemon.deliverer().pasted.is_empty());
    assert_eq!(
        phase(&fixture.daemon),
        WorkflowPhase::Recording { job_id: second.id }
    );
    fixture.daemon.stop_recording().unwrap();
    assert!(fixture.daemon.wait_for_processing(WAIT));
    assert_eq!(fixture.daemon.deliverer().pasted, vec![first.id, second.id]);
}

/// Serves the daemon over IPC the way the production daemon does: every
/// command under one lock around the whole daemon.
struct Served(TestDaemon);

impl IpcHandler for Served {
    fn snapshot(&self, request_id: u64) -> ServerMessage {
        ServerMessage::snapshot(request_id, self.0.snapshot(), self.0.settings())
    }

    fn handle(&mut self, command: ClientCommand) -> ServerMessage {
        let (request_id, result) = match command.kind {
            ClientCommandKind::GetSnapshot { request_id } => (request_id, Ok(())),
            ClientCommandKind::StartRecording { request_id, .. } => {
                (request_id, self.0.start_recording().map(|_| ()))
            }
            ClientCommandKind::StopRecording { request_id } => {
                (request_id, self.0.stop_recording().map(|_| ()))
            }
            _ => panic!("unexpected command"),
        };
        match result {
            Ok(()) => self.snapshot(request_id),
            Err(error) => ServerMessage::command_rejected(request_id, error.to_string()),
        }
    }
}

fn snapshot_phase(message: &ServerMessage) -> WorkflowPhase {
    match &message.kind {
        ServerMessageKind::Snapshot { snapshot, .. } => snapshot.workflow.phase,
        other => panic!("expected a snapshot, got {other:?}"),
    }
}

#[test]
fn other_clients_are_answered_and_the_next_start_is_accepted_while_a_dictation_transcribes() {
    let fixture = Fixture::new();
    let Fixture {
        daemon,
        transcribing,
        release,
        paths,
        directory: _directory,
        ..
    } = fixture;
    let served = Arc::new(Mutex::new(Served(daemon)));
    let server = IpcServer::bind(&paths.runtime).unwrap();
    let handler = Arc::clone(&served);
    std::thread::spawn(move || {
        loop {
            if server.serve_next_concurrent(Arc::clone(&handler)).is_err() {
                return;
            }
        }
    });

    let (mut hotkey, _) = IpcClient::connect(&paths.runtime).unwrap();
    let started = hotkey.send(ClientCommand::start_recording(1)).unwrap();
    let WorkflowPhase::Recording { job_id: first } = snapshot_phase(&started) else {
        panic!("the first dictation did not start")
    };
    // The stop returns once the capture is durable, so the hotkey
    // dispatcher's in-flight action ends here, not after the paste.
    let stopped = hotkey.send(ClientCommand::stop_recording(2)).unwrap();
    assert_eq!(snapshot_phase(&stopped), transcribing_phase(first));
    assert_eq!(transcribing.recv_timeout(WAIT), Ok(first));

    let asked = Instant::now();
    let (mut settings, initial) = IpcClient::connect(&paths.runtime).unwrap();
    let answer = settings.send(ClientCommand::get_snapshot(3)).unwrap();
    let elapsed = asked.elapsed();
    assert!(
        elapsed < Duration::from_millis(200),
        "a snapshot took {elapsed:?}"
    );
    assert_eq!(snapshot_phase(&initial), transcribing_phase(first));
    assert_eq!(snapshot_phase(&answer), transcribing_phase(first));

    let next = hotkey.send(ClientCommand::start_recording(4)).unwrap();
    let WorkflowPhase::Recording { job_id: second } = snapshot_phase(&next) else {
        panic!("the next dictation did not start while the first transcribed")
    };
    hotkey.send(ClientCommand::stop_recording(5)).unwrap();
    release.send(Release::Transcribe).unwrap();

    let served = served.lock().unwrap();
    assert!(served.0.wait_for_processing(WAIT));
    assert_eq!(served.0.deliverer().pasted, vec![first, second]);
}
