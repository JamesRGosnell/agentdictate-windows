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
pub fn bootstrap_daemon_service(
    runtime: &Path,
    _service: &Path,
    executable: &Path,
) -> anyhow::Result<()> {
    if IpcClient::connect(runtime).is_ok() {
        return Ok(());
    }
    let mut child = Command::new(executable)
        .arg(SERVICE_ARGUMENT)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x08000000)
        .spawn()?;
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
}
