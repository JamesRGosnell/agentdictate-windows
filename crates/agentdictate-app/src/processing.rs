//! Transcribes, cleans and pastes stopped dictations on one processing
//! worker, so the daemon lock is free while a dictation is processed.
//!
//! Lock rules:
//! 1. The daemon lock (the IPC handler's mutex around `AgentProcess`) is
//!    never taken here. A stop releases it once the capture is checkpointed;
//!    the worker then transcribes, cleans and pastes on its own database
//!    connection, with the dictation's own transcriber.
//! 2. `Shared` holds what snapshots and the pill show. Every holder keeps
//!    its lock for bounded work only: no network, no ffmpeg, no paste, no
//!    database write.
//! 3. The deliverer's own mutex is held only for one paste or copy. Nothing
//!    that holds it takes the `Shared` lock.
//! 4. One worker runs the dictations in the order they were stopped, so a
//!    later short dictation never pastes before an earlier long one.

use std::collections::VecDeque;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::mpsc::{SendError, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use agentdictate_core::{
    HotkeyReadiness, JobId, JobStage, ProcessingStage, Settings, Workflow, WorkflowPhase,
    WorkflowSignal,
};
use agentdictate_runtime::{
    Deliverer, DeliveryDisposition, DeliveryGate, DeliveryGateError, ExternalError, FocusedWindow,
    HeadlessDeliveryGate, RecordingJob, RecordingPriorityGuard, RecoveryEntry, Runtime,
    RuntimeError, Transcriber,
};

use crate::overlay_process::OVERLAY_CANNOT_TAKE_FOCUS;
use crate::{
    ActiveRecordingUpdate, DaemonError, DictationEnd, Notice, NoticeSink, OverlayController,
    OverlayUpdate, dictation_notice,
};

/// A transcriber the daemon can hand one stopped dictation to. The
/// dictation is then transcribed on the processing worker while the daemon
/// keeps serving commands and records the next one.
pub trait ProcessingTranscriber: Transcriber {
    type Job: Transcriber + Send + 'static;

    /// A transcriber of its own for `job`, taking over what was prepared
    /// while it recorded, such as a live transcription stream. It runs
    /// under the daemon lock, so it must not block.
    fn for_job(&mut self, job: &RecordingJob) -> Self::Job;
}

/// How many finished dictations `Shared` remembers for `Daemon::processed`.
const FINISHED_KEPT: usize = 16;

#[derive(Clone)]
pub(crate) enum OverlayGate {
    Headless(HeadlessDeliveryGate),
    Live(OverlayController),
}

impl DeliveryGate for OverlayGate {
    fn confirm_ready(&mut self) -> Result<(), DeliveryGateError> {
        match self {
            Self::Headless(gate) => gate.confirm_ready(),
            Self::Live(gate) => gate.confirm_ready(),
        }
    }
}

/// What snapshots, the pill and the tray show, shared by the daemon and the
/// processing worker.
pub(crate) struct Shared {
    state: Mutex<Presentation>,
    /// Notified after every change, for waits on processing.
    changed: Condvar,
}

pub(crate) struct Presentation {
    /// Follows the newest dictation: the recording when there is one,
    /// otherwise the last dictation stopped. Older dictations finish
    /// without moving it.
    pub(crate) workflow: Workflow,
    pub(crate) active_recording: Option<ActiveRecordingUpdate>,
    pub(crate) sequence: u64,
    pub(crate) recoverable_count: usize,
    pub(crate) last_transcript: Option<String>,
    pub(crate) hotkey: HotkeyReadiness,
    pub(crate) overlay: OverlayGate,
    pub(crate) notice_sink: Option<NoticeSink>,
    /// Stopped dictations that have not finished, oldest first. They are
    /// not offered in Recovery while they are processed.
    pub(crate) in_flight: VecDeque<JobId>,
    /// The dictation the worker is processing now.
    pub(crate) running: Option<JobId>,
    /// Set once shutdown stops waiting: no further dictation starts or
    /// pastes, and what is left waits in Recovery.
    pub(crate) quitting: bool,
    finished: VecDeque<(JobId, Result<RecordingJob, DaemonError>)>,
}

impl Shared {
    pub(crate) fn new(recoverable_count: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(Presentation {
                workflow: Workflow::new(),
                active_recording: None,
                sequence: 0,
                recoverable_count,
                last_transcript: None,
                hotkey: HotkeyReadiness::Starting,
                overlay: OverlayGate::Headless(HeadlessDeliveryGate),
                notice_sink: None,
                in_flight: VecDeque::new(),
                running: None,
                quitting: false,
                finished: VecDeque::new(),
            }),
            changed: Condvar::new(),
        })
    }

    /// A panic while the lock was held leaves plain bookkeeping behind, so
    /// a poisoned lock is still used.
    pub(crate) fn lock(&self) -> MutexGuard<'_, Presentation> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Publishes a change made through `state`, with `notice` about a
    /// dictation that just ended, and wakes every waiter.
    pub(crate) fn changed(&self, mut state: MutexGuard<'_, Presentation>, notice: Option<&Notice>) {
        state.sequence += 1;
        state.publish(notice);
        drop(state);
        self.changed.notify_all();
    }

    /// Waits while `condition` holds, for at most `timeout`. Returns the
    /// guard and whether the wait timed out.
    pub(crate) fn wait_while<'a>(
        &self,
        state: MutexGuard<'a, Presentation>,
        timeout: Duration,
        condition: impl FnMut(&mut Presentation) -> bool,
    ) -> (MutexGuard<'a, Presentation>, bool) {
        let (state, result) = self
            .changed
            .wait_timeout_while(state, timeout, condition)
            .unwrap_or_else(PoisonError::into_inner);
        (state, result.timed_out())
    }
}

impl Presentation {
    pub(crate) fn overlay_update(&self) -> OverlayUpdate {
        OverlayUpdate {
            workflow: self.workflow.snapshot(),
            active_recording: self.active_recording.clone(),
            notice: None,
        }
    }

    /// Publishes the workflow and, when a dictation just ended without a
    /// normal paste, the notice about it: on the pill and to the sink.
    pub(crate) fn publish(&self, notice: Option<&Notice>) {
        if let OverlayGate::Live(overlay) = &self.overlay {
            overlay.update(OverlayUpdate {
                notice: notice.map(|notice| notice.notice),
                ..self.overlay_update()
            });
        }
        if let Some(notice) = notice {
            tracing::info!(notice = %notice.notice.pill_text(), "dictation ended without a paste");
            if let Some(sink) = &self.notice_sink {
                sink(notice);
            }
        }
    }

    /// True when the workflow follows `id`.
    pub(crate) fn follows(&self, id: JobId) -> bool {
        self.workflow.snapshot().phase.job_id() == Some(id)
    }

    pub(crate) fn is_in_flight(&self, id: JobId) -> bool {
        self.in_flight.contains(&id)
    }

    /// True while a recording other than `id` starts, runs or stops.
    fn records_another(&self, id: JobId) -> bool {
        matches!(
            self.workflow.snapshot().phase,
            WorkflowPhase::Starting { job_id }
                | WorkflowPhase::Recording { job_id }
                | WorkflowPhase::Stopping { job_id }
                if job_id != id
        )
    }

    /// Ends the workflow's dictation `id` with `end`. An out-of-order
    /// workflow is rebuilt, and when it is left ready while older
    /// dictations are still processed, it follows the newest of them.
    pub(crate) fn end(&mut self, id: JobId, end: WorkflowSignal) {
        let needs_attention = matches!(end, WorkflowSignal::Interrupted { .. });
        if let Err(workflow_error) = self.workflow.apply(end) {
            tracing::warn!(job_id = %id, %workflow_error, ?end, "rebuilding the workflow");
            self.workflow = Workflow::new();
            if needs_attention {
                let _ = self
                    .workflow
                    .apply(WorkflowSignal::StartRequested { job_id: id });
                let _ = self.workflow.apply(end);
            }
        }
        if self.workflow.snapshot().phase == WorkflowPhase::Ready
            && let Some(&newest) = self.in_flight.back()
        {
            self.workflow = processing_workflow(newest);
        }
    }

    /// Clears what the workflow shows; it then follows the newest dictation
    /// still being processed, if any.
    pub(crate) fn reset_workflow(&mut self) {
        self.workflow = self
            .in_flight
            .back()
            .map_or_else(Workflow::new, |&newest| processing_workflow(newest));
    }

    /// Moves the workflow's dictation `id` to delivering. Refused steps
    /// are skipped: the end of the dictation rebuilds the workflow.
    fn advance_to_delivery(&mut self, id: JobId) {
        let stored = match self.workflow.snapshot().phase {
            WorkflowPhase::Processing {
                stage: ProcessingStage::Cleaning,
                ..
            } => WorkflowSignal::CleanupStored { job_id: id },
            _ => WorkflowSignal::TranscriptStored { job_id: id },
        };
        let _ = self.workflow.apply(stored);
        let _ = self
            .workflow
            .apply(WorkflowSignal::DeliveryStarted { job_id: id });
    }

    /// Recovery items that need the user. An Esc-cancelled recording is
    /// kept only in case the cancel was a mistake, and a dictation still
    /// being processed is not recoverable yet.
    pub(crate) fn attention_count(&self, entries: &[RecoveryEntry]) -> usize {
        entries
            .iter()
            .filter(|entry| entry.stage != JobStage::Canceled && !self.is_in_flight(entry.job_id))
            .count()
    }

    /// Recounts Recovery from `entries`, read after the change that needs
    /// the recount. When they could not be read, a dictation that needs
    /// attention still shows as one.
    pub(crate) fn recount(
        &mut self,
        id: JobId,
        entries: Result<Vec<RecoveryEntry>, RuntimeError>,
        needs_attention: bool,
    ) {
        match entries {
            Ok(entries) => self.recoverable_count = self.attention_count(&entries),
            Err(recovery_error) => {
                tracing::error!(
                    job_id = %id,
                    %recovery_error,
                    "could not recount recoverable recordings"
                );
                if needs_attention {
                    self.recoverable_count = self.recoverable_count.max(1);
                }
            }
        }
    }

    pub(crate) fn has_finished(&self, id: JobId) -> bool {
        self.finished.iter().any(|(job, _)| *job == id)
    }

    pub(crate) fn take_finished(&mut self, id: JobId) -> Option<Result<RecordingJob, DaemonError>> {
        let index = self.finished.iter().position(|(job, _)| *job == id)?;
        self.finished.remove(index).map(|(_, outcome)| outcome)
    }

    fn record_finished(&mut self, id: JobId, outcome: Result<RecordingJob, DaemonError>) {
        if self.finished.len() == FINISHED_KEPT {
            self.finished.pop_front();
        }
        self.finished.push_back((id, outcome));
    }
}

/// A workflow that shows dictation `id` transcribing.
fn processing_workflow(id: JobId) -> Workflow {
    let mut workflow = Workflow::new();
    for signal in [
        WorkflowSignal::StartRequested { job_id: id },
        WorkflowSignal::FirstAudioFrameWritten { job_id: id },
        WorkflowSignal::StopRequested,
        WorkflowSignal::CaptureFinalized { job_id: id },
    ] {
        let _ = workflow.apply(signal);
    }
    workflow
}

type Task = Box<dyn FnOnce(&mut WorkerConnection) + Send>;

/// The processing worker: one thread that runs dictations in the order
/// they were stopped. It starts with the first dictation, and ends once
/// the queue is dropped and empty.
pub(crate) struct ProcessingQueue {
    database: PathBuf,
    sender: Option<Sender<Task>>,
}

impl ProcessingQueue {
    pub(crate) const fn new(database: PathBuf) -> Self {
        Self {
            database,
            sender: None,
        }
    }

    /// Runs `task` on the worker after every task pushed before it. If no
    /// worker can be started, it runs here instead: late, but never lost.
    pub(crate) fn push(&mut self, task: Task) {
        let task = match &self.sender {
            Some(sender) => match sender.send(task) {
                Ok(()) => return,
                Err(SendError(task)) => task,
            },
            None => task,
        };
        let (sender, receiver) = channel::<Task>();
        let database = self.database.clone();
        let spawned = std::thread::Builder::new()
            .name("agentdictate-processing".into())
            .spawn(move || {
                let mut connection = WorkerConnection::new(database);
                while let Ok(task) = receiver.recv() {
                    // Tasks catch their own panics; this only keeps the
                    // worker serving the dictations queued behind one.
                    if panic::catch_unwind(AssertUnwindSafe(|| task(&mut connection))).is_err() {
                        connection.runtime = None;
                    }
                }
            });
        let task = match spawned {
            Ok(_) => match sender.send(task) {
                Ok(()) => {
                    self.sender = Some(sender);
                    return;
                }
                Err(SendError(task)) => task,
            },
            Err(error) => {
                tracing::error!(%error, "could not start the processing worker");
                task
            }
        };
        self.sender = None;
        task(&mut WorkerConnection::new(self.database.clone()));
    }
}

/// The worker's own database connection, opened when first needed. It
/// never runs startup reconciliation, which would misread the daemon's
/// live recording as abandoned.
pub(crate) struct WorkerConnection {
    database: PathBuf,
    runtime: Option<Runtime>,
}

impl WorkerConnection {
    const fn new(database: PathBuf) -> Self {
        Self {
            database,
            runtime: None,
        }
    }

    fn runtime(&mut self) -> Result<&mut Runtime, RuntimeError> {
        if self.runtime.is_none() {
            self.runtime = Some(Runtime::open_background_writer(&self.database)?);
        }
        Ok(self
            .runtime
            .as_mut()
            .expect("the connection was just opened"))
    }
}

/// One stopped dictation, durable at its Captured checkpoint, with
/// everything the worker needs to finish it without the daemon lock.
pub(crate) struct StoppedDictation<J, D> {
    pub(crate) job: RecordingJob,
    pub(crate) transcriber: J,
    pub(crate) deliverer: Arc<Mutex<D>>,
    /// The window focused at the stop; the paste may only land there.
    pub(crate) stopped_in: Option<FocusedWindow>,
    /// The settings in effect at the stop.
    pub(crate) settings: Settings,
    pub(crate) shared: Arc<Shared>,
    pub(crate) stopped_at: Instant,
    /// Keeps deferred index maintenance from holding the database's write
    /// lock until this dictation is done.
    pub(crate) _priority: Option<RecordingPriorityGuard>,
}

impl<J, D> StoppedDictation<J, D>
where
    J: Transcriber + Send + 'static,
    D: Deliverer + Send + 'static,
{
    pub(crate) fn into_task(self) -> Task {
        Box::new(move |connection| self.run(connection))
    }

    fn run(mut self, connection: &mut WorkerConnection) {
        let id = self.job.id;
        {
            let mut state = self.shared.lock();
            if state.quitting || !state.is_in_flight(id) {
                // Shutdown already left it in Recovery.
                tracing::warn!(job_id = %id, "not processing a dictation stopped during shutdown");
                state.in_flight.retain(|job| *job != id);
                drop(state);
                self.shared.changed.notify_all();
                return;
            }
            state.running = Some(id);
        }
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| self.process(connection)))
            .unwrap_or_else(|_| {
                tracing::error!(job_id = %id, "dictation processing panicked");
                connection.runtime = None;
                let error = RuntimeError::External(ExternalError::new(
                    "processing stopped unexpectedly; the audio is saved",
                ));
                Err(match connection.runtime() {
                    Ok(runtime) => runtime.fail_in_flight_job(id, error),
                    Err(_) => error,
                })
            });
        self.finish(connection, outcome);
    }

    /// Transcribes, cleans and pastes on the worker's connection. Only the
    /// short `Shared` and deliverer locks are taken here.
    fn process(&mut self, connection: &mut WorkerConnection) -> Result<RecordingJob, RuntimeError> {
        let id = self.job.id;
        let runtime = connection.runtime()?;
        let result = runtime.process_captured(
            id,
            &mut self.transcriber,
            &mut WorkerGate {
                shared: &self.shared,
                id,
                hold_mode: self.settings.recording_mode == "hold",
            },
            &mut LockedDeliverer {
                deliverer: &self.deliverer,
                stopped_in: self.stopped_in,
            },
        )?;
        if result.stage == JobStage::Delivered
            && let Err(error) = runtime.complete_delivered(id, &self.settings)
        {
            // The paste command was already submitted. A bookkeeping failure
            // must never make it retryable and risk a duplicate paste; the
            // next daemon start completes the job instead.
            tracing::error!(job_id = %id, %error, "could not complete delivered dictation");
        }
        if matches!(result.stage, JobStage::Delivered | JobStage::NoSpeech) {
            remove_completed_audio(&self.settings, &result);
        }
        Ok(result)
    }

    /// Publishes how the dictation ended: on the pill only when the
    /// workflow still follows it, and always to the tray.
    fn finish(
        self,
        connection: &mut WorkerConnection,
        outcome: Result<RecordingJob, RuntimeError>,
    ) {
        let id = self.job.id;
        // Everything that reads the database runs before the lock.
        let (end, notice) = match &outcome {
            Ok(result) if result.stage == JobStage::NoSpeech => (
                WorkflowSignal::NoSpeechDetected { job_id: id },
                dictation_notice(DictationEnd::Finished(result)),
            ),
            Ok(result) => (
                if result.stage == JobStage::Delivered {
                    WorkflowSignal::DeliverySubmitted { job_id: id }
                } else {
                    WorkflowSignal::Interrupted {
                        job_id: id,
                        at: result.stage,
                    }
                },
                dictation_notice(DictationEnd::Finished(result)),
            ),
            Err(error) => {
                tracing::error!(job_id = %id, %error, "dictation processing failed");
                let persisted = connection
                    .runtime()
                    .ok()
                    .and_then(|runtime| runtime.job(id).ok().flatten());
                let at = persisted.as_ref().map_or(JobStage::Failed, |job| job.stage);
                (
                    WorkflowSignal::Interrupted { job_id: id, at },
                    dictation_notice(DictationEnd::Failed {
                        error,
                        job: persisted.as_ref(),
                    }),
                )
            }
        };
        let entries = connection
            .runtime()
            .and_then(|runtime| runtime.recovery_entries());
        let mut state = self.shared.lock();
        state.in_flight.retain(|job| *job != id);
        state.running = None;
        if state.follows(id) {
            if matches!(&outcome, Ok(result) if result.stage != JobStage::NoSpeech) {
                state.advance_to_delivery(id);
            }
            state.end(id, end);
        }
        if let Ok(result) = &outcome
            && result.stage != JobStage::NoSpeech
        {
            state.last_transcript = Some(result.final_text.clone());
        }
        state.recount(
            id,
            entries,
            matches!(end, WorkflowSignal::Interrupted { .. }),
        );
        match &outcome {
            Ok(result) => tracing::info!(
                job_id = %id,
                stage = ?result.stage,
                stop_to_paste_ms = self.stopped_at.elapsed().as_millis() as u64,
                "dictation flow completed"
            ),
            Err(_) => tracing::info!(job_id = %id, "dictation flow ended in Recovery"),
        }
        state.record_finished(id, outcome.map_err(DaemonError::from));
        // A shutdown is announced by the app closing, not by a notice.
        let notice = notice.filter(|_| !state.quitting);
        self.shared.changed(state, notice.as_ref());
    }
}

/// Confirms a paste for the worker. The pill is dismissed only when it
/// shows this dictation, or where it could take the paste's focus;
/// otherwise it keeps showing the newer recording.
struct WorkerGate<'a> {
    shared: &'a Shared,
    id: JobId,
    hold_mode: bool,
}

/// How long one wait for a newer hold-mode recording lasts before it is
/// checked again; the wait itself ends when that recording does.
const HOLD_RECHECK: Duration = Duration::from_secs(1);

impl DeliveryGate for WorkerGate<'_> {
    fn confirm_ready(&mut self) -> Result<(), DeliveryGateError> {
        let mut state = self.shared.lock();
        // In hold mode the shortcut stays down for the whole of a newer
        // recording, and a paste needs its modifiers up: wait for that
        // recording to stop rather than let the paste time out.
        while self.hold_mode && !state.quitting && state.records_another(self.id) {
            (state, _) = self.shared.wait_while(state, HOLD_RECHECK, |state| {
                !state.quitting && state.records_another(self.id)
            });
        }
        if state.quitting {
            return Err(DeliveryGateError::new(
                "AgentDictate quit before the paste; the transcript is saved",
            ));
        }
        let follows = state.follows(self.id);
        if follows {
            state.advance_to_delivery(self.id);
            state.sequence += 1;
        }
        let overlay = match &state.overlay {
            OverlayGate::Live(overlay) if follows || !OVERLAY_CANNOT_TAKE_FOCUS => {
                Some(overlay.clone())
            }
            OverlayGate::Live(_) | OverlayGate::Headless(_) => None,
        };
        drop(state);
        overlay.map_or(Ok(()), |mut overlay| overlay.confirm_ready())
    }
}

/// Pastes through the shared deliverer, into the window the dictation was
/// stopped in, holding the deliverer only for the paste itself.
struct LockedDeliverer<'a, D> {
    deliverer: &'a Mutex<D>,
    stopped_in: Option<FocusedWindow>,
}

impl<D: Deliverer> Deliverer for LockedDeliverer<'_, D> {
    fn deliver(&mut self, job: &RecordingJob) -> Result<DeliveryDisposition, ExternalError> {
        lock_deliverer(self.deliverer).deliver_to(job, self.stopped_in)
    }
}

pub(crate) fn lock_deliverer<D>(deliverer: &Mutex<D>) -> MutexGuard<'_, D> {
    deliverer.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn remove_completed_audio(settings: &Settings, job: &RecordingJob) {
    if settings.preserve_temp_audio {
        return;
    }
    match std::fs::remove_file(&job.audio_path) {
        Ok(()) => tracing::debug!(job_id = %job.id, "removed completed recording audio"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(job_id = %job.id, %error, "could not remove completed recording audio");
        }
    }
}
