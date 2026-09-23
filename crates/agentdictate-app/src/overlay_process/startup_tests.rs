//! Exercise daemon startup with a channel in place of the native presenter.
use super::*;
use crate::{AppPaths, CapturedRecording, Daemon, RecordingController};
use agentdictate_core::{JobStage, Settings, WorkflowPhase};
use agentdictate_runtime::{
    Deliverer, DeliveryDisposition, ExternalError, Recorder, RecordingJob, Runtime, Transcriber,
    Transcript,
};

struct InspectingRecorder {
    updates: Receiver<OverlayCommand>,
    database: PathBuf,
    fail: bool,
    aborted: bool,
}

impl Recorder for InspectingRecorder {
    fn start(&mut self, job: &RecordingJob) -> Result<(), ExternalError> {
        let observer = Runtime::open_observer(&self.database).unwrap();
        assert_eq!(
            observer.job(job.id).unwrap().unwrap().stage,
            JobStage::Starting
        );
        let updates = self.updates.try_iter().collect::<Vec<_>>();
        let Some(OverlayCommand::Update(starting)) = updates.last() else {
            panic!("the waiting indicator must be queued before microphone setup");
        };
        assert_eq!(
            starting.workflow.phase,
            WorkflowPhase::Starting { job_id: job.id }
        );
        assert!(starting.active_recording.is_none());
        assert!(starting.presentation().state().is_visible());
        assert!(updates.iter().all(|command| !matches!(
            command,
            OverlayCommand::Update(update) if matches!(update.workflow.phase, WorkflowPhase::Recording { .. })
        )));
        if self.fail {
            Err(ExternalError::new("microphone unavailable"))
        } else {
            std::fs::write(&job.audio_path, b"RIFFfixture").unwrap();
            Ok(())
        }
    }

    fn abort_start(&mut self, _: &RecordingJob) -> Result<(), ExternalError> {
        self.aborted = true;
        Ok(())
    }
}

impl RecordingController for InspectingRecorder {
    fn finish(&mut self, _: &RecordingJob) -> Result<CapturedRecording, ExternalError> {
        Ok(CapturedRecording {
            duration_seconds: 1.,
        })
    }
}

struct Unused;
impl Transcriber for Unused {
    fn transcribe(&mut self, _: &RecordingJob) -> Result<Transcript, ExternalError> {
        panic!("startup must not transcribe");
    }
}
impl Deliverer for Unused {
    fn deliver(&mut self, _: &RecordingJob) -> Result<DeliveryDisposition, ExternalError> {
        panic!("startup must not paste");
    }
}

#[test]
fn waiting_indicator_precedes_audio_and_only_becomes_ready_after_its_checkpoint() {
    for (microphone_fails, checkpoint_fails) in [(false, false), (true, false), (false, true)] {
        let directory = tempfile::tempdir().unwrap();
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
        let connection = rusqlite::Connection::open(&paths.database_file).unwrap();
        if checkpoint_fails {
            connection
                .execute_batch(
                    "CREATE TRIGGER reject_recording BEFORE UPDATE OF stage ON dictation_jobs
                 WHEN NEW.stage = 'recording' BEGIN SELECT RAISE(FAIL, 'checkpoint failed'); END;",
                )
                .unwrap();
        }
        let (commands, updates) = channel();
        let controller = OverlayController {
            commands,
            health: Arc::default(),
        };
        let recorder = InspectingRecorder {
            updates,
            database: paths.database_file.clone(),
            fail: microphone_fails,
            aborted: false,
        };
        let mut daemon = Daemon::new(
            runtime,
            Settings::default(),
            paths,
            recorder,
            Unused,
            Unused,
        );
        daemon.set_overlay_controller(controller);
        let result = daemon.start_recording();
        let update = match daemon.recorder().updates.try_recv().unwrap() {
            OverlayCommand::Update(update) => update,
            _ => panic!("expected startup result"),
        };
        assert_eq!(daemon.recorder().aborted, checkpoint_fails);
        if microphone_fails || checkpoint_fails {
            assert!(result.is_err());
            assert!(matches!(
                update.workflow.phase,
                WorkflowPhase::NeedsAttention { .. }
            ));
            assert!(!update.presentation().state().is_visible());
            assert!(update.active_recording.is_none());
            assert_eq!(daemon.snapshot().recoverable_count, 1);
            daemon.recorder_mut().fail = false;
            connection
                .execute_batch("DROP TRIGGER IF EXISTS reject_recording")
                .unwrap();
            daemon
                .start_recording()
                .expect("a failed startup must allow retry");
        } else {
            let job = result.unwrap();
            assert_eq!(
                update.workflow.phase,
                WorkflowPhase::Recording { job_id: job.id }
            );
            let active = update.active_recording.unwrap();
            assert_eq!(active.audio_path, job.audio_path);
            assert!(active.started_at_unix_millis >= job.started_at.timestamp_millis());
        }
    }
}
