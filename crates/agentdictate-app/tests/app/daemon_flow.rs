#[cfg(unix)]
use agentdictate_app::{OverlayUpdate, start_overlay_presenter};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;

use agentdictate_app::{
    AppPaths, CapturedRecording, Daemon, ProcessingTranscriber, RecordingController,
};
use agentdictate_core::{HistoryPageRequest, HotkeyReadiness, JobStage, Settings, WorkflowPhase};
use agentdictate_runtime::{
    Deliverer, DeliveryDisposition, ExternalError, FocusedWindow, HistoryQuery, Recorder,
    RecordingJob, Runtime, Transcriber, Transcript,
};
use rusqlite::params;
use tempfile::tempdir;

use super::support::{FailingStartRecorder, FixedTranscriber, InspectingRecorder};

struct FailingFinishRecorder;

#[derive(Default)]
struct PreservingRecorder {
    finish_attempts: usize,
}

impl Recorder for PreservingRecorder {
    fn start(&mut self, job: &RecordingJob) -> Result<(), ExternalError> {
        std::fs::write(&job.audio_path, b"RIFFpreserved audio").unwrap();
        Ok(())
    }
}

impl RecordingController for PreservingRecorder {
    fn finish(&mut self, _job: &RecordingJob) -> Result<CapturedRecording, ExternalError> {
        self.finish_attempts += 1;
        Ok(CapturedRecording {
            duration_seconds: 27.5,
        })
    }
}

/// Records a take of the given length in seconds, writing a WAV so audio retention is observable.
struct TimedRecorder(f64);

impl Recorder for TimedRecorder {
    fn start(&mut self, job: &RecordingJob) -> Result<(), ExternalError> {
        std::fs::write(&job.audio_path, b"RIFFtimed audio").unwrap();
        Ok(())
    }
}

impl RecordingController for TimedRecorder {
    fn finish(&mut self, _job: &RecordingJob) -> Result<CapturedRecording, ExternalError> {
        Ok(CapturedRecording {
            duration_seconds: self.0,
        })
    }
}

impl Recorder for FailingFinishRecorder {
    fn start(&mut self, job: &RecordingJob) -> Result<(), ExternalError> {
        std::fs::write(&job.audio_path, b"RIFFpartial audio").unwrap();
        Ok(())
    }
}

impl RecordingController for FailingFinishRecorder {
    fn finish(&mut self, _job: &RecordingJob) -> Result<CapturedRecording, ExternalError> {
        Err(ExternalError::new("recorder disappeared"))
    }
}

#[derive(Default)]
struct SubmittedDelivery {
    attempts: usize,
}

#[test]
fn empty_dictation_finishes_without_delivery_history_or_recovery_and_allows_the_next_start() {
    struct Empty;
    impl agentdictate_app::SpeechTransport for Empty {
        fn transcribe_audio(
            &mut self,
            _: agentdictate_app::TranscriptionRequest<'_>,
        ) -> Result<String, ExternalError> {
            panic!("an accidental tap must not be uploaded")
        }
    }
    impl agentdictate_app::JobTransport for Empty {
        fn for_job(&mut self, _: &RecordingJob) -> Self {
            Self
        }
    }
    impl agentdictate_app::CleanupTransport for Empty {
        fn cleanup_text(
            &mut self,
            _: agentdictate_app::CleanupRequest<'_>,
        ) -> Result<String, ExternalError> {
            panic!("an accidental tap must not trigger cleanup")
        }
    }
    struct TapRecorder;
    impl Recorder for TapRecorder {
        fn start(&mut self, job: &RecordingJob) -> Result<(), ExternalError> {
            let mut wav = b"RIFF\x64\x01\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x80\x3e\0\0\0\x7d\0\0\x02\0\x10\0data\x40\x01\0\0".to_vec();
            for _ in 0..160 {
                wav.extend_from_slice(&2000i16.to_le_bytes());
            }
            std::fs::write(&job.audio_path, wav).unwrap();
            Ok(())
        }
    }
    impl RecordingController for TapRecorder {
        fn finish(&mut self, _: &RecordingJob) -> Result<CapturedRecording, ExternalError> {
            Ok(CapturedRecording {
                duration_seconds: 0.01,
            })
        }
    }
    let dir = tempdir().unwrap();
    let paths = app_paths(dir.path());
    std::fs::create_dir_all(paths.database_file.parent().unwrap()).unwrap();
    let runtime = Runtime::open(&paths.database_file).unwrap();
    let mut daemon = Daemon::new(
        runtime,
        Settings::default(),
        paths.clone(),
        TapRecorder,
        agentdictate_app::TranscriptionPipeline::new(Settings::default(), Empty, Empty),
        SubmittedDelivery::default(),
    );
    daemon.start_recording().unwrap();
    let finished = daemon.stop_recording_and_wait().unwrap();
    assert_eq!(finished.stage, JobStage::NoSpeech);
    assert_eq!(daemon.snapshot().workflow.phase, WorkflowPhase::Ready);
    assert_eq!(daemon.snapshot().recoverable_count, 0);
    assert_eq!(daemon.deliverer().attempts, 0);
    assert!(!finished.audio_path.exists());
    assert!(daemon.workspace_snapshot().unwrap().recoveries.is_empty());
    let observer = Runtime::open_observer(&paths.database_file).unwrap();
    assert!(
        observer
            .list_history(HistoryQuery::default())
            .unwrap()
            .is_empty()
    );
    assert!(observer.recoverable_jobs().unwrap().is_empty());
    assert!(observer.job(finished.id).unwrap().is_none());
    daemon.start_recording().unwrap();
}

impl Deliverer for SubmittedDelivery {
    fn deliver(&mut self, _job: &RecordingJob) -> Result<DeliveryDisposition, ExternalError> {
        self.attempts += 1;
        Ok(DeliveryDisposition::Submitted {
            copied_to_clipboard: true,
            paste_triggered: true,
        })
    }
}

#[cfg(unix)]
struct ExitInspectingDelivery {
    helper_exited: PathBuf,
    delivered_after_exit: bool,
}

#[cfg(unix)]
impl Deliverer for ExitInspectingDelivery {
    fn deliver(&mut self, _job: &RecordingJob) -> Result<DeliveryDisposition, ExternalError> {
        self.delivered_after_exit = self.helper_exited.is_file();
        Ok(DeliveryDisposition::Submitted {
            copied_to_clipboard: true,
            paste_triggered: true,
        })
    }
}

const EDITOR: FocusedWindow = FocusedWindow {
    window: 0x1234,
    process_id: 40,
};
const BROWSER: FocusedWindow = FocusedWindow {
    window: 0x5678,
    process_id: 41,
};

/// The desktop's focused window, shared by the fakes so transcription can
/// move it the way a user switching windows would.
#[derive(Clone, Default)]
struct SharedFocus(std::sync::Arc<std::sync::Mutex<Option<FocusedWindow>>>);

impl SharedFocus {
    fn set(&self, window: Option<FocusedWindow>) {
        *self.0.lock().unwrap() = window;
    }

    fn get(&self) -> Option<FocusedWindow> {
        *self.0.lock().unwrap()
    }
}

/// Moves the focus to the browser while the transcript is produced.
struct FocusMovingTranscriber(SharedFocus);

impl Transcriber for FocusMovingTranscriber {
    fn transcribe(&mut self, job: &RecordingJob) -> Result<Transcript, ExternalError> {
        self.0.set(Some(BROWSER));
        FixedTranscriber.transcribe(job)
    }
}

impl ProcessingTranscriber for FocusMovingTranscriber {
    type Job = Self;

    fn for_job(&mut self, _: &RecordingJob) -> Self {
        Self(self.0.clone())
    }
}

/// Records which stop window each delivery was asked to respect, and
/// leaves the first one unsent so it can be retried from Recovery.
struct StopWindowDelivery {
    focus: SharedFocus,
    received: Vec<Option<FocusedWindow>>,
}

impl Deliverer for StopWindowDelivery {
    fn deliver(&mut self, job: &RecordingJob) -> Result<DeliveryDisposition, ExternalError> {
        self.deliver_to(job, None)
    }

    fn focused_window(&mut self) -> Option<FocusedWindow> {
        self.focus.get()
    }

    fn deliver_to(
        &mut self,
        _job: &RecordingJob,
        stopped_in: Option<FocusedWindow>,
    ) -> Result<DeliveryDisposition, ExternalError> {
        self.received.push(stopped_in);
        Ok(if self.received.len() == 1 {
            DeliveryDisposition::NotSent {
                copied_to_clipboard: false,
                reason: "held for the retry".into(),
            }
        } else {
            DeliveryDisposition::Submitted {
                copied_to_clipboard: true,
                paste_triggered: true,
            }
        })
    }
}

#[test]
fn delivery_receives_the_window_focused_at_stop_and_a_retry_receives_none() {
    let directory = tempdir().unwrap();
    let paths = app_paths(directory.path());
    std::fs::create_dir_all(paths.database_file.parent().unwrap()).unwrap();
    let runtime = Runtime::open(&paths.database_file).unwrap();
    let focus = SharedFocus::default();
    let mut daemon = Daemon::new(
        runtime,
        Settings::default(),
        paths,
        TimedRecorder(3.0),
        FocusMovingTranscriber(focus.clone()),
        StopWindowDelivery {
            focus: focus.clone(),
            received: Vec::new(),
        },
    );

    daemon.start_recording().unwrap();
    focus.set(Some(EDITOR));
    let stopped = daemon.stop_recording_and_wait().unwrap();
    // The focus moved to the browser during transcription; delivery still
    // gets the editor the user stopped in.
    assert_eq!(focus.get(), Some(BROWSER));
    assert_eq!(daemon.deliverer().received, vec![Some(EDITOR)]);
    assert_ne!(stopped.stage, JobStage::Delivered);

    let retried = daemon.retry_delivery(stopped.id).unwrap();
    assert_eq!(retried.stage, JobStage::Delivered);
    assert_eq!(daemon.deliverer().received, vec![Some(EDITOR), None]);
}

#[test]
fn daemon_checkpoints_audio_before_capture_and_transcript_before_delivery() {
    let directory = tempdir().unwrap();
    let paths = app_paths(directory.path());
    std::fs::create_dir_all(paths.database_file.parent().unwrap()).unwrap();
    let runtime = Runtime::open(&paths.database_file).unwrap();
    let recorder = InspectingRecorder {
        database: paths.database_file.clone(),
        started_after_checkpoint: false,
    };
    let mut daemon = Daemon::new(
        runtime,
        Settings::default(),
        paths.clone(),
        recorder,
        FixedTranscriber,
        SubmittedDelivery::default(),
    );

    let started = daemon.start_recording().unwrap();
    assert!(daemon.recorder().started_after_checkpoint);
    assert_eq!(started.stage, JobStage::Recording);
    assert!(matches!(
        daemon.snapshot().workflow.phase,
        WorkflowPhase::Recording { job_id } if job_id == started.id
    ));
    assert_eq!(daemon.snapshot().recoverable_count, 0);
    assert!(daemon.workspace_snapshot().unwrap().recoveries.is_empty());

    let delivered = daemon.stop_recording_and_wait().unwrap();

    assert_eq!(delivered.stage, JobStage::Delivered);
    assert_eq!(delivered.raw_transcript, "raw transcript");
    assert_eq!(delivered.final_text, "Final transcript.");
    assert!(!delivered.audio_path.exists());
    assert_eq!(daemon.deliverer().attempts, 1);
    assert_eq!(daemon.snapshot().workflow.phase, WorkflowPhase::Ready);
    assert_eq!(
        daemon.snapshot().last_transcript.as_deref(),
        Some("Final transcript.")
    );
    assert_eq!(daemon.snapshot().hotkey, HotkeyReadiness::Starting);
    let observer = Runtime::open_observer(&paths.database_file).unwrap();
    let history = observer.list_history(HistoryQuery::default()).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].job_id, Some(delivered.id));
    assert_eq!(history[0].final_text, "Final transcript.");
    // The transcript now lives only in History.
    assert!(observer.job(delivered.id).unwrap().is_none());
}

#[test]
#[cfg(unix)]
fn daemon_waits_for_overlay_exit_before_delivery() {
    let directory = tempdir().unwrap();
    let paths = app_paths(directory.path());
    std::fs::create_dir_all(paths.database_file.parent().unwrap()).unwrap();
    let executable = directory.path().join("overlay-helper");
    let received = directory.path().join("received");
    let helper_exited = directory.path().join("helper-exited");
    std::fs::write(
        &executable,
        format!(
            "#!/bin/sh\nwhile IFS= read -r line; do printf '%s\\n' \"$line\" >> '{}'; done\nprintf 'exited' > '{}'\n",
            received.display(),
            helper_exited.display(),
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&executable, permissions).unwrap();
    let (overlay, presenter) = start_overlay_presenter(executable).unwrap();
    let runtime = Runtime::open(&paths.database_file).unwrap();
    let recorder = InspectingRecorder {
        database: paths.database_file.clone(),
        started_after_checkpoint: false,
    };
    let mut daemon = Daemon::new(
        runtime,
        Settings::default(),
        paths,
        recorder,
        FixedTranscriber,
        ExitInspectingDelivery {
            helper_exited: helper_exited.clone(),
            delivered_after_exit: false,
        },
    );
    daemon.set_overlay_controller(overlay);

    let started = daemon.start_recording().unwrap();
    let delivered = daemon.stop_recording_and_wait().unwrap();

    assert_eq!(delivered.stage, JobStage::Delivered);
    assert!(daemon.deliverer().delivered_after_exit);
    assert_eq!(std::fs::read_to_string(helper_exited).unwrap(), "exited");
    let encoded = std::fs::read_to_string(received).unwrap();
    let updates: Vec<OverlayUpdate> = encoded
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(matches!(
        updates[0].workflow.phase,
        WorkflowPhase::Starting { .. }
    ));
    assert!(updates[0].active_recording.is_none());
    let recording = updates
        .iter()
        .find(|update| matches!(update.workflow.phase, WorkflowPhase::Recording { .. }))
        .unwrap();
    assert_eq!(
        recording
            .active_recording
            .as_ref()
            .map(|active| active.audio_path.as_path()),
        Some(started.audio_path.as_path())
    );
    let overlay_started_at = recording
        .active_recording
        .as_ref()
        .map(|active| active.started_at_unix_millis)
        .unwrap();
    assert!(overlay_started_at >= started.started_at.timestamp_millis());
    assert!(matches!(
        recording.workflow.phase,
        WorkflowPhase::Recording { job_id } if job_id == started.id
    ));
    assert!(!encoded.contains("last_transcript"));
    assert!(!encoded.contains("recoverable_count"));
    drop(daemon);
    presenter.join().unwrap();
}

#[test]
fn recorder_finalize_failure_is_immediately_durable_and_recoverable() {
    let directory = tempdir().unwrap();
    let paths = app_paths(directory.path());
    std::fs::create_dir_all(paths.database_file.parent().unwrap()).unwrap();
    let runtime = Runtime::open(&paths.database_file).unwrap();
    let mut daemon = Daemon::new(
        runtime,
        Settings::default(),
        paths.clone(),
        FailingFinishRecorder,
        FixedTranscriber,
        SubmittedDelivery::default(),
    );
    let started = daemon.start_recording().unwrap();

    assert!(daemon.stop_recording().is_err());

    let observer = Runtime::open_observer(&paths.database_file).unwrap();
    let interrupted = observer.job(started.id).unwrap().unwrap();
    assert_eq!(interrupted.stage, JobStage::Interrupted);
    assert!(interrupted.audio_path.exists());
    assert!(
        interrupted
            .error_message
            .as_deref()
            .unwrap()
            .contains("recorder disappeared")
    );
    assert_eq!(daemon.snapshot().recoverable_count, 1);
}

#[test]
fn stop_capture_checkpoint_failure_clears_the_session_and_preserves_audio() {
    let directory = tempdir().unwrap();
    let paths = app_paths(directory.path());
    std::fs::create_dir_all(paths.database_file.parent().unwrap()).unwrap();
    let runtime = Runtime::open(&paths.database_file).unwrap();
    let mut daemon = Daemon::new(
        runtime,
        Settings::default(),
        paths.clone(),
        PreservingRecorder::default(),
        FixedTranscriber,
        SubmittedDelivery::default(),
    );
    let started = daemon.start_recording().unwrap();
    let connection = rusqlite::Connection::open(&paths.database_file).unwrap();
    reject_capture_checkpoint(&connection);

    let error = daemon.stop_recording().unwrap_err();

    assert!(error.to_string().contains("capture checkpoint unavailable"));
    assert_eq!(daemon.recorder().finish_attempts, 1);
    assert!(matches!(
        daemon.snapshot().workflow.phase,
        WorkflowPhase::NeedsAttention {
            job_id,
            at: JobStage::Interrupted,
        } if job_id == started.id
    ));
    assert_eq!(daemon.snapshot().recoverable_count, 1);
    let observer = Runtime::open_observer(&paths.database_file).unwrap();
    let interrupted = observer.job(started.id).unwrap().unwrap();
    assert_eq!(interrupted.stage, JobStage::Interrupted);
    assert!(interrupted.audio_path.is_file());
    drop(observer);

    connection
        .execute_batch("DROP TRIGGER reject_capture_checkpoint")
        .unwrap();
    let next = daemon.start_recording().unwrap();
    assert_ne!(next.id, started.id);
    daemon.discard_recording().unwrap();
}

#[test]
fn failure_after_transcription_leaves_a_failed_recovery_and_allows_the_next_start() {
    let directory = tempdir().unwrap();
    let paths = app_paths(directory.path());
    std::fs::create_dir_all(paths.database_file.parent().unwrap()).unwrap();
    let runtime = Runtime::open(&paths.database_file).unwrap();
    let mut daemon = Daemon::new(
        runtime,
        Settings::default(),
        paths.clone(),
        PreservingRecorder::default(),
        FixedTranscriber,
        SubmittedDelivery::default(),
    );
    let started = daemon.start_recording().unwrap();
    let connection = rusqlite::Connection::open(&paths.database_file).unwrap();
    connection
        .execute_batch(
            r#"
            CREATE TRIGGER reject_ready_checkpoint
            BEFORE UPDATE OF stage ON dictation_jobs
            WHEN NEW.stage = 'ready_to_deliver'
            BEGIN
                SELECT RAISE(FAIL, 'ready checkpoint unavailable');
            END;
            "#,
        )
        .unwrap();

    let error = daemon.stop_recording_and_wait().unwrap_err();

    assert!(error.to_string().contains("ready checkpoint unavailable"));
    assert_eq!(daemon.deliverer().attempts, 0);
    assert!(matches!(
        daemon.snapshot().workflow.phase,
        WorkflowPhase::NeedsAttention {
            job_id,
            at: JobStage::Failed,
        } if job_id == started.id
    ));
    assert_eq!(daemon.snapshot().recoverable_count, 1);
    let failed = Runtime::open_observer(&paths.database_file)
        .unwrap()
        .job(started.id)
        .unwrap()
        .unwrap();
    assert_eq!(failed.stage, JobStage::Failed);
    assert_eq!(failed.raw_transcript, "raw transcript");

    let next = daemon.start_recording().unwrap();
    assert_ne!(next.id, started.id);
    daemon.discard_recording().unwrap();
    connection
        .execute_batch("DROP TRIGGER reject_ready_checkpoint")
        .unwrap();
    let delivered = daemon.retry_transcription(started.id).unwrap();
    assert_eq!(delivered.stage, JobStage::Delivered);
}

#[test]
fn deleting_another_recovery_item_keeps_a_live_recording_stoppable() {
    let directory = tempdir().unwrap();
    let paths = app_paths(directory.path());
    std::fs::create_dir_all(&paths.recordings).unwrap();
    let mut runtime = Runtime::open(&paths.database_file).unwrap();
    let old = runtime
        .start_recording(
            agentdictate_runtime::RecordingRequest {
                options: None,
                audio_path: paths.recordings.join("old.wav"),
                started_at: chrono::Utc::now(),
                transcription_provider: Default::default(),
                transcription_model: "gpt-transcribe".into(),
            },
            &mut PreservingRecorder::default(),
        )
        .unwrap();
    runtime.capture_recording(old.id, 3.0).unwrap();
    runtime
        .interrupt_job(old.id, JobStage::Captured, "left for Recovery")
        .unwrap();
    let mut daemon = Daemon::new(
        runtime,
        Settings::default(),
        paths.clone(),
        PreservingRecorder::default(),
        FixedTranscriber,
        SubmittedDelivery::default(),
    );
    let live = daemon.start_recording().unwrap();

    daemon.delete_recovery(old.id).unwrap();

    assert!(matches!(
        daemon.snapshot().workflow.phase,
        WorkflowPhase::Recording { job_id } if job_id == live.id
    ));
    assert_eq!(daemon.snapshot().recoverable_count, 0);
    let delivered = daemon.stop_recording_and_wait().unwrap();
    assert_eq!(delivered.stage, JobStage::Delivered);
    assert_eq!(daemon.recorder().finish_attempts, 1);
    assert_eq!(daemon.snapshot().workflow.phase, WorkflowPhase::Ready);
}

#[test]
fn escape_on_a_short_recording_keeps_its_audio_only_when_audio_retention_is_enabled() {
    for preserve_temp_audio in [false, true] {
        let directory = tempdir().unwrap();
        let paths = app_paths(directory.path());
        std::fs::create_dir_all(paths.database_file.parent().unwrap()).unwrap();
        let runtime = Runtime::open(&paths.database_file).unwrap();
        let settings = Settings {
            preserve_temp_audio,
            ..Settings::default()
        };
        let mut daemon = Daemon::new(
            runtime,
            settings,
            paths.clone(),
            TimedRecorder(5.0),
            FixedTranscriber,
            SubmittedDelivery::default(),
        );
        let started = daemon.start_recording().unwrap();

        let discarded = daemon.discard_recording().unwrap();

        assert_eq!(discarded.stage, JobStage::Deleted);
        assert_eq!(started.audio_path.exists(), preserve_temp_audio);
        assert_eq!(daemon.snapshot().workflow.phase, WorkflowPhase::Ready);
        assert_eq!(daemon.snapshot().recoverable_count, 0);
        assert!(daemon.workspace_snapshot().unwrap().recoveries.is_empty());
    }
}

#[test]
fn escape_on_a_long_recording_keeps_it_in_recovery_without_demanding_attention() {
    let directory = tempdir().unwrap();
    let paths = app_paths(directory.path());
    std::fs::create_dir_all(paths.database_file.parent().unwrap()).unwrap();
    let runtime = Runtime::open(&paths.database_file).unwrap();
    let mut daemon = Daemon::new(
        runtime,
        Settings::default(),
        paths.clone(),
        TimedRecorder(42.0),
        FixedTranscriber,
        SubmittedDelivery::default(),
    );
    let started = daemon.start_recording().unwrap();

    let canceled = daemon.discard_recording().unwrap();

    assert_eq!(canceled.stage, JobStage::Canceled);
    assert!(started.audio_path.is_file());
    assert_eq!(daemon.snapshot().workflow.phase, WorkflowPhase::Ready);
    assert_eq!(daemon.snapshot().recoverable_count, 0);
    let recoveries = daemon.workspace_snapshot().unwrap().recoveries;
    assert_eq!(recoveries.len(), 1);
    assert_eq!(recoveries[0].job_id, started.id);
    assert_eq!(recoveries[0].stage, JobStage::Canceled);
    assert!(recoveries[0].audio_present);

    let delivered = daemon.retry_transcription(started.id).unwrap();
    assert_eq!(delivered.stage, JobStage::Delivered);
    assert_eq!(daemon.deliverer().attempts, 1);
    assert!(daemon.workspace_snapshot().unwrap().recoveries.is_empty());
}

#[test]
fn failed_escape_delete_restores_audio_and_surfaces_recovery_attention() {
    let directory = tempdir().unwrap();
    let paths = app_paths(directory.path());
    std::fs::create_dir_all(paths.database_file.parent().unwrap()).unwrap();
    let runtime = Runtime::open(&paths.database_file).unwrap();
    let mut daemon = Daemon::new(
        runtime,
        Settings::default(),
        paths.clone(),
        TimedRecorder(2.0),
        FixedTranscriber,
        SubmittedDelivery::default(),
    );
    let started = daemon.start_recording().unwrap();
    rusqlite::Connection::open(&paths.database_file)
        .unwrap()
        .execute_batch(
            r#"
            CREATE TRIGGER reject_escape_delete
            BEFORE DELETE ON dictation_jobs
            BEGIN
                SELECT RAISE(ABORT, 'forced discard failure');
            END;
            "#,
        )
        .unwrap();

    let error = daemon.discard_recording().unwrap_err();

    assert!(error.to_string().contains("forced discard failure"));
    assert!(started.audio_path.is_file());
    assert!(matches!(
        daemon.snapshot().workflow.phase,
        WorkflowPhase::NeedsAttention {
            job_id,
            at: JobStage::Captured,
        } if job_id == started.id
    ));
    assert_eq!(daemon.snapshot().recoverable_count, 1);
}

#[test]
fn escape_finalize_failure_preserves_partial_audio_for_recovery() {
    let directory = tempdir().unwrap();
    let paths = app_paths(directory.path());
    std::fs::create_dir_all(paths.database_file.parent().unwrap()).unwrap();
    let runtime = Runtime::open(&paths.database_file).unwrap();
    let mut daemon = Daemon::new(
        runtime,
        Settings::default(),
        paths.clone(),
        FailingFinishRecorder,
        FixedTranscriber,
        SubmittedDelivery::default(),
    );
    let started = daemon.start_recording().unwrap();

    let interrupted = daemon.discard_recording().unwrap();

    assert_eq!(interrupted.stage, JobStage::Interrupted);
    assert!(interrupted.audio_path.is_file());
    assert!(matches!(
        daemon.snapshot().workflow.phase,
        WorkflowPhase::NeedsAttention {
            job_id,
            at: JobStage::Interrupted,
        } if job_id == started.id
    ));
    assert_eq!(daemon.snapshot().recoverable_count, 1);
}

#[test]
fn discard_capture_checkpoint_failure_clears_the_session_and_preserves_audio() {
    let directory = tempdir().unwrap();
    let paths = app_paths(directory.path());
    std::fs::create_dir_all(paths.database_file.parent().unwrap()).unwrap();
    let runtime = Runtime::open(&paths.database_file).unwrap();
    let mut daemon = Daemon::new(
        runtime,
        Settings::default(),
        paths.clone(),
        PreservingRecorder::default(),
        FixedTranscriber,
        SubmittedDelivery::default(),
    );
    let started = daemon.start_recording().unwrap();
    let connection = rusqlite::Connection::open(&paths.database_file).unwrap();
    reject_capture_checkpoint(&connection);

    let error = daemon.discard_recording().unwrap_err();

    assert!(error.to_string().contains("capture checkpoint unavailable"));
    assert_eq!(daemon.recorder().finish_attempts, 1);
    assert!(matches!(
        daemon.snapshot().workflow.phase,
        WorkflowPhase::NeedsAttention {
            job_id,
            at: JobStage::Interrupted,
        } if job_id == started.id
    ));
    assert_eq!(daemon.snapshot().recoverable_count, 1);
    let observer = Runtime::open_observer(&paths.database_file).unwrap();
    let interrupted = observer.job(started.id).unwrap().unwrap();
    assert_eq!(interrupted.stage, JobStage::Interrupted);
    assert!(interrupted.audio_path.is_file());
    drop(observer);

    connection
        .execute_batch("DROP TRIGGER reject_capture_checkpoint")
        .unwrap();
    let next = daemon.start_recording().unwrap();
    assert_ne!(next.id, started.id);
    daemon.discard_recording().unwrap();
}

#[test]
fn recorder_start_failure_is_published_as_recoverable_attention() {
    let directory = tempdir().unwrap();
    let paths = app_paths(directory.path());
    std::fs::create_dir_all(paths.database_file.parent().unwrap()).unwrap();
    let runtime = Runtime::open(&paths.database_file).unwrap();
    let mut daemon = Daemon::new(
        runtime,
        Settings::default(),
        paths,
        FailingStartRecorder,
        FixedTranscriber,
        SubmittedDelivery::default(),
    );

    assert!(daemon.start_recording().is_err());

    assert_eq!(daemon.snapshot().recoverable_count, 1);
    assert!(matches!(
        daemon.snapshot().workflow.phase,
        WorkflowPhase::NeedsAttention {
            at: JobStage::Interrupted,
            ..
        }
    ));
}

#[test]
fn unexpected_recorder_exit_preserves_audio_for_recovery_without_transcribing() {
    let directory = tempdir().unwrap();
    let paths = app_paths(directory.path());
    std::fs::create_dir_all(paths.database_file.parent().unwrap()).unwrap();
    let runtime = Runtime::open(&paths.database_file).unwrap();
    let recorder = InspectingRecorder {
        database: paths.database_file.clone(),
        started_after_checkpoint: false,
    };
    let mut daemon = Daemon::new(
        runtime,
        Settings::default(),
        paths.clone(),
        recorder,
        FixedTranscriber,
        SubmittedDelivery::default(),
    );
    let started = daemon.start_recording().unwrap();

    let recovered = daemon.recorder_exited(started.id).unwrap().unwrap();

    assert_eq!(recovered.stage, JobStage::Interrupted);
    assert!(recovered.audio_path.is_file());
    assert!(recovered.final_text.is_empty());
    assert!(matches!(
        daemon.snapshot().workflow.phase,
        WorkflowPhase::NeedsAttention {
            job_id,
            at: JobStage::Interrupted,
        } if job_id == started.id
    ));
    assert_eq!(daemon.snapshot().recoverable_count, 1);
    assert!(daemon.recorder_exited(started.id).unwrap().is_none());
    let observer = Runtime::open_observer(&paths.database_file).unwrap();
    assert!(
        observer
            .list_history(HistoryQuery::default())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn graceful_shutdown_finalizes_and_preserves_active_audio_for_recovery() {
    let directory = tempdir().unwrap();
    let paths = app_paths(directory.path());
    std::fs::create_dir_all(paths.database_file.parent().unwrap()).unwrap();
    let runtime = Runtime::open(&paths.database_file).unwrap();
    let recorder = InspectingRecorder {
        database: paths.database_file.clone(),
        started_after_checkpoint: false,
    };
    let mut daemon = Daemon::new(
        runtime,
        Settings::default(),
        paths.clone(),
        recorder,
        FixedTranscriber,
        SubmittedDelivery::default(),
    );
    let started = daemon.start_recording().unwrap();

    daemon.shutdown().unwrap();

    let observer = Runtime::open_observer(&paths.database_file).unwrap();
    let preserved = observer.job(started.id).unwrap().unwrap();
    assert_eq!(preserved.stage, JobStage::Interrupted);
    assert!(preserved.audio_path.is_file());
    assert!(
        preserved
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("shut down"))
    );
    assert!(matches!(
        daemon.snapshot().workflow.phase,
        WorkflowPhase::NeedsAttention {
            job_id,
            at: JobStage::Interrupted,
        } if job_id == started.id
    ));
    assert_eq!(daemon.snapshot().recoverable_count, 1);
}

#[test]
fn workspace_history_is_bounded_even_when_the_archive_is_large() {
    let directory = tempdir().unwrap();
    let paths = app_paths(directory.path());
    std::fs::create_dir_all(paths.database_file.parent().unwrap()).unwrap();
    drop(Runtime::open(&paths.database_file).unwrap());
    let mut connection = rusqlite::Connection::open(&paths.database_file).unwrap();
    let transaction = connection.transaction().unwrap();
    let full_body = "é".repeat(200);
    for index in 0..251 {
        let timestamp = format!("2026-08-18T12:{:02}:{:02}Z", index / 60, index % 60);
        transaction
            .execute(
                r#"
                INSERT INTO dictation_sessions (
                    started_at, ended_at, duration_seconds, transcription_model,
                    raw_word_count, final_word_count, final_character_count
                ) VALUES (?1, ?1, 1, 'test-model', 1, 1, ?2)
                "#,
                params![timestamp, full_body.chars().count() as u64],
            )
            .unwrap();
        transaction
            .execute(
                r#"
                INSERT INTO transcript_history (
                    session_id, created_at, raw_transcript, final_text
                ) VALUES (?1, ?2, ?3, ?3)
                "#,
                params![transaction.last_insert_rowid(), timestamp, full_body],
            )
            .unwrap();
    }
    transaction.commit().unwrap();
    drop(connection);
    let runtime = Runtime::open(&paths.database_file).unwrap();
    let daemon = Daemon::new(
        runtime,
        Settings::default(),
        paths.clone(),
        InspectingRecorder {
            database: paths.database_file,
            started_after_checkpoint: false,
        },
        FixedTranscriber,
        SubmittedDelivery::default(),
    );

    let workspace = daemon.workspace_snapshot().unwrap();

    assert_eq!(workspace.history.len(), 20);
    assert_eq!(workspace.recent_history.len(), 30);
    assert_eq!(workspace.recent_history[..20], workspace.history);
    assert_eq!(workspace.history_total, 251);
    assert!(workspace.history_has_more);
    assert!(!workspace.history.iter().any(|entry| entry.id == 1));
    assert!(workspace.history.iter().all(|entry| {
        entry.preview_text.chars().count() == 161 && entry.preview_text.ends_with('…')
    }));
}

#[test]
fn daemon_restarts_an_expired_fuzzy_cursor_at_page_one() {
    let directory = tempdir().unwrap();
    let paths = app_paths(directory.path());
    std::fs::create_dir_all(paths.database_file.parent().unwrap()).unwrap();
    let mut runtime = Runtime::open(&paths.database_file).unwrap();
    runtime.ensure_history_search_index().unwrap();
    let mut connection = rusqlite::Connection::open(&paths.database_file).unwrap();
    for (index, text) in ["needle one", "needle two", "needle three"]
        .into_iter()
        .enumerate()
    {
        insert_search_history(&mut connection, index, text);
    }
    let daemon = Daemon::new(
        runtime,
        Settings::default(),
        paths.clone(),
        InspectingRecorder {
            database: paths.database_file.clone(),
            started_after_checkpoint: false,
        },
        FixedTranscriber,
        SubmittedDelivery::default(),
    );
    let first_page = daemon
        .history_page_snapshot(HistoryPageRequest {
            search: "nedle".into(),
            page_size: 1,
            after: None,
        })
        .unwrap();
    assert!(!first_page.cursor_restarted);
    let cursor = first_page.next_cursor.expect("first fuzzy page cursor");

    insert_search_history(&mut connection, 10, "nedle exact one");
    insert_search_history(&mut connection, 11, "nedle exact two");

    let restarted = daemon
        .history_page_snapshot(HistoryPageRequest {
            search: "nedle".into(),
            page_size: 1,
            after: Some(cursor),
        })
        .unwrap();
    assert!(restarted.cursor_restarted);
    assert_eq!(restarted.rows.len(), 1);
    assert_eq!(restarted.rows[0].preview_text, "nedle exact two");
}

fn insert_search_history(
    connection: &mut rusqlite::Connection,
    timestamp_offset: usize,
    final_text: &str,
) {
    let timestamp = format!("2026-08-18T13:{timestamp_offset:02}:00Z");
    let transaction = connection.transaction().unwrap();
    transaction
        .execute(
            r#"
            INSERT INTO dictation_sessions (
                started_at, ended_at, duration_seconds, transcription_model,
                raw_word_count, final_word_count, final_character_count
            ) VALUES (?1, ?1, 1, 'test-model', 2, 2, ?2)
            "#,
            params![timestamp, final_text.chars().count()],
        )
        .unwrap();
    transaction
        .execute(
            r#"
            INSERT INTO transcript_history (
                session_id, created_at, raw_transcript, final_text
            ) VALUES (?1, ?2, ?3, ?3)
            "#,
            params![transaction.last_insert_rowid(), timestamp, final_text],
        )
        .unwrap();
    transaction.commit().unwrap();
}

fn app_paths(root: &Path) -> AppPaths {
    AppPaths::from_roots(
        root.join("config"),
        root.join("data"),
        root.join("state"),
        root.join("cache"),
        root.join("runtime"),
    )
}

fn reject_capture_checkpoint(connection: &rusqlite::Connection) {
    connection
        .execute_batch(
            r#"
            CREATE TRIGGER reject_capture_checkpoint
            BEFORE UPDATE OF stage ON dictation_jobs
            WHEN NEW.stage = 'captured'
            BEGIN
                SELECT RAISE(FAIL, 'capture checkpoint unavailable');
            END;
            "#,
        )
        .unwrap();
}

mod notices {
    use std::sync::{Arc, Mutex, mpsc::Receiver};

    use agentdictate_app::{
        AppPaths, Daemon, Notice, OverlayUpdate, ProcessingTranscriber, RecordingController,
        overlay_update_recorder,
    };
    use agentdictate_core::{DictationNotice, Settings};
    use agentdictate_runtime::{
        Deliverer, ExternalError, ExternalFailureKind, RecordingJob, Runtime, Transcriber,
        Transcript,
    };
    use tempfile::{TempDir, tempdir};

    use super::{FixedTranscriber, PreservingRecorder, SubmittedDelivery, app_paths};

    struct LimitedTranscriber;
    impl Transcriber for LimitedTranscriber {
        fn transcribe(&mut self, _: &RecordingJob) -> Result<Transcript, ExternalError> {
            Err(ExternalError::with_kind(
                ExternalFailureKind::LimitReached,
                "ChatGPT transcription limit reached. Try again later.",
            ))
        }
    }
    impl ProcessingTranscriber for LimitedTranscriber {
        type Job = Self;

        fn for_job(&mut self, _: &RecordingJob) -> Self {
            Self
        }
    }

    struct Observed<R, T, D> {
        daemon: Daemon<R, T, D>,
        updates: Receiver<OverlayUpdate>,
        sent: Arc<Mutex<Vec<Notice>>>,
        _directory: TempDir,
    }

    impl<R, T, D> Observed<R, T, D>
    where
        R: RecordingController,
        T: ProcessingTranscriber,
        D: Deliverer + Send + 'static,
    {
        fn new(recorder: R, transcriber: T, deliverer: D) -> Self {
            let directory = tempdir().unwrap();
            let paths: AppPaths = app_paths(directory.path());
            std::fs::create_dir_all(paths.database_file.parent().unwrap()).unwrap();
            let runtime = Runtime::open(&paths.database_file).unwrap();
            let mut daemon = Daemon::new(
                runtime,
                Settings::default(),
                paths,
                recorder,
                transcriber,
                deliverer,
            );
            let (controller, updates) = overlay_update_recorder();
            daemon.set_overlay_controller(controller);
            let sent = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::clone(&sent);
            daemon.set_notice_sink(Arc::new(move |notice: &Notice| {
                sink.lock().unwrap().push(notice.clone());
            }));
            Self {
                daemon,
                updates,
                sent,
                _directory: directory,
            }
        }

        /// The notices published to the pill since the last call: every
        /// update forwarded until the recorder has been quiet for a while.
        fn pill_notices(&self) -> Vec<DictationNotice> {
            let quiet = std::time::Duration::from_millis(200);
            std::iter::from_fn(|| self.updates.recv_timeout(quiet).ok())
                .filter_map(|update| update.notice)
                .collect()
        }

        fn sent(&self) -> Vec<Notice> {
            std::mem::take(&mut *self.sent.lock().unwrap())
        }
    }

    #[test]
    fn a_failed_dictation_publishes_its_notice_and_a_paste_publishes_none() {
        let mut failing = Observed::new(
            PreservingRecorder::default(),
            LimitedTranscriber,
            SubmittedDelivery::default(),
        );
        failing.daemon.start_recording().unwrap();
        failing.daemon.stop_recording_and_wait().unwrap_err();
        assert_eq!(failing.pill_notices(), vec![DictationNotice::LimitReached]);
        let sent = failing.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].notice, DictationNotice::LimitReached);
        assert!(
            sent[0]
                .balloon
                .as_deref()
                .is_some_and(|text| text.contains("limit reached")),
            "{sent:?}"
        );

        let mut pasting = Observed::new(
            PreservingRecorder::default(),
            FixedTranscriber,
            SubmittedDelivery::default(),
        );
        pasting.daemon.start_recording().unwrap();
        pasting.daemon.stop_recording_and_wait().unwrap();
        assert!(pasting.pill_notices().is_empty());
        assert!(pasting.sent().is_empty());
    }

    #[test]
    fn retries_from_settings_and_escape_stay_quiet() {
        let mut observed = Observed::new(
            PreservingRecorder::default(),
            LimitedTranscriber,
            SubmittedDelivery::default(),
        );
        let failed = observed.daemon.start_recording().unwrap();
        observed.daemon.stop_recording_and_wait().unwrap_err();
        assert_eq!(observed.pill_notices().len(), 1);
        observed.sent();

        observed.daemon.retry_transcription(failed.id).unwrap_err();
        observed.daemon.start_recording().unwrap();
        observed.daemon.discard_recording().unwrap();
        observed.daemon.shutdown().unwrap();

        assert!(observed.pill_notices().is_empty());
        assert!(observed.sent().is_empty());
    }
}
