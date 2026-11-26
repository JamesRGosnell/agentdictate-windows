use std::fs;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use agentdictate_core::{
    AppSnapshot, HistoryPageCursor, HistoryPageRequest, HistoryPageSnapshot, HistorySnapshot,
    HotkeyReadiness, JobId, JobStage, ProcessingStage, RecoverySnapshot, ReplacementRule, Settings,
    UsageDaySnapshot, UsageSnapshot, UsageTotalsSnapshot, WorkflowError, WorkflowPhase,
    WorkflowSignal, WorkspaceSnapshot,
};
use agentdictate_runtime::{
    Deliverer, DeliveryStatus, ExternalError, HistoryCursor, HistoryEntry, HistoryIndexMaintenance,
    HistoryQuery, Recorder, RecordingJob, RecordingRequest, Runtime, RuntimeError, UsageAggregate,
    UsageMetric,
};
use chrono::Utc;
use thiserror::Error;

use crate::processing::{
    OverlayGate, ProcessingQueue, Shared, StoppedDictation, lock_deliverer, remove_completed_audio,
};
use crate::{
    ActiveRecordingUpdate, AppPaths, DictationEnd, Notice, OverlayController,
    ProcessingTranscriber, dictation_notice,
};

/// Receives the notice about each dictation that was not pasted normally,
/// for the tray notification. It must not block.
pub type NoticeSink = Arc<dyn Fn(&Notice) + Send + Sync>;

pub(crate) const OVERVIEW_RECENT_HISTORY_LIMIT: usize = 30;

/// How long shutdown waits for stopped dictations to be pasted. What is
/// still processing after that waits in Recovery at the next start.
const SHUTDOWN_PROCESSING_GRACE: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CapturedRecording {
    pub duration_seconds: f64,
}

/// Recorder lifecycle owned by the daemon. `Recorder::start` is called only
/// after the durable Starting checkpoint; `finish` must finalize the WAV before
/// the Captured checkpoint is written.
pub trait RecordingController: Recorder {
    fn finish(&mut self, job: &RecordingJob) -> Result<CapturedRecording, ExternalError>;
}

/// Publishes the durable Starting checkpoint before microphone initialization
/// blocks, allowing the presentation process to initialize in parallel.
struct StartingRecorder<'a, R> {
    inner: &'a mut R,
    shared: &'a Shared,
}

impl<R: Recorder> Recorder for StartingRecorder<'_, R> {
    fn start(&mut self, job: &RecordingJob) -> Result<(), ExternalError> {
        let mut state = self.shared.lock();
        state
            .workflow
            .apply(WorkflowSignal::StartRequested { job_id: job.id })
            .map_err(|error| ExternalError::new(error.to_string()))?;
        self.shared.changed(state, None);
        tracing::info!(job_id = %job.id, "recording startup requested");
        self.inner.start(job)
    }

    fn abort_start(&mut self, job: &RecordingJob) -> Result<(), ExternalError> {
        self.inner.abort_start(job)
    }
}

#[derive(Debug, Error)]
pub enum DaemonError {
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error(transparent)]
    Workflow(#[from] WorkflowError),
    #[error("recording operation failed: {0}")]
    Recording(#[from] ExternalError),
    #[error("recording storage could not be prepared: {0}")]
    Io(#[from] std::io::Error),
    #[error("a recording is already active")]
    AlreadyRecording,
    #[error("no recording is active")]
    NotRecording,
    #[error("a dictation is still being transcribed or pasted; try again once it is done")]
    StillProcessing,
}

/// The dictation daemon. Its methods run under the daemon lock and return
/// once their bounded part is done: a stop returns as soon as the capture
/// is durable, and the dictation is transcribed, cleaned and pasted on the
/// processing worker, in stop order, while the next one can start.
pub struct Daemon<R, T, D> {
    runtime: Runtime,
    settings: Settings,
    paths: AppPaths,
    recorder: R,
    transcriber: T,
    /// Shared with the processing worker, which holds it only to paste.
    deliverer: Arc<Mutex<D>>,
    /// The recording in progress; a stopped dictation is no longer active.
    active_job: Option<JobId>,
    shared: Arc<Shared>,
    processing: ProcessingQueue,
    shutdown_grace: Duration,
    history_maintenance: Option<HistoryIndexMaintenance>,
}

impl<R, T, D> Daemon<R, T, D>
where
    R: RecordingController,
    T: ProcessingTranscriber,
    D: Deliverer + Send + 'static,
{
    #[must_use]
    pub fn new(
        runtime: Runtime,
        settings: Settings,
        paths: AppPaths,
        recorder: R,
        transcriber: T,
        deliverer: D,
    ) -> Self {
        let recoverable_count = attention_recovery_count(&runtime).unwrap_or(0);
        let processing = ProcessingQueue::new(paths.database_file.clone());
        Self {
            runtime,
            settings,
            paths,
            recorder,
            transcriber,
            deliverer: Arc::new(Mutex::new(deliverer)),
            active_job: None,
            shared: Shared::new(recoverable_count),
            processing,
            shutdown_grace: SHUTDOWN_PROCESSING_GRACE,
            history_maintenance: None,
        }
    }

    /// Gives dictations being processed the same priority over deferred
    /// index maintenance as recordings, so it never holds the database's
    /// write lock while a dictation's transcript is saved.
    pub fn set_history_maintenance(&mut self, maintenance: HistoryIndexMaintenance) {
        self.history_maintenance = Some(maintenance);
    }

    pub fn start_recording(&mut self) -> Result<RecordingJob, DaemonError> {
        self.start_recording_in_mode(None)
    }

    pub fn start_recording_in_mode(
        &mut self,
        mode: Option<agentdictate_core::DictationMode>,
    ) -> Result<RecordingJob, DaemonError> {
        if self.active_job.is_some() {
            return Err(DaemonError::AlreadyRecording);
        }
        fs::create_dir_all(&self.paths.recordings)?;
        let now = Utc::now();
        let path = self.paths.recordings.join(format!(
            "dictation-{}-{}.wav",
            now.format("%Y%m%dT%H%M%S%.fZ"),
            JobId::new()
        ));
        let mut recording_settings = self.settings.clone();
        recording_settings.cleanup_enabled = false;
        if recording_settings.dictation_mode == agentdictate_core::DictationMode::Organize {
            recording_settings.dictation_mode = agentdictate_core::DictationMode::Dictate;
        }
        if let Some(mode) = mode {
            if mode == agentdictate_core::DictationMode::Organize {
                return Err(ExternalError::new(
                    "Organize has been removed; use Dictate or Literal",
                )
                .into());
            }
            recording_settings.dictation_mode = mode;
        }
        let job = match self.runtime.start_recording(
            RecordingRequest {
                options: Some(agentdictate_core::DictationOptions::from_settings(
                    &recording_settings,
                    self.runtime.replacement_rules()?,
                )),
                audio_path: path.clone(),
                started_at: now,
                transcription_provider: self.settings.transcription_provider,
                transcription_model: self.settings.active_transcription_model().to_owned(),
            },
            &mut StartingRecorder {
                inner: &mut self.recorder,
                shared: &self.shared,
            },
        ) {
            Ok(job) => job,
            Err(error) => {
                let starting = match self.shared.lock().workflow.snapshot().phase {
                    WorkflowPhase::Starting { job_id } => Some(job_id),
                    _ => None,
                };
                if let Some(job_id) = starting {
                    // Dismisses the waiting indicator even if the database can
                    // no longer be queried after a failed checkpoint.
                    let message = external_message(&error);
                    self.settle_announcing(
                        job_id,
                        WorkflowSignal::Interrupted {
                            job_id,
                            at: JobStage::Interrupted,
                        },
                        dictation_notice(DictationEnd::RecorderDidNotStart { message: &message }),
                    );
                }
                return Err(error.into());
            }
        };
        // The recorder is running, so the session is tracked before anything
        // else can fail; Stop and Escape must always be able to end it.
        self.active_job = Some(job.id);
        let started = {
            let mut state = self.shared.lock();
            state.active_recording = Some(ActiveRecordingUpdate {
                audio_path: job.audio_path.clone(),
                // Match the previous overlay: elapsed time starts only after
                // the recorder has produced its first durable audio frame.
                started_at_unix_millis: Utc::now().timestamp_millis(),
            });
            state
                .workflow
                .apply(WorkflowSignal::FirstAudioFrameWritten { job_id: job.id })
        };
        started?;
        self.transcriber.begin_recording(&job);
        // Bookkeeping only: a failed recount must not report a running
        // recording as a failed start.
        let entries = self.runtime.recovery_entries();
        let mut state = self.shared.lock();
        state.recount(job.id, entries, false);
        self.shared.changed(state, None);
        tracing::info!(job_id = %job.id, audio_path = %job.audio_path.display(), "recording ready");
        Ok(job)
    }

    /// Stops the recording and returns its job at the durable Captured
    /// checkpoint. Transcription, cleanup and the paste run on the
    /// processing worker afterwards, after every dictation stopped before
    /// this one; the paste may only land in the window focused now.
    pub fn stop_recording(&mut self) -> Result<RecordingJob, DaemonError> {
        let stopped_at = Instant::now();
        let id = self.active_job.ok_or(DaemonError::NotRecording)?;
        // Every stop (hotkey, tray, CLI) arrives here; the paste may only
        // land in the window the user was in when they stopped. This waits
        // only if an earlier dictation is pasting at this very moment.
        let stopped_in = lock_deliverer(&self.deliverer).focused_window();
        let job = self.runtime.job(id)?.ok_or(RuntimeError::JobNotFound(id))?;
        {
            let mut state = self.shared.lock();
            state.workflow.apply(WorkflowSignal::StopRequested)?;
            self.shared.changed(state, None);
        }
        tracing::info!(job_id = %id, "recording stop requested");
        let capture = match self.recorder.finish(&job) {
            Ok(capture) => capture,
            Err(error) => {
                self.transcriber.cancel_recording(id);
                tracing::error!(job_id = %id, %error, "recording finalization failed");
                let persisted = self.runtime.interrupt_job(
                    id,
                    JobStage::Recording,
                    format!("recording could not be finalized: {error}"),
                );
                self.settle_announcing(
                    id,
                    WorkflowSignal::Interrupted {
                        job_id: id,
                        at: JobStage::Interrupted,
                    },
                    dictation_notice(DictationEnd::RecordingInterrupted),
                );
                persisted?;
                return Err(error.into());
            }
        };
        let audio_bytes = std::fs::metadata(&job.audio_path)
            .ok()
            .map(|metadata| metadata.len());
        tracing::info!(
            job_id = %id,
            duration_seconds = capture.duration_seconds,
            ?audio_bytes,
            "recording finalized"
        );
        let captured = match self.runtime.capture_recording(id, capture.duration_seconds) {
            Ok(captured) => captured,
            Err(error) => {
                tracing::error!(
                    job_id = %id,
                    %error,
                    "recording finalized but its capture checkpoint failed"
                );
                self.recover_after_capture_checkpoint_failure(
                    id,
                    &error,
                    dictation_notice(DictationEnd::RecordingInterrupted),
                );
                return Err(error.into());
            }
        };
        // The capture is durable: from here the dictation belongs to the
        // processing worker, and the next one may start.
        let transcriber = self.transcriber.for_job(&captured);
        self.active_job = None;
        {
            let mut state = self.shared.lock();
            if let Err(workflow_error) = state
                .workflow
                .apply(WorkflowSignal::CaptureFinalized { job_id: id })
            {
                tracing::warn!(job_id = %id, %workflow_error, "the workflow missed a capture");
            }
            state.active_recording = None;
            state.in_flight.push_back(id);
            self.shared.changed(state, None);
        }
        self.processing.push(
            StoppedDictation {
                job: captured.clone(),
                transcriber,
                deliverer: Arc::clone(&self.deliverer),
                stopped_in,
                settings: self.settings.clone(),
                shared: Arc::clone(&self.shared),
                stopped_at,
                // The recording still holds priority, so this returns at once.
                _priority: self
                    .history_maintenance
                    .as_ref()
                    .map(HistoryIndexMaintenance::prioritize_recording),
            }
            .into_task(),
        );
        Ok(captured)
    }

    /// Stops the recording and waits for the worker to finish it, for
    /// tests that check how a dictation ends.
    #[doc(hidden)]
    pub fn stop_recording_and_wait(&mut self) -> Result<RecordingJob, DaemonError> {
        let stopped = self.stop_recording()?;
        self.processed(stopped.id, Duration::from_secs(30))
            .unwrap_or_else(|| {
                Err(ExternalError::new("the dictation was not processed in time").into())
            })
    }

    /// Waits up to `timeout` for the stopped dictation `id` to finish and
    /// returns how it ended. Only the last few dictations are remembered.
    #[doc(hidden)]
    pub fn processed(
        &self,
        id: JobId,
        timeout: Duration,
    ) -> Option<Result<RecordingJob, DaemonError>> {
        let state = self.shared.lock();
        let (mut state, _) = self.shared.wait_while(state, timeout, |state| {
            state.is_in_flight(id) || !state.has_finished(id)
        });
        state.take_finished(id)
    }

    /// Waits up to `timeout` until no stopped dictation is left to process.
    /// Returns whether none is.
    #[doc(hidden)]
    pub fn wait_for_processing(&self, timeout: Duration) -> bool {
        let state = self.shared.lock();
        let (state, _) = self
            .shared
            .wait_while(state, timeout, |state| !state.in_flight.is_empty());
        state.in_flight.is_empty()
    }

    /// Sets how long shutdown waits for stopped dictations.
    #[doc(hidden)]
    pub fn set_shutdown_grace(&mut self, grace: Duration) {
        self.shutdown_grace = grace;
    }

    /// Discards the active recording because the user explicitly pressed
    /// Escape. A recording longer than a few seconds waits in Recovery as
    /// `Canceled` for a day; a shorter one is deleted with its audio unless
    /// "Preserve temporary audio" is on. Either way the workflow returns to
    /// Ready without asking for attention. Shutdown and platform failures
    /// must use the separate recovery preservation path below. A stopped
    /// dictation still being processed is never discarded: Escape only
    /// ends recordings.
    pub fn discard_recording(&mut self) -> Result<RecordingJob, DaemonError> {
        let id = self.active_job.ok_or(DaemonError::NotRecording)?;
        self.transcriber.cancel_recording(id);
        tracing::info!(job_id = %id, "dictation discard requested");
        let job = self.runtime.job(id)?.ok_or(RuntimeError::JobNotFound(id))?;
        let capture = match self.recorder.finish(&job) {
            Ok(capture) => capture,
            Err(error) => {
                let interrupted = self.runtime.interrupt_job(
                    id,
                    JobStage::Recording,
                    format!("recording could not be finalized while discarding: {error}"),
                );
                self.settle(
                    id,
                    WorkflowSignal::Interrupted {
                        job_id: id,
                        at: JobStage::Interrupted,
                    },
                );
                return Ok(interrupted?);
            }
        };
        if let Err(error) = self.runtime.capture_recording(id, capture.duration_seconds) {
            tracing::error!(
                job_id = %id,
                %error,
                "discarded recording finalized but its capture checkpoint failed"
            );
            self.recover_after_capture_checkpoint_failure(id, &error, None);
            return Err(error.into());
        }
        let discarded = match self
            .runtime
            .discard_recording(id, self.settings.preserve_temp_audio)
        {
            Ok(discarded) => discarded,
            Err(error) => {
                self.settle(
                    id,
                    WorkflowSignal::Interrupted {
                        job_id: id,
                        at: JobStage::Captured,
                    },
                );
                return Err(error.into());
            }
        };
        self.settle(id, WorkflowSignal::DiscardCommitted { job_id: id });
        tracing::info!(
            job_id = %id,
            duration_seconds = capture.duration_seconds,
            kept_in_recovery = discarded.stage == JobStage::Canceled,
            "dictation discarded"
        );
        Ok(discarded)
    }

    /// Handles a kernel-observed recorder exit. A stale notification from a
    /// normal stop is ignored; an active unexpected exit preserves whatever
    /// audio was finalized for explicit recovery instead of guessing that the
    /// dictation was complete.
    pub fn recorder_exited(&mut self, id: JobId) -> Result<Option<RecordingJob>, DaemonError> {
        if self.active_job != Some(id) {
            return Ok(None);
        }
        tracing::warn!(job_id = %id, "recorder exited without an explicit stop");
        self.preserve_active_recording(
            "recorder exited unexpectedly before the dictation completed; audio was preserved",
            dictation_notice(DictationEnd::RecordingInterrupted),
        )
        .map(Some)
    }

    /// Finalizes active audio without transcribing or deleting it, so process
    /// shutdown can never discard an in-progress dictation. Stopped
    /// dictations get the shutdown grace to be pasted; any not yet started
    /// then waits in Recovery, and one still processing is reconciled at the
    /// next start like any interrupted dictation. None is pasted twice. A
    /// shutdown that could not preserve the recording keeps processing, as
    /// the daemon then keeps taking work.
    pub fn shutdown(&mut self) -> Result<(), DaemonError> {
        if let Some(id) = self.active_job {
            self.transcriber.cancel_recording(id);
            // A shutdown is announced by the app closing, not by a notice.
            self.preserve_active_recording(
                "AgentDictate shut down before this dictation completed; audio was preserved",
                None,
            )?;
        }
        self.stop_processing();
        Ok(())
    }

    fn stop_processing(&mut self) {
        let state = self.shared.lock();
        let (mut state, timed_out) = self.shared.wait_while(state, self.shutdown_grace, |state| {
            !state.in_flight.is_empty()
        });
        state.quitting = true;
        let running = state.running;
        let waiting: Vec<JobId> = state
            .in_flight
            .iter()
            .copied()
            .filter(|id| Some(*id) != running)
            .collect();
        state.in_flight.retain(|id| Some(*id) == running);
        drop(state);
        if timed_out {
            tracing::warn!(
                running = ?running,
                waiting = waiting.len(),
                "quitting before every stopped dictation was pasted; the rest waits in Recovery"
            );
        }
        for id in waiting {
            if let Err(error) = self.runtime.interrupt_job(
                id,
                JobStage::Captured,
                "AgentDictate shut down before this dictation was transcribed; audio was preserved",
            ) {
                tracing::error!(job_id = %id, %error, "could not mark a waiting dictation for Recovery");
            }
        }
        let entries = self.runtime.recovery_entries();
        let mut state = self.shared.lock();
        let id = running.unwrap_or_default();
        state.recount(id, entries, false);
        self.shared.changed(state, None);
    }

    pub fn retry_transcription(&mut self, id: JobId) -> Result<RecordingJob, DaemonError> {
        self.ensure_idle()?;
        let mut gate = self.shared.lock().overlay.clone();
        let result = self.runtime.retry_transcription(
            id,
            &mut self.transcriber,
            &mut gate,
            &mut *lock_deliverer(&self.deliverer),
        )?;
        self.finish_retry(id, &result, "could not complete retried transcription");
        Ok(result)
    }

    pub fn retry_delivery(&mut self, id: JobId) -> Result<RecordingJob, DaemonError> {
        self.ensure_idle()?;
        let mut gate = self.shared.lock().overlay.clone();
        let result =
            self.runtime
                .retry_delivery(id, &mut gate, &mut *lock_deliverer(&self.deliverer))?;
        self.finish_retry(id, &result, "could not complete retried delivery");
        Ok(result)
    }

    /// Recovery retries run here, under the daemon lock, so they wait until
    /// no dictation is recording or being processed: a retry then never
    /// races the worker for the same job, the paste order or the pill.
    fn ensure_idle(&self) -> Result<(), DaemonError> {
        if self.active_job.is_some() {
            return Err(DaemonError::AlreadyRecording);
        }
        if !self.shared.lock().in_flight.is_empty() {
            return Err(DaemonError::StillProcessing);
        }
        Ok(())
    }

    fn finish_retry(&mut self, id: JobId, result: &RecordingJob, failure: &'static str) {
        if result.stage == JobStage::Delivered
            && let Err(error) = self.runtime.complete_delivered(id, &self.settings)
        {
            tracing::error!(job_id = %id, %error, "{failure}");
        }
        if matches!(result.stage, JobStage::Delivered | JobStage::NoSpeech) {
            remove_completed_audio(&self.settings, result);
        }
        let entries = self.runtime.recovery_entries();
        let mut state = self.shared.lock();
        state.reset_workflow();
        if result.stage != JobStage::NoSpeech {
            state.last_transcript = Some(result.final_text.clone());
        }
        state.recount(id, entries, false);
        self.shared.changed(state, None);
    }

    pub fn delete_recovery(&mut self, id: JobId) -> Result<RecordingJob, DaemonError> {
        if self.shared.lock().is_in_flight(id) {
            return Err(DaemonError::StillProcessing);
        }
        let result = self.runtime.delete_recovery(id)?;
        let entries = self.runtime.recovery_entries();
        let mut state = self.shared.lock();
        // Deleting another item from Settings while dictating must not
        // desync the live recording's or a processed dictation's workflow,
        // or Stop and Escape fail; only a shown failure is cleared.
        if matches!(
            state.workflow.snapshot().phase,
            WorkflowPhase::NeedsAttention { .. }
        ) {
            state.reset_workflow();
        }
        state.recount(id, entries, false);
        self.shared.changed(state, None);
        Ok(result)
    }

    pub fn workspace_snapshot(&self) -> Result<WorkspaceSnapshot, RuntimeError> {
        let in_flight = self.shared.lock().in_flight.clone();
        let recoveries = self
            .runtime
            .recovery_entries()?
            .into_iter()
            // A dictation still being processed is not recoverable yet.
            .filter(|entry| !in_flight.contains(&entry.job_id))
            .map(|entry| RecoverySnapshot {
                job_id: entry.job_id,
                stage: entry.stage,
                updated_at: entry.updated_at,
                duration_seconds: entry.duration_seconds,
                raw_transcript: entry.raw_transcript,
                final_text: entry.final_text,
                error_message: entry.error_message,
                audio_present: entry.audio_present,
                delivery_ambiguous: entry.delivery_status == DeliveryStatus::Ambiguous,
            })
            .collect();
        let history_page = self.history_page_snapshot(HistoryPageRequest::default())?;
        let recent_history = self
            .history_page_snapshot(HistoryPageRequest {
                search: String::new(),
                page_size: OVERVIEW_RECENT_HISTORY_LIMIT,
                after: None,
            })?
            .rows;
        let replacements = self.runtime.replacement_rules()?;
        let usage = self.usage_snapshot()?;
        Ok(WorkspaceSnapshot {
            overlay_unavailable: matches!(&self.shared.lock().overlay, OverlayGate::Live(controller) if controller.is_unavailable()),
            recoveries,
            recent_history,
            history: history_page.rows,
            history_total: history_page.total_matches,
            history_has_more: history_page.next_cursor.is_some(),
            history_next_cursor: history_page.next_cursor,
            history_search: history_page.search,
            replacements,
            usage,
            model_catalog: Default::default(),
        })
    }

    pub fn history_page_snapshot(
        &self,
        request: HistoryPageRequest,
    ) -> Result<HistoryPageSnapshot, RuntimeError> {
        let history_query = HistoryQuery {
            search: request.search.clone(),
            limit: request.page_size,
            after: request
                .after
                .as_ref()
                .map(|cursor| HistoryCursor::from_opaque(cursor.as_str())),
            ..HistoryQuery::default()
        };
        let (page, cursor_restarted) = match self.runtime.history_page(history_query) {
            Ok(page) => (page, false),
            Err(RuntimeError::InvalidHistoryCursor(_)) if request.after.is_some() => (
                self.runtime.history_page(HistoryQuery {
                    search: request.search.clone(),
                    limit: request.page_size,
                    ..HistoryQuery::default()
                })?,
                true,
            ),
            Err(error) => return Err(error),
        };
        Ok(HistoryPageSnapshot {
            search: request.search,
            total_matches: page.total_matches,
            cursor_restarted,
            next_cursor: page
                .next_cursor
                .map(|cursor| HistoryPageCursor::new(cursor.into_opaque())),
            rows: page
                .matches
                .into_iter()
                .map(|matched| HistorySnapshot {
                    id: matched.entry.id,
                    created_at: matched.entry.created_at,
                    preview_text: matched.preview,
                    word_count: matched.entry.final_word_count,
                    duration_seconds: matched.entry.duration_seconds,
                })
                .collect(),
        })
    }

    pub fn create_replacement(
        &mut self,
        rule: ReplacementRule,
    ) -> Result<ReplacementRule, RuntimeError> {
        self.runtime.create_replacement(rule)
    }

    pub fn update_replacement(
        &mut self,
        rule: ReplacementRule,
    ) -> Result<ReplacementRule, RuntimeError> {
        self.runtime.update_replacement(rule)
    }

    pub fn delete_replacement(&mut self, id: i64) -> Result<bool, RuntimeError> {
        self.runtime.delete_replacement(id)
    }

    pub fn delete_history(&mut self, id: i64) -> Result<bool, RuntimeError> {
        self.runtime.delete_history(id)
    }

    pub fn clear_history(&mut self) -> Result<(), RuntimeError> {
        self.runtime.clear_history()
    }

    pub fn history(&self, id: i64) -> Result<Option<HistoryEntry>, RuntimeError> {
        self.runtime.history(id)
    }

    pub fn recovery_transcript(&self, id: JobId) -> Result<Option<String>, RuntimeError> {
        self.runtime.recovery_transcript(id)
    }

    #[must_use]
    pub fn snapshot(&self) -> AppSnapshot {
        let state = self.shared.lock();
        AppSnapshot {
            sequence: state.sequence,
            workflow: state.workflow.snapshot(),
            hotkey: state.hotkey.clone(),
            recoverable_count: state.recoverable_count,
            last_transcript: state.last_transcript.clone(),
        }
    }

    pub fn set_hotkey_readiness(&mut self, readiness: HotkeyReadiness) {
        let mut state = self.shared.lock();
        state.hotkey = readiness;
        self.shared.changed(state, None);
    }

    pub fn set_overlay_controller(&mut self, controller: OverlayController) {
        let mut state = self.shared.lock();
        state.overlay = OverlayGate::Live(controller);
        state.publish(None);
    }

    /// Shows the cleaning stage while a dictation's cleanup model runs, for
    /// the transcription pipeline to call. Only the dictation the workflow
    /// follows moves it.
    pub fn cleanup_started_observer(&self) -> impl Fn(JobId) + Send + Sync + 'static {
        let shared = Arc::clone(&self.shared);
        move |job_id| {
            let mut state = shared.lock();
            if state.workflow.snapshot().phase
                == (WorkflowPhase::Processing {
                    job_id,
                    stage: ProcessingStage::Transcribing,
                })
                && state
                    .workflow
                    .apply(WorkflowSignal::TranscriptStoredForCleanup { job_id })
                    .is_ok()
            {
                shared.changed(state, None);
            }
        }
    }

    #[must_use]
    pub const fn recorder(&self) -> &R {
        &self.recorder
    }

    /// The deliverer, locked. The processing worker holds it while it
    /// pastes, so this can wait for one paste.
    pub fn deliverer(&self) -> MutexGuard<'_, D> {
        lock_deliverer(&self.deliverer)
    }

    #[must_use]
    pub const fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn update_settings(&mut self, settings: Settings) {
        self.settings = settings;
        let state = self.shared.lock();
        self.shared.changed(state, None);
    }

    pub fn sync_pricing(&mut self, settings: &Settings) -> Result<(), RuntimeError> {
        self.runtime.sync_pricing(settings)
    }

    pub const fn transcriber_mut(&mut self) -> &mut T {
        &mut self.transcriber
    }

    pub const fn recorder_mut(&mut self) -> &mut R {
        &mut self.recorder
    }

    /// The deliverer, locked, as `deliverer`.
    pub fn deliverer_mut(&mut self) -> MutexGuard<'_, D> {
        lock_deliverer(&self.deliverer)
    }

    /// Shows how dictations that are not pasted normally end as tray
    /// notifications.
    pub fn set_notice_sink(&mut self, sink: NoticeSink) {
        self.shared.lock().notice_sink = Some(sink);
    }

    fn recover_after_capture_checkpoint_failure(
        &mut self,
        id: JobId,
        primary: &RuntimeError,
        notice: Option<Notice>,
    ) {
        self.transcriber.cancel_recording(id);
        if let Err(recovery_error) = self.runtime.interrupt_job(
            id,
            JobStage::Recording,
            format!("recording was finalized but its capture checkpoint failed: {primary}"),
        ) {
            tracing::error!(
                job_id = %id,
                %recovery_error,
                "could not persist capture-checkpoint recovery state"
            );
        }

        self.settle_announcing(
            id,
            WorkflowSignal::Interrupted {
                job_id: id,
                at: JobStage::Interrupted,
            },
            notice,
        );
    }

    /// Ends the active session. The session is cleared before anything can
    /// fail, an out-of-order workflow is rebuilt, and the Recovery recount is
    /// best-effort, so no bookkeeping error can leave a stale active job.
    fn settle(&mut self, id: JobId, end: WorkflowSignal) {
        self.settle_announcing(id, end, None);
    }

    /// `settle`, then announces `notice` about the dictation that ended.
    /// Only dictations the user just made pass one: Recovery retries,
    /// Escape and shutdown stay quiet.
    fn settle_announcing(&mut self, id: JobId, end: WorkflowSignal, notice: Option<Notice>) {
        self.active_job = None;
        let needs_attention = matches!(end, WorkflowSignal::Interrupted { .. });
        let entries = self.runtime.recovery_entries();
        let mut state = self.shared.lock();
        state.active_recording = None;
        state.end(id, end);
        state.recount(id, entries, needs_attention);
        self.shared.changed(state, notice.as_ref());
    }

    fn preserve_active_recording(
        &mut self,
        reason: &'static str,
        notice: Option<Notice>,
    ) -> Result<RecordingJob, DaemonError> {
        let id = self.active_job.ok_or(DaemonError::NotRecording)?;
        self.transcriber.cancel_recording(id);
        let job = self.runtime.job(id)?.ok_or(RuntimeError::JobNotFound(id))?;
        let capture = match self.recorder.finish(&job) {
            Ok(capture) => capture,
            Err(error) => {
                let interrupted = self.runtime.interrupt_job(
                    id,
                    JobStage::Recording,
                    format!("{reason}; recording could not be finalized: {error}"),
                );
                self.settle_announcing(
                    id,
                    WorkflowSignal::Interrupted {
                        job_id: id,
                        at: JobStage::Interrupted,
                    },
                    notice,
                );
                return Ok(interrupted?);
            }
        };
        if let Err(error) = self.runtime.capture_recording(id, capture.duration_seconds) {
            tracing::error!(
                job_id = %id,
                %error,
                "preserved recording finalized but its capture checkpoint failed"
            );
            self.recover_after_capture_checkpoint_failure(id, &error, notice);
            return Err(error.into());
        }
        let interrupted = self
            .runtime
            .interrupt_job(id, JobStage::Captured, reason.to_owned());
        self.settle_announcing(
            id,
            WorkflowSignal::Interrupted {
                job_id: id,
                at: JobStage::Interrupted,
            },
            notice,
        );
        Ok(interrupted?)
    }

    fn usage_snapshot(&self) -> Result<UsageSnapshot, RuntimeError> {
        let summary = self.runtime.usage_summary()?;
        let sessions = self.runtime.usage_series(30, UsageMetric::Sessions)?;
        let words = self.runtime.usage_series(30, UsageMetric::Words)?;
        let audio = self.runtime.usage_series(30, UsageMetric::AudioMinutes)?;
        let costs = self.runtime.usage_series(30, UsageMetric::EstimatedCost)?;
        let activity = sessions
            .into_iter()
            .zip(words)
            .zip(audio)
            .zip(costs)
            .map(|(((sessions, words), audio), cost)| UsageDaySnapshot {
                date: sessions.date,
                totals: UsageTotalsSnapshot {
                    dictations: sessions.value.round().max(0.0) as u64,
                    words: words.value.round().max(0.0) as u64,
                    audio_seconds: (audio.value * 60.0).max(0.0),
                    estimated_cost: cost.value.max(0.0),
                },
            })
            .collect::<Vec<_>>();
        let last_30_days = sum_usage(activity.iter().map(|day| day.totals));
        let last_7_days = sum_usage(activity.iter().rev().take(7).map(|day| day.totals));
        let weekly_activity = self
            .runtime
            .usage_weekly_series()?
            .into_iter()
            .map(|week| UsageDaySnapshot {
                date: week.week_start,
                totals: UsageTotalsSnapshot {
                    dictations: week.total_sessions,
                    words: week.total_words,
                    audio_seconds: week.total_audio_seconds,
                    estimated_cost: week.estimated_total_cost,
                },
            })
            .collect();
        Ok(UsageSnapshot {
            last_7_days,
            last_30_days,
            all_time: usage_totals(summary.all_time),
            activity,
            weekly_activity,
        })
    }
}

/// The overlay presenter ends once its controller is gone; a worker still
/// finishing a dictation must not keep it alive past the daemon.
impl<R, T, D> Drop for Daemon<R, T, D> {
    fn drop(&mut self) {
        self.shared.lock().overlay =
            OverlayGate::Headless(agentdictate_runtime::HeadlessDeliveryGate);
    }
}

/// The external failure's own message, without the runtime's prefix.
fn external_message(error: &RuntimeError) -> String {
    match error {
        RuntimeError::External(error) => error.to_string(),
        error => error.to_string(),
    }
}

/// Recovery items that need the user when no dictation is in flight. An
/// Esc-cancelled recording is kept only in case the cancel was a mistake,
/// so it never demands attention.
fn attention_recovery_count(runtime: &Runtime) -> Result<usize, RuntimeError> {
    Ok(runtime
        .recovery_entries()?
        .iter()
        .filter(|entry| entry.stage != JobStage::Canceled)
        .count())
}

fn usage_totals(aggregate: UsageAggregate) -> UsageTotalsSnapshot {
    UsageTotalsSnapshot {
        dictations: aggregate.total_sessions,
        words: aggregate.total_words,
        audio_seconds: aggregate.total_audio_seconds,
        estimated_cost: aggregate.estimated_total_cost,
    }
}

fn sum_usage(values: impl Iterator<Item = UsageTotalsSnapshot>) -> UsageTotalsSnapshot {
    values.fold(UsageTotalsSnapshot::default(), |mut total, value| {
        total.dictations += value.dictations;
        total.words += value.words;
        total.audio_seconds += value.audio_seconds;
        total.estimated_cost += value.estimated_cost;
        total
    })
}
