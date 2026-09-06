//! AgentDictate process composition and production adapters.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{io, path::PathBuf};

mod captured_audio;
mod chatgpt_dictation_import;
mod codex_subscription;
mod daemon;
pub mod diagnostics;
mod hotkey_dispatch;
mod live_transcription;
mod model_catalog;
mod openai;
mod overlay_process;
mod process;
#[cfg_attr(windows, path = "windows/startup.rs")]
mod startup;
#[cfg_attr(windows, path = "windows/system.rs")]
mod system;
#[cfg_attr(windows, path = "windows/tray.rs")]
mod tray;
mod workspace;

pub use codex_subscription::CodexSubscriptionTransport;
#[cfg(windows)]
pub use codex_subscription::sign_in_with_chatgpt;
pub use daemon::{CapturedRecording, Daemon, DaemonError, RecordingController};
pub use diagnostics::init_file_logging;
pub use hotkey_dispatch::{
    HotkeyActionOutcome, HotkeyDispatchGate, HotkeyIgnoreReason, start_hotkey_listener,
};
pub use openai::{
    CleanupRequest, CleanupTransport, ReqwestOpenAiTransport, SpeechRouter, SpeechTransport,
    TranscriptionPipeline, TranscriptionRequest,
};
#[cfg(feature = "desktop")]
pub use overlay_process::run_overlay_helper;
pub use overlay_process::{
    ActiveRecordingUpdate, OVERLAY_HEALTH_FILE, OVERLAY_TEARDOWN_TIMEOUT, OverlayController,
    OverlayProcessAction, OverlayProcessState, OverlayTeardownError, OverlayUpdate,
    is_overlay_helper_argument, start_overlay_presenter, start_overlay_presenter_with_timeout,
};
pub use process::{AgentProcess, HotkeyReconfigurer, ProductionDaemon, command_for_hotkey};
pub use startup::{
    DAEMON_SERVICE_NAME, SERVICE_ARGUMENT, START_SERVICE_ARGUMENT, bootstrap_daemon_service,
    sync_startup_command, sync_startup_with_systemctl,
};
pub use system::{SystemDeliverer, SystemRecordingController};
pub use tray::{
    SystemTrayHandle, TrayAction, settings_executable_for_current_process, start_system_tray,
    tray_command_for_phase,
};
pub use workspace::{WorkspaceClient, WorkspaceError, workspace_view_model};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppPaths {
    pub config_file: PathBuf,
    pub autostart_file: PathBuf,
    pub daemon_service_file: PathBuf,
    pub database_file: PathBuf,
    pub recordings: PathBuf,
    pub logs: PathBuf,
    pub cache: PathBuf,
    pub runtime: PathBuf,
}

impl AppPaths {
    #[must_use]
    pub fn model_catalog_cache_file(&self) -> PathBuf {
        self.cache.join("model-catalog.json")
    }

    #[cfg(unix)]
    pub fn from_environment() -> io::Result<Self> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
        let xdg =
            |name: &str, fallback: PathBuf| std::env::var_os(name).map_or(fallback, PathBuf::from);
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                // SAFETY: `geteuid` has no preconditions and does not access
                // caller-provided memory.
                let effective_user = unsafe { libc::geteuid() };
                PathBuf::from(format!("/run/user/{effective_user}"))
            });
        Ok(Self::from_roots(
            xdg("XDG_CONFIG_HOME", home.join(".config")),
            xdg("XDG_DATA_HOME", home.join(".local/share")),
            xdg("XDG_STATE_HOME", home.join(".local/state")),
            xdg("XDG_CACHE_HOME", home.join(".cache")),
            runtime,
        ))
    }

    #[cfg(windows)]
    pub fn from_environment() -> io::Result<Self> {
        let local = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "LOCALAPPDATA is not set"))?;
        let root = std::env::var_os("AGENTDICTATE_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| local.join("AgentDictate"));
        Ok(Self {
            config_file: root.join("config.json"),
            autostart_file: root.join("startup.json"),
            daemon_service_file: root.join("daemon.json"),
            database_file: root.join("agentdictate.sqlite"),
            recordings: root.join("recordings"),
            logs: root.join("logs"),
            cache: root.join("cache"),
            runtime: root.join("runtime"),
        })
    }

    pub fn ensure_directories(&self) -> io::Result<()> {
        for directory in [
            self.config_file.parent(),
            self.database_file.parent(),
            Some(self.recordings.as_path()),
            Some(self.logs.as_path()),
            Some(self.cache.as_path()),
            Some(self.runtime.as_path()),
        ]
        .into_iter()
        .flatten()
        {
            std::fs::create_dir_all(directory)?;
            #[cfg(windows)]
            agentdictate_runtime::restrict_path(directory)?;
            #[cfg(unix)]
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    #[must_use]
    pub fn from_roots(
        config_root: impl Into<PathBuf>,
        data_root: impl Into<PathBuf>,
        state_root: impl Into<PathBuf>,
        cache_root: impl Into<PathBuf>,
        runtime_root: impl Into<PathBuf>,
    ) -> Self {
        let config_root = config_root.into();
        let config = config_root.join("agentdictate");
        let data_root = data_root.into();
        let data = data_root.join("agentdictate");
        let state = state_root.into().join("agentdictate");
        let cache = cache_root.into().join("agentdictate");
        let runtime = runtime_root.into().join("agentdictate");
        Self {
            config_file: config.join("config.json"),
            autostart_file: config_root.join("autostart/local.agentdictate.AgentDictate.desktop"),
            daemon_service_file: data_root.join("systemd/user/agentdictated.service"),
            database_file: data.join("agentdictate.sqlite"),
            recordings: data.join("recordings"),
            logs: state.join("logs"),
            cache,
            runtime,
        }
    }
}
