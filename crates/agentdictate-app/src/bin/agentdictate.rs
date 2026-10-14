#![cfg_attr(windows, windows_subsystem = "windows")]
use std::sync::Arc;

use agentdictate_app::{
    AppPaths, WorkspaceClient, WorkspaceError, bootstrap_daemon_service, init_file_logging,
};
use agentdictate_core::{ClientCommand, ServerMessageKind};
use agentdictate_runtime::IpcClient;
use agentdictate_ui::{
    Route, ShellViewModel, UiActionError, run_settings_shell_with_workspace_actions,
    run_settings_shell_with_workspace_actions_and_updates,
};

fn main() {
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(u32::MAX);
        let id: Vec<u16> = "local.agentdictate.AgentDictate"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(id.as_ptr());
    }
    if let Err(error) = run() {
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::{
                System::Console::GetConsoleWindow, UI::WindowsAndMessaging::*,
            };
            if GetConsoleWindow().is_null()
                && (std::env::args().len() == 1
                    || matches!(
                        std::env::args().nth(1).as_deref(),
                        Some("login" | "--route")
                    ))
            {
                let message: Vec<u16> =
                    format!("{error:#}").encode_utf16().chain(Some(0)).collect();
                let title: Vec<u16> = "AgentDictate".encode_utf16().chain(Some(0)).collect();
                MessageBoxW(
                    std::ptr::null_mut(),
                    message.as_ptr(),
                    title.as_ptr(),
                    MB_OK | MB_ICONERROR,
                );
            }
        }
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let paths = AppPaths::from_environment()?;
    let args: Vec<_> = std::env::args().skip(1).collect();
    #[cfg(windows)]
    if args.as_slice() == ["login"] {
        return agentdictate_app::sign_in_with_chatgpt();
    }
    #[cfg(windows)]
    if args.first().map(String::as_str) == Some("copilot") {
        return match args.get(1).map(String::as_str) {
            None => agentdictate_app::launch_copilot().map_err(Into::into),
            Some("--check") => {
                let latency = args.iter().any(|arg| arg == "--latency");
                let speech = args
                    .iter()
                    .skip(2)
                    .find(|arg| !arg.starts_with("--"))
                    .map(std::path::PathBuf::from);
                let failures = agentdictate_app::run_copilot_check(speech.as_deref(), latency)?;
                anyhow::ensure!(failures == 0, "{failures} copilot check(s) failed");
                Ok(())
            }
            Some("--echo-test") => {
                let speech = args.get(2).ok_or_else(|| {
                    anyhow::anyhow!("usage: agentdictate copilot --echo-test speech.wav")
                })?;
                let failures =
                    agentdictate_app::run_copilot_echo_test(std::path::Path::new(speech))?;
                anyhow::ensure!(failures == 0, "{failures} echo check(s) failed");
                Ok(())
            }
            Some("--voices") => {
                let paths: Vec<std::path::PathBuf> =
                    args.iter().skip(2).map(std::path::PathBuf::from).collect();
                anyhow::ensure!(
                    !paths.is_empty(),
                    "usage: agentdictate copilot --voices recording.wav..."
                );
                agentdictate_app::run_copilot_voices(&paths)?;
                Ok(())
            }
            Some("--names") => {
                let failures = agentdictate_app::run_copilot_names()?;
                anyhow::ensure!(failures == 0, "{failures} naming check(s) failed");
                Ok(())
            }
            Some("--rewrites") => {
                let failures =
                    agentdictate_app::run_copilot_rewrites(args.get(2).map(String::as_str))?;
                anyhow::ensure!(failures == 0, "{failures} rewrite check(s) failed");
                Ok(())
            }
            Some("--window") => {
                let image = args.get(2).map(std::path::PathBuf::from);
                let failures = agentdictate_app::run_copilot_window(image.as_deref())?;
                anyhow::ensure!(failures == 0, "the meeting window could not be read");
                Ok(())
            }
            Some(other) => anyhow::bail!("unknown copilot option: {other}"),
        };
    }
    // `--route meetings` opens the settings window on a page, as the tray's
    // Meeting notes item does.
    let mut route = Route::Overview;
    let args = match args.as_slice() {
        [flag, slug] if flag == "--route" => {
            route = slug.parse().map_err(anyhow::Error::msg)?;
            Vec::new()
        }
        _ => args,
    };
    if !args.is_empty() {
        let command = match args.as_slice() {
            [command] if command == "quit" => ClientCommand::quit(1),
            [command] if command == "status" => ClientCommand::get_snapshot(1),
            [command] if command == "stop" => ClientCommand::stop_recording(1),
            [command] if command == "cancel" => ClientCommand::cancel(1),
            [command] if command == "start" => ClientCommand::start_recording(1),
            [command, flag, mode] if command == "start" && flag == "--mode" => {
                ClientCommand::start_recording_in_mode(1, mode.parse().map_err(anyhow::Error::msg)?)
            }
            _ => anyhow::bail!(
                "Usage: agentdictate [start [--mode dictate|literal] | stop | cancel | status | quit | login | copilot | --route <page>]"
            ),
        };
        let (mut client, _) = IpcClient::connect(&paths.runtime)?;
        let response = client.send(command)?;
        if args[0] == "status" {
            println!("{}", serde_json::to_string(&response)?);
        }
        if let ServerMessageKind::CommandRejected { error, .. } = response.kind {
            anyhow::bail!(error);
        }
        return Ok(());
    }
    let _log_guard = init_file_logging(&paths.logs, "agentdictate.log")?;
    tracing::info!("native settings window starting");
    let (mut bootstrap_client, initial) = connect_or_start_daemon(&paths)?;
    let workspace = bootstrap_client.send(ClientCommand::get_workspace(1))?;
    // UI actions use short-lived sessions so a closed settings window cannot
    // retain an unnecessary daemon connection.
    drop(bootstrap_client);
    let ServerMessageKind::Snapshot {
        snapshot, settings, ..
    } = initial.kind
    else {
        anyhow::bail!("AgentDictate daemon rejected its initial snapshot request")
    };
    let settings = *settings;
    let ServerMessageKind::Workspace { workspace, .. } = workspace.kind else {
        anyhow::bail!("AgentDictate daemon did not provide workspace data")
    };
    let runtime = paths.runtime.clone();
    let workspace_client = Arc::new(
        WorkspaceClient::new(runtime.clone(), *workspace).with_meetings_dir(paths.meetings_dir()),
    );
    let mut workspace_model = workspace_client.view_model()?;
    let workspace_updates = match workspace_client
        .watch_with_catalog(&paths.database_file, paths.model_catalog_cache_file())
    {
        Ok(updates) => {
            // The watcher is registered before this refresh. A database write
            // racing with window startup is therefore either observed by the
            // refresh or queued by inotify, rather than being silently lost.
            match workspace_client.refresh() {
                Ok(refreshed) => workspace_model = refreshed,
                Err(error) => tracing::warn!(%error, "initial live workspace refresh failed"),
            }
            Some(updates)
        }
        Err(error) => {
            tracing::warn!(%error, "live workspace updates are unavailable");
            None
        }
    };
    let workspace_action_sink = {
        let workspace_client = Arc::clone(&workspace_client);
        Arc::new(move |action| {
            workspace_client
                .perform(action)
                .map_err(|error| -> UiActionError { Box::new(error) })
        })
    };
    let command_sink = Arc::new(move |command| -> Result<(), UiActionError> {
        let (mut client, _) = IpcClient::connect(&runtime)
            .map_err(|error| -> UiActionError { Box::new(WorkspaceError::from(error)) })?;
        let response = client
            .send(command)
            .map_err(|error| -> UiActionError { Box::new(WorkspaceError::from(error)) })?;
        match response.kind {
            ServerMessageKind::CommandRejected { error, .. } => {
                Err(Box::new(WorkspaceError::CommandRejected { message: error }))
            }
            ServerMessageKind::Snapshot { .. }
            | ServerMessageKind::Workspace { .. }
            | ServerMessageKind::HistoryPage { .. } => Ok(()),
        }
    });
    let model = ShellViewModel::from_app_snapshot(route, snapshot).with_workspace(workspace_model);
    match workspace_updates {
        Some(updates) => run_settings_shell_with_workspace_actions_and_updates(
            model,
            settings.values,
            settings.has_api_key,
            command_sink,
            workspace_action_sink,
            updates,
        ),
        None => run_settings_shell_with_workspace_actions(
            model,
            settings.values,
            settings.has_api_key,
            command_sink,
            workspace_action_sink,
        ),
    }
    Ok(())
}

fn connect_or_start_daemon(
    paths: &AppPaths,
) -> anyhow::Result<(IpcClient, agentdictate_core::ServerMessage)> {
    let daemon = std::env::current_exe()?.with_file_name(if cfg!(windows) {
        "agentdictated.exe"
    } else {
        "agentdictated"
    });
    bootstrap_daemon_service(&paths.runtime, &paths.daemon_service_file, &daemon)?;
    IpcClient::connect(&paths.runtime).map_err(Into::into)
}
