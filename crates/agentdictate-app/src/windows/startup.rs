use agentdictate_runtime::IpcClient;
use std::{
    io,
    os::windows::process::CommandExt,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use windows_sys::Win32::{Foundation::*, System::Registry::*};
pub const DAEMON_SERVICE_NAME: &str = "AgentDictate";
pub const START_SERVICE_ARGUMENT: &str = "--start-service";
pub const SERVICE_ARGUMENT: &str = "--service";
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
pub fn sync_startup_with_systemctl(
    entry: &Path,
    service: &Path,
    enabled: bool,
    daemon: &Path,
    _systemctl: &Path,
) -> io::Result<()> {
    sync_startup_command(
        entry,
        service,
        enabled,
        daemon,
        &[SERVICE_ARGUMENT.into()],
        daemon,
        &[SERVICE_ARGUMENT.into()],
        _systemctl,
    )
}
#[allow(clippy::too_many_arguments)]
pub fn sync_startup_command(
    entry: &Path,
    _service: &Path,
    enabled: bool,
    _daemon: &Path,
    _daemon_args: &[String],
    bootstrap: &Path,
    arguments: &[String],
    _systemctl: &Path,
) -> io::Result<()> {
    let command = std::iter::once(bootstrap.as_os_str().to_string_lossy().into_owned())
        .chain(arguments.iter().cloned())
        .map(|arg| quote_argument(&arg))
        .collect::<Vec<_>>()
        .join(" ");
    unsafe {
        let mut key = std::ptr::null_mut();
        let status = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            wide(r"Software\Microsoft\Windows\CurrentVersion\Run").as_ptr(),
            0,
            std::ptr::null(),
            0,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        );
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let value = wide(&command);
        let status = if enabled {
            RegSetValueExW(
                key,
                wide("AgentDictate").as_ptr(),
                0,
                REG_SZ,
                value.as_ptr().cast(),
                (value.len() * 2) as u32,
            )
        } else {
            RegDeleteValueW(key, wide("AgentDictate").as_ptr())
        };
        RegCloseKey(key);
        if status != 0 && (status != ERROR_FILE_NOT_FOUND || enabled) {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
    }
    agentdictate_runtime::write_atomic(entry, serde_json::to_string(&enabled)?.as_bytes(), 0o600)
}
fn quote_argument(arg: &str) -> String {
    let mut result = String::from("\"");
    let mut slashes = 0;
    for c in arg.chars() {
        if c == '\\' {
            slashes += 1;
            continue;
        }
        if c == '"' {
            result.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
        } else {
            result.extend(std::iter::repeat_n('\\', slashes));
        }
        slashes = 0;
        result.push(c);
    }
    result.extend(std::iter::repeat_n('\\', slashes * 2));
    result.push('"');
    result
}
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
/// Starts the daemon outside the launcher's job object. A terminal or IDE
/// often runs its children in a kill-on-close job, so a daemon left inside
/// it dies when that session ends. A job that forbids breakaway rejects the
/// flag with ERROR_ACCESS_DENIED; the daemon then starts inside the job.
fn spawn_outside_job<T>(mut spawn: impl FnMut(u32) -> io::Result<T>) -> io::Result<T> {
    match spawn(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB) {
        Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
            tracing::warn!(
                "cannot start the daemon outside this job; it will end with its session"
            );
            spawn(CREATE_NO_WINDOW)
        }
        result => result,
    }
}
pub fn bootstrap_daemon_service(
    runtime: &Path,
    _service: &Path,
    executable: &Path,
) -> anyhow::Result<()> {
    if IpcClient::connect(runtime).is_ok() {
        return Ok(());
    }
    let mut child = spawn_outside_job(|flags| {
        Command::new(executable)
            .arg(SERVICE_ARGUMENT)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(flags)
            .spawn()
    })?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if IpcClient::connect(runtime).is_ok() {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            anyhow::bail!("AgentDictate daemon exited during startup: {status}; see its log");
        }
        if Instant::now() >= deadline {
            anyhow::bail!("AgentDictate daemon did not become ready; see its log");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn startup_paths_are_quoted() {
        assert_eq!(
            quote_argument(r"C:\My Apps\agentdictated.exe"),
            r#""C:\My Apps\agentdictated.exe""#
        );
        assert_eq!(quote_argument("a\"b"), "\"a\\\"b\"");
    }
    #[test]
    fn the_daemon_breaks_away_from_the_launcher_job_when_allowed() {
        let mut attempts = Vec::new();
        let flags = spawn_outside_job(|flags| {
            attempts.push(flags);
            Ok(flags)
        })
        .unwrap();
        assert_eq!(flags, CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB);
        assert_eq!(attempts.len(), 1);
    }
    #[test]
    fn a_job_that_forbids_breakaway_still_starts_the_daemon() {
        let mut attempts = Vec::new();
        let flags = spawn_outside_job(|flags| {
            attempts.push(flags);
            if flags & CREATE_BREAKAWAY_FROM_JOB != 0 {
                Err(io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32))
            } else {
                Ok(flags)
            }
        })
        .unwrap();
        assert_eq!(flags, CREATE_NO_WINDOW);
        assert_eq!(
            attempts,
            [
                CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB,
                CREATE_NO_WINDOW
            ]
        );
    }
    #[test]
    fn other_spawn_failures_are_not_retried() {
        let mut attempts = 0;
        let error = spawn_outside_job::<()>(|_| {
            attempts += 1;
            Err(io::Error::from_raw_os_error(ERROR_FILE_NOT_FOUND as i32))
        })
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(ERROR_FILE_NOT_FOUND as i32));
        assert_eq!(attempts, 1);
    }
}
