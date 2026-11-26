#![cfg_attr(windows, windows_subsystem = "windows")]
use std::sync::{Arc, Mutex};

use agentdictate_app::{
    AgentProcess, AppPaths, SERVICE_ARGUMENT, START_SERVICE_ARGUMENT, bootstrap_daemon_service,
    init_file_logging, is_copilot_argument, is_overlay_helper_argument, run_overlay_helper,
    settings_executable_for_current_process, start_hotkey_listener, start_overlay_presenter,
    start_system_tray,
};
#[cfg(unix)]
use agentdictate_core::{ClientCommand, ServerMessageKind};
#[cfg(unix)]
use agentdictate_runtime::IpcClient;
use agentdictate_runtime::{IpcHandler, IpcServer};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
static REQUEST_ID: AtomicU64 = AtomicU64::new(1);

fn main() -> anyhow::Result<()> {
    let argument = std::env::args().nth(1);
    let paths = AppPaths::from_environment()?;
    if is_copilot_argument(argument.as_deref()) {
        let _log_guard = init_file_logging(&paths.logs, "copilot.log")?;
        #[cfg(windows)]
        return agentdictate_app::run_copilot();
        #[cfg(not(windows))]
        anyhow::bail!("the meeting copilot is available on Windows");
    }
    let _log_guard = init_file_logging(&paths.logs, "agentdictated.log")?;
    if is_overlay_helper_argument(argument.as_deref()) {
        tracing::info!("transient recording overlay starting");
        return run_overlay_helper();
    }
    if argument.as_deref() != Some(SERVICE_ARGUMENT) {
        if argument.is_some() && argument.as_deref() != Some(START_SERVICE_ARGUMENT) {
            anyhow::bail!(
                "unknown agentdictated argument: {}",
                argument.as_deref().unwrap_or_default()
            )
        }
        tracing::info!("daemon service bootstrap starting");
        let executable = std::env::current_exe()?;
        return bootstrap_daemon_service(&paths.runtime, &paths.daemon_service_file, &executable);
    }
    run_daemon(paths)
}

fn run_daemon(paths: AppPaths) -> anyhow::Result<()> {
    tracing::info!("native daemon starting");
    let runtime = paths.runtime.clone();
    let server = IpcServer::bind(&paths.runtime)?;
    let mut process = AgentProcess::open(paths)?;
    let overlay_presenter = match std::env::current_exe()
        .map_err(anyhow::Error::from)
        .and_then(|executable| start_overlay_presenter(executable).map_err(anyhow::Error::from))
    {
        Ok((controller, thread)) => {
            controller
                .notify_health_changes_at(runtime.join(agentdictate_app::OVERLAY_HEALTH_FILE));
            process.set_overlay_controller(controller);
            Some(thread)
        }
        Err(error) => {
            tracing::warn!(%error, "recording overlay is unavailable; dictation will continue");
            None
        }
    };
    start_hotkey_listener(&mut process, &runtime)?;
    let _maintenance_thread = match process.start_post_listener_maintenance() {
        Ok(thread) => Some(thread),
        Err(error) => {
            tracing::warn!(%error, "could not start nonessential maintenance");
            None
        }
    };
    let _chatgpt_dictation_importer = match process.start_chatgpt_dictation_importer() {
        Ok(thread) => Some(thread),
        Err(error) => {
            tracing::warn!(%error, "could not start ChatGPT dictation usage importer");
            None
        }
    };
    let show_tray_icon = process.show_tray_icon();
    let quitting = Arc::new(AtomicBool::new(false));
    let process = Arc::new(Mutex::new(QuitSignallingHandler {
        process,
        quitting: Arc::clone(&quitting),
        runtime: runtime.clone(),
    }));
    let ipc_thread = std::thread::Builder::new()
        .name("agentdictate-ipc".into())
        .spawn(move || {
            loop {
                // Never lock the process here: a long command (a
                // transcription) holds it, and no client could connect.
                if quitting.load(AtomicOrdering::Acquire) {
                    break;
                }
                if process.is_poisoned() {
                    tracing::error!("daemon process lock is poisoned");
                    break;
                }
                if let Err(error) = server.serve_next_concurrent(Arc::clone(&process)) {
                    tracing::warn!(%error, "could not accept IPC session");
                }
            }
        })?;
    if let Err(error) = start_signal_listener(runtime.clone()) {
        tracing::warn!(%error, "signal listener is unavailable; daemon will continue");
    }
    // Opens the meeting copilot when a call starts, if the user asked for it.
    #[cfg(windows)]
    agentdictate_app::watch_for_meetings();
    let _tray_handle = if show_tray_icon {
        match settings_executable_for_current_process() {
            Ok(executable) => match start_system_tray(runtime, executable) {
                Ok(handle) => Some(handle),
                Err(error) => {
                    tracing::warn!(%error, "native tray is unavailable; daemon will continue");
                    None
                }
            },
            Err(error) => {
                tracing::warn!(%error, "settings launcher is unavailable; tray will stay hidden");
                None
            }
        }
    } else {
        None
    };
    ipc_thread
        .join()
        .map_err(|_| anyhow::anyhow!("daemon IPC thread panicked"))?;
    if let Some(thread) = overlay_presenter {
        thread
            .join()
            .map_err(|_| anyhow::anyhow!("overlay presenter thread panicked"))?;
    }
    Ok(())
}

/// Serves daemon IPC and raises `quitting` once a command made the process
/// quit, so the accept loop checks for quit without the process lock.
struct QuitSignallingHandler {
    process: AgentProcess,
    quitting: Arc<AtomicBool>,
    runtime: std::path::PathBuf,
}

impl IpcHandler for QuitSignallingHandler {
    fn snapshot(&self, request_id: u64) -> agentdictate_core::ServerMessage {
        self.process.snapshot(request_id)
    }

    fn handle(
        &mut self,
        command: agentdictate_core::ClientCommand,
    ) -> agentdictate_core::ServerMessage {
        let response = self.process.handle(command);
        if self.process.should_quit() && !self.quitting.swap(true, AtomicOrdering::AcqRel) {
            // The process already woke the accept loop, possibly before this
            // flag was raised; wake it again so it is sure to see the flag.
            let _ = agentdictate_runtime::IpcClient::wake(&self.runtime);
        }
        response
    }
}

#[cfg(unix)]
fn start_signal_listener(runtime: std::path::PathBuf) -> anyhow::Result<()> {
    let mut signals = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
    ])?;
    let request_shutdown = move || -> anyhow::Result<()> {
        let (mut client, _) = IpcClient::connect(&runtime)?;
        let request_id = REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let response = client.send(ClientCommand::quit(request_id))?;
        if let ServerMessageKind::CommandRejected { error, .. } = response.kind {
            anyhow::bail!(error)
        }
        Ok(())
    };
    std::thread::Builder::new()
        .name("agentdictate-signals".into())
        .spawn(move || {
            if signals.forever().next().is_some()
                && let Err(error) = request_shutdown()
            {
                tracing::error!(%error, "graceful shutdown request failed");
            }
        })?;
    Ok(())
}

#[cfg(windows)]
fn start_signal_listener(_runtime: std::path::PathBuf) -> anyhow::Result<()> {
    Ok(())
}
