use std::{
    io,
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, channel, sync_channel},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use agentdictate_runtime::{DeliveryGate, DeliveryGateError};
use agentdictate_ui::OverlayState;
use thiserror::Error;

use super::{
    child::OverlayChild,
    protocol::{OverlayHelperStatus, OverlayUpdate},
};

pub const OVERLAY_HEALTH_FILE: &str = "overlay-health";

const AUTOMATIC_RESTART_LIMIT_PER_UPDATE: u8 = 1;
const OVERLAY_READY_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(test)]
#[path = "startup_tests.rs"]
mod startup_tests;

#[cfg(all(test, windows))]
mod focusless_teardown_tests {
    use super::*;

    use agentdictate_core::{JobId, Workflow, WorkflowSignal};

    fn controller() -> (OverlayController, Receiver<OverlayCommand>) {
        let (commands, received) = channel();
        let controller = OverlayController {
            commands,
            health: Arc::new(OverlayHealth::default()),
        };
        (controller, received)
    }

    /// The presenter side of a dismissal: the reply the supervisor would hold.
    fn received_dismissal(
        received: &Receiver<OverlayCommand>,
    ) -> SyncSender<Result<(), OverlayTeardownError>> {
        match received.recv_timeout(Duration::from_secs(1)) {
            Ok(OverlayCommand::Dismiss { reply }) => reply,
            _ => panic!("the controller did not send a dismissal"),
        }
    }

    #[test]
    fn the_windows_paste_does_not_wait_for_the_pill_to_fade() {
        let (controller, received) = controller();
        // A presenter that accepts the dismissal but never observes the exit.
        let started = Instant::now();
        let result = controller.dismiss_and_wait();
        let elapsed = started.elapsed();
        assert!(result.is_ok(), "paste must proceed: {result:?}");
        assert!(
            elapsed < Duration::from_millis(50),
            "paste waited {elapsed:?} for the overlay"
        );
        let _reply = received_dismissal(&received);
        assert!(
            received.try_recv().is_err(),
            "the fade must not be cut short"
        );
    }

    #[test]
    fn a_pill_that_does_not_exit_is_terminated_after_the_teardown_timeout() {
        let (controller, received) = controller();
        let timeout = Duration::from_millis(100);
        let started = Instant::now();
        controller.dismiss_during_paste(timeout).unwrap();
        let _reply = received_dismissal(&received);

        let forced = received.recv_timeout(Duration::from_secs(5));

        assert!(matches!(forced, Ok(OverlayCommand::ForceDismiss)));
        assert!(started.elapsed() >= timeout);
    }

    #[test]
    fn a_pill_that_exits_in_time_is_never_terminated() {
        let (controller, received) = controller();
        let timeout = Duration::from_millis(100);
        controller.dismiss_during_paste(timeout).unwrap();
        received_dismissal(&received).send(Ok(())).unwrap();

        let after = received.recv_timeout(timeout * 4);

        assert!(
            matches!(after, Err(RecvTimeoutError::Timeout)),
            "an acknowledged dismissal was forced"
        );
    }

    fn recording() -> OverlayUpdate {
        let job_id = JobId::new();
        let mut workflow = Workflow::new();
        workflow
            .apply(WorkflowSignal::StartRequested { job_id })
            .unwrap();
        workflow
            .apply(WorkflowSignal::FirstAudioFrameWritten { job_id })
            .unwrap();
        OverlayUpdate {
            workflow: workflow.snapshot(),
            active_recording: None,
            notice: None,
        }
    }

    /// A helper whose first launch never finishes its fade (bounded to about
    /// 15 s so a failing run does not leak it) and whose later launches exit
    /// when their input closes.
    const FAKE_HELPER: &str = "@echo off\r\n\
        if exist \"%~dp0first-launched\" goto graceful\r\n\
        echo.>\"%~dp0first-launched\"\r\n\
        echo {\"status\":\"frame_submitted\"}\r\n\
        set n=0\r\n\
        :hang\r\n\
        ping -n 2 127.0.0.1 >nul\r\n\
        set /a n+=1\r\n\
        if %n% lss 15 goto hang\r\n\
        exit /b 0\r\n\
        :graceful\r\n\
        echo {\"status\":\"frame_submitted\"}\r\n\
        more >nul\r\n\
        exit /b 0\r\n";

    /// Feeds helper statuses to `supervisor` until `wanted` matches an event.
    fn pump_until(
        supervisor: &mut OverlaySupervisor<'_>,
        events: &Receiver<PresenterEvent>,
        wanted: impl Fn(&PresenterEvent) -> bool,
    ) -> PresenterEvent {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let event = events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("the overlay helper did not respond in time");
            if wanted(&event) {
                return event;
            }
            if let PresenterEvent::HelperStatus { generation, status } = event {
                supervisor.handle_helper_status(generation, status);
            }
        }
    }

    fn exit_of(
        supervisor: &mut OverlaySupervisor<'_>,
        events: &Receiver<PresenterEvent>,
    ) -> (u64, io::Result<ExitStatus>) {
        match pump_until(supervisor, events, |event| {
            matches!(event, PresenterEvent::HelperExited { .. })
        }) {
            PresenterEvent::HelperExited { generation, result } => (generation, result),
            _ => unreachable!(),
        }
    }

    #[test]
    fn a_dictation_started_while_the_pill_fades_keeps_its_own_pill() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("overlay-helper.cmd");
        std::fs::write(&executable, FAKE_HELPER).unwrap();
        let (events, event_receiver) = channel();
        let mut supervisor = OverlaySupervisor::new(
            &executable,
            Duration::from_secs(5),
            events,
            Arc::new(OverlayHealth::default()),
        );
        let shown = |wanted: u64| {
            move |event: &PresenterEvent| {
                matches!(
                    event,
                    PresenterEvent::HelperStatus {
                        generation,
                        status: Ok(OverlayHelperStatus::FrameSubmitted),
                    } if *generation == wanted
                )
            }
        };

        supervisor.handle_update(recording());
        pump_until(&mut supervisor, &event_receiver, shown(1));
        let (reply, first_acknowledgment) = sync_channel(1);
        supervisor.handle_command(OverlayCommand::Dismiss { reply });
        // The paste went ahead; the next dictation starts while pill 1 fades.
        supervisor.handle_update(recording());
        assert_eq!(
            supervisor.helper.as_ref().map(OverlayChild::generation),
            Some(2)
        );
        pump_until(&mut supervisor, &event_receiver, shown(2));
        // Pill 1's watchdog fires: it must stop pill 1, not the new pill.
        supervisor.handle_command(OverlayCommand::ForceDismiss);

        let (generation, result) = exit_of(&mut supervisor, &event_receiver);
        assert_eq!(generation, 1, "the fading pill was not the one stopped");
        supervisor.handle_helper_exit(generation, result);
        assert!(matches!(first_acknowledgment.try_recv(), Ok(Ok(()))));
        let current = supervisor
            .helper
            .as_mut()
            .expect("the new pill was dropped");
        assert_eq!(current.generation(), 2);
        current
            .send(&recording())
            .expect("the new pill must still accept updates");

        let (reply, second_acknowledgment) = sync_channel(1);
        supervisor.handle_command(OverlayCommand::Dismiss { reply });
        let (generation, result) = exit_of(&mut supervisor, &event_receiver);
        assert_eq!(generation, 2);
        supervisor.handle_helper_exit(generation, result);
        assert!(matches!(second_acknowledgment.try_recv(), Ok(Ok(()))));
        assert!(supervisor.helper.is_none() && supervisor.pending_dismissal.is_none());
    }
}
// Upper bound the daemon waits for the helper to exit after dismissal. The
/// dismissal ack now includes the helper's fade-out, so this must comfortably
/// exceed the UI crate's `OVERLAY_FADE_HOLD` plus process teardown; the
/// relation is asserted by the overlay lifecycle tests.
#[doc(hidden)]
pub const OVERLAY_TEARDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// The Windows overlay window is created `WS_EX_NOACTIVATE | WS_EX_TRANSPARENT`,
/// so it cannot become the paste target even while its process lingers.
const OVERLAY_CANNOT_TAKE_FOCUS: bool = cfg!(windows);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlayProcessAction {
    StayHeadless,
    Launch,
    Update,
    Stop,
}

#[derive(Debug, Error)]
pub enum OverlayTeardownError {
    #[error("recording overlay presenter is unavailable")]
    PresenterUnavailable,
    #[error("recording overlay helper did not exit before the teardown deadline")]
    TimedOut,
    #[error("recording overlay helper exit could not be confirmed: {0}")]
    ExitObservation(#[source] io::Error),
}

#[derive(Clone)]
pub struct OverlayController {
    commands: Sender<OverlayCommand>,
    health: Arc<OverlayHealth>,
}

#[derive(Default)]
struct OverlayHealth {
    unavailable: AtomicBool,
    notification_file: Mutex<Option<PathBuf>>,
}

impl OverlayHealth {
    fn set_unavailable(&self, unavailable: bool) {
        if self.unavailable.swap(unavailable, Ordering::AcqRel) != unavailable {
            self.notify();
        }
    }

    fn notify(&self) {
        if let Ok(path) = self.notification_file.lock()
            && let Some(path) = path.as_ref()
            && let Err(error) = std::fs::write(path, [])
        {
            tracing::warn!(%error, "could not notify desktop of overlay health change");
        }
    }
}

impl OverlayController {
    pub fn is_unavailable(&self) -> bool {
        self.health.unavailable.load(Ordering::Acquire)
    }

    /// The file is only an inotify signal; the daemon snapshot owns the value.
    pub fn notify_health_changes_at(&self, path: PathBuf) {
        *self
            .health
            .notification_file
            .lock()
            .expect("overlay health lock") = Some(path);
        self.health.notify();
    }

    pub fn update(&self, update: OverlayUpdate) {
        let _ = self.commands.send(OverlayCommand::Update(update));
    }

    /// Closes the overlay before a paste and returns once the paste is safe.
    /// Where the overlay cannot take focus (Windows) that is at once: the
    /// fade-out overlaps the paste, and a watchdog kills a helper that has not
    /// exited by `OVERLAY_TEARDOWN_TIMEOUT`. Elsewhere (X11) the paste waits
    /// for the helper's observed exit.
    pub fn dismiss_and_wait(&self) -> Result<(), OverlayTeardownError> {
        let started = Instant::now();
        let result = if OVERLAY_CANNOT_TAKE_FOCUS {
            self.dismiss_during_paste(OVERLAY_TEARDOWN_TIMEOUT)
        } else {
            self.dismiss_and_confirm_exit()
        };
        tracing::info!(
            overlay_wait_ms = started.elapsed().as_millis() as u64,
            success = result.is_ok(),
            "recording overlay dismissal completed"
        );
        result
    }

    /// Starts the fade and returns without waiting for the helper to exit.
    /// A watchdog thread waits up to `teardown_timeout` for the exit and then
    /// sends `ForceDismiss`, so a stalled renderer cannot leave a frozen pill
    /// on screen. If a newer dismissal supersedes this one, or the presenter
    /// stops, the acknowledgment disconnects and the watchdog does nothing.
    fn dismiss_during_paste(&self, teardown_timeout: Duration) -> Result<(), OverlayTeardownError> {
        let (reply, acknowledgment) = sync_channel(1);
        self.commands
            .send(OverlayCommand::Dismiss { reply })
            .map_err(|_| OverlayTeardownError::PresenterUnavailable)?;
        let commands = self.commands.clone();
        let dismissed_at = Instant::now();
        let watchdog = std::thread::Builder::new()
            .name("agentdictate-overlay-teardown".into())
            .spawn(
                move || match acknowledgment.recv_timeout(teardown_timeout) {
                    Ok(result) => tracing::info!(
                        overlay_fade_ms = dismissed_at.elapsed().as_millis() as u64,
                        success = result.is_ok(),
                        "recording overlay helper exited after dismissal"
                    ),
                    Err(RecvTimeoutError::Timeout) => {
                        tracing::warn!("overlay graceful teardown timed out; terminating helper");
                        let _ = commands.send(OverlayCommand::ForceDismiss);
                    }
                    Err(RecvTimeoutError::Disconnected) => {}
                },
            );
        if let Err(error) = watchdog {
            tracing::warn!(%error, "overlay teardown watchdog could not start; terminating helper");
            let _ = self.commands.send(OverlayCommand::ForceDismiss);
        }
        Ok(())
    }

    /// Waits for the helper's observed exit, killing it after
    /// `OVERLAY_TEARDOWN_TIMEOUT`. A window that can take focus must be gone
    /// before the paste, so an unconfirmed exit fails the delivery.
    fn dismiss_and_confirm_exit(&self) -> Result<(), OverlayTeardownError> {
        let (reply, acknowledgment) = sync_channel(1);
        self.commands
            .send(OverlayCommand::Dismiss { reply })
            .map_err(|_| OverlayTeardownError::PresenterUnavailable)?;
        match acknowledgment.recv_timeout(OVERLAY_TEARDOWN_TIMEOUT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                tracing::warn!(
                    "overlay graceful teardown timed out; terminating helper before paste"
                );
                self.commands
                    .send(OverlayCommand::ForceDismiss)
                    .map_err(|_| OverlayTeardownError::PresenterUnavailable)?;
                // Killing the presentation-only helper is safe, but paste still
                // requires its observed process exit, not merely a kill request.
                match acknowledgment.recv_timeout(OVERLAY_TEARDOWN_TIMEOUT) {
                    Ok(result) => result,
                    Err(RecvTimeoutError::Timeout) => Err(OverlayTeardownError::TimedOut),
                    Err(RecvTimeoutError::Disconnected) => {
                        Err(OverlayTeardownError::PresenterUnavailable)
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => Err(OverlayTeardownError::PresenterUnavailable),
        }
    }
}

impl DeliveryGate for OverlayController {
    fn confirm_ready(&mut self) -> Result<(), DeliveryGateError> {
        self.dismiss_and_wait()
            .map_err(|error| DeliveryGateError::new(error.to_string()))
    }
}

pub(super) enum OverlayCommand {
    Update(OverlayUpdate),
    Dismiss {
        reply: SyncSender<Result<(), OverlayTeardownError>>,
    },
    ForceDismiss,
}

/// Tracks whether the transient notification helper exists. The daemon itself
/// never creates a GPUI application or window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OverlayProcessState {
    running: bool,
}

impl OverlayProcessState {
    pub fn transition(&mut self, update: &OverlayUpdate) -> OverlayProcessAction {
        let visible = OverlayState::from(update.workflow).is_visible();
        match (self.running, visible) {
            (false, false) => OverlayProcessAction::StayHeadless,
            (false, true) => {
                self.running = true;
                OverlayProcessAction::Launch
            }
            (true, true) => OverlayProcessAction::Update,
            (true, false) => {
                self.running = false;
                OverlayProcessAction::Stop
            }
        }
    }

    pub fn mark_stopped(&mut self) {
        self.running = false;
    }
}

pub fn start_overlay_presenter(
    executable: PathBuf,
) -> io::Result<(OverlayController, JoinHandle<()>)> {
    start_overlay_presenter_with_timeout(executable, OVERLAY_READY_TIMEOUT)
}

#[doc(hidden)]
pub fn start_overlay_presenter_with_timeout(
    executable: PathBuf,
    ready_timeout: Duration,
) -> io::Result<(OverlayController, JoinHandle<()>)> {
    let (commands, receiver) = channel();
    let health = Arc::new(OverlayHealth::default());
    let controller = OverlayController {
        commands,
        health: Arc::clone(&health),
    };
    let presenter = std::thread::Builder::new()
        .name("agentdictate-overlay-presenter".into())
        .spawn(move || {
            overlay_presenter_loop(&executable, receiver, ready_timeout, health);
        })?;
    Ok((controller, presenter))
}

fn overlay_presenter_loop(
    executable: &Path,
    commands: Receiver<OverlayCommand>,
    ready_timeout: Duration,
    health: Arc<OverlayHealth>,
) {
    let (events, event_receiver) = channel();
    let update_events = events.clone();
    let Ok(update_forwarder) = std::thread::Builder::new()
        .name("agentdictate-overlay-updates".into())
        .spawn(move || {
            while let Ok(command) = commands.recv() {
                if update_events
                    .send(PresenterEvent::Command(command))
                    .is_err()
                {
                    return;
                }
            }
            let _ = update_events.send(PresenterEvent::UpdatesClosed);
        })
    else {
        tracing::warn!("recording overlay update forwarder could not start");
        health.set_unavailable(true);
        return;
    };

    let mut supervisor = OverlaySupervisor::new(executable, ready_timeout, events.clone(), health);
    while let Ok(event) = event_receiver.recv() {
        match event {
            PresenterEvent::Command(command) => supervisor.handle_command(command),
            PresenterEvent::HelperStatus { generation, status } => {
                supervisor.handle_helper_status(generation, status);
            }
            PresenterEvent::HelperExited { generation, result } => {
                supervisor.handle_helper_exit(generation, result);
            }
            PresenterEvent::UpdatesClosed => {
                supervisor.shutdown();
                break;
            }
        }
    }
    let _ = update_forwarder.join();
}

pub(super) enum PresenterEvent {
    Command(OverlayCommand),
    HelperStatus {
        generation: u64,
        status: Result<OverlayHelperStatus, String>,
    },
    HelperExited {
        generation: u64,
        result: io::Result<ExitStatus>,
    },
    UpdatesClosed,
}

struct OverlaySupervisor<'a> {
    executable: &'a Path,
    ready_timeout: Duration,
    events: Sender<PresenterEvent>,
    lifecycle: OverlayProcessState,
    health: Arc<OverlayHealth>,
    helper: Option<OverlayChild>,
    last_visible_update: Option<OverlayUpdate>,
    remaining_restarts: u8,
    next_generation: u64,
    pending_dismissal: Option<PendingDismissal>,
}

/// A dismissed helper that is fading out. It leaves `helper` at dismissal, so
/// a new dictation can launch the next helper while this one finishes, and a
/// later `ForceDismiss` still reaches this one rather than its successor.
struct PendingDismissal {
    child: OverlayChild,
    reply: SyncSender<Result<(), OverlayTeardownError>>,
}

impl<'a> OverlaySupervisor<'a> {
    fn new(
        executable: &'a Path,
        ready_timeout: Duration,
        events: Sender<PresenterEvent>,
        health: Arc<OverlayHealth>,
    ) -> Self {
        Self {
            executable,
            ready_timeout,
            events,
            lifecycle: OverlayProcessState::default(),
            health,
            helper: None,
            last_visible_update: None,
            remaining_restarts: 0,
            next_generation: 0,
            pending_dismissal: None,
        }
    }

    fn handle_command(&mut self, command: OverlayCommand) {
        match command {
            OverlayCommand::Update(update) => self.handle_update(update),
            OverlayCommand::Dismiss { reply } => self.dismiss_and_wait(reply),
            OverlayCommand::ForceDismiss => self.force_dismissal(),
        }
    }

    fn dismiss_and_wait(&mut self, reply: SyncSender<Result<(), OverlayTeardownError>>) {
        self.last_visible_update = None;
        self.remaining_restarts = 0;
        self.lifecycle.mark_stopped();
        let Some(mut child) = self.helper.take() else {
            let _ = reply.send(Ok(()));
            return;
        };
        child.finish();
        // An older helper still fading when the next dictation is dismissed
        // has had its chance; its waiter sees a disconnect and stands down.
        if let Some(mut superseded) = self
            .pending_dismissal
            .replace(PendingDismissal { child, reply })
        {
            superseded.child.terminate();
        }
    }

    fn force_dismissal(&mut self) {
        if let Some(pending) = self.pending_dismissal.as_mut() {
            pending.child.terminate();
        }
    }

    fn is_dismissed(&self, generation: u64) -> bool {
        self.pending_dismissal
            .as_ref()
            .is_some_and(|pending| pending.child.generation() == generation)
    }

    fn handle_update(&mut self, update: OverlayUpdate) {
        if OverlayState::from(update.workflow).is_visible() {
            self.last_visible_update = Some(update.clone());
            self.remaining_restarts = AUTOMATIC_RESTART_LIMIT_PER_UPDATE;
        } else {
            // Clear this before closing the helper so a normal hidden
            // transition is ineligible for restart when its exit arrives.
            self.last_visible_update = None;
            self.remaining_restarts = 0;
        }

        match self.lifecycle.transition(&update) {
            OverlayProcessAction::StayHeadless => {}
            OverlayProcessAction::Launch => {
                self.launch(&update, "recording overlay helper could not start");
            }
            OverlayProcessAction::Update => {
                let send_result = self
                    .helper
                    .as_mut()
                    .ok_or_else(|| io::Error::other("overlay helper is missing"))
                    .and_then(|child| child.send(&update));
                if let Err(error) = send_result {
                    tracing::warn!(%error, "recording overlay helper disconnected");
                    if let Some(mut child) = self.helper.take() {
                        child.terminate();
                    }
                    self.lifecycle.mark_stopped();
                    if self.lifecycle.transition(&update) == OverlayProcessAction::Launch {
                        self.launch(&update, "recording overlay helper could not recover");
                    }
                }
            }
            OverlayProcessAction::Stop => {
                if let Some(mut child) = self.helper.take() {
                    let _ = child.send(&update);
                    child.finish();
                }
            }
        }
    }

    fn handle_helper_status(
        &mut self,
        generation: u64,
        status: Result<OverlayHelperStatus, String>,
    ) {
        if self.is_dismissed(generation) {
            return;
        }
        let Some(child) = self
            .helper
            .as_mut()
            .filter(|child| child.generation() == generation)
        else {
            return;
        };
        match status {
            Ok(OverlayHelperStatus::WindowCreated) => {
                tracing::info!(
                    generation,
                    "recording overlay window created; awaiting a submitted frame"
                );
            }
            Ok(OverlayHelperStatus::FrameSubmitted) => {
                child.mark_ready();
                self.health.set_unavailable(false);
                tracing::info!(generation, "recording overlay first frame submitted");
            }
            Ok(OverlayHelperStatus::Error { message }) => {
                tracing::warn!(generation, %message, "recording overlay helper presentation failed");
                self.health.set_unavailable(true);
                child.terminate();
            }
            Err(error) => {
                tracing::warn!(generation, %error, "recording overlay helper presentation failed");
                self.health.set_unavailable(true);
                child.terminate();
            }
        }
    }

    fn handle_helper_exit(&mut self, generation: u64, result: io::Result<ExitStatus>) {
        if self.is_dismissed(generation) {
            let pending = self
                .pending_dismissal
                .take()
                .expect("matching pending dismissal must exist");
            let acknowledgment = result
                .map(|_| ())
                .map_err(OverlayTeardownError::ExitObservation);
            let _ = pending.reply.send(acknowledgment);
            return;
        }
        if self.helper.as_ref().map(OverlayChild::generation) != Some(generation) {
            return;
        }
        self.health.set_unavailable(true);
        let was_ready = self.helper.take().is_some_and(|child| child.is_ready());
        self.lifecycle.mark_stopped();
        match result {
            Err(error) => {
                tracing::warn!(generation, %error, "recording overlay helper exit could not be observed")
            }
            Ok(status) => {
                tracing::warn!(generation, %status, was_ready, "recording overlay helper exited unexpectedly")
            }
        }

        if let Some(update) = self.last_visible_update.clone() {
            if self.remaining_restarts == 0 {
                tracing::warn!(
                    "recording overlay helper restart budget exhausted; waiting for a new update"
                );
                return;
            }
            self.remaining_restarts -= 1;
            if self.lifecycle.transition(&update) == OverlayProcessAction::Launch {
                self.launch(&update, "recording overlay helper could not relaunch");
            }
        }
    }

    fn launch(&mut self, update: &OverlayUpdate, failure_message: &'static str) {
        self.next_generation = self.next_generation.wrapping_add(1);
        match OverlayChild::launch(
            self.executable,
            self.ready_timeout,
            self.next_generation,
            self.events.clone(),
        ) {
            Ok(mut child) => {
                if let Err(error) = child.send(update) {
                    // Keep the generation installed so its kernel exit event
                    // drives the same recovery path as a later disconnect.
                    tracing::warn!(%error, "recording overlay helper rejected its initial update");
                }
                self.helper = Some(child);
            }
            Err(error) => {
                self.lifecycle.mark_stopped();
                self.health.set_unavailable(true);
                tracing::warn!(%error, "{failure_message}");
            }
        }
    }

    fn shutdown(&mut self) {
        self.last_visible_update = None;
        self.remaining_restarts = 0;
        self.lifecycle.mark_stopped();
        if let Some(mut child) = self.helper.take() {
            child.finish();
        }
        if let Some(pending) = self.pending_dismissal.take() {
            let _ = pending
                .reply
                .send(Err(OverlayTeardownError::PresenterUnavailable));
        }
    }
}
