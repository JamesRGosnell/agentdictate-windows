//! Desktop signals for meeting detection.

use windows_sys::Win32::{Foundation::*, System::Registry::*, UI::WindowsAndMessaging::*};

const CONSENT_STORE: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\CapabilityAccessManager\\ConsentStore\\microphone";

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

unsafe fn subkeys(key: HKEY) -> Vec<String> {
    let mut names = Vec::new();
    let mut index = 0;
    loop {
        let mut buffer = [0_u16; 512];
        let mut length = buffer.len() as u32;
        let status = unsafe {
            RegEnumKeyExW(
                key,
                index,
                buffer.as_mut_ptr(),
                &mut length,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if status != ERROR_SUCCESS {
            break;
        }
        names.push(String::from_utf16_lossy(&buffer[..length as usize]));
        index += 1;
    }
    names
}

unsafe fn qword(key: HKEY, subkey: &str, value: &str) -> Option<u64> {
    let subkey = wide(subkey);
    let value = wide(value);
    let mut data = 0_u64;
    let mut size = std::mem::size_of::<u64>() as u32;
    let status = unsafe {
        RegGetValueW(
            key,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_QWORD,
            std::ptr::null_mut(),
            (&raw mut data).cast(),
            &mut size,
        )
    };
    (status == ERROR_SUCCESS).then_some(data)
}

unsafe fn open(parent: HKEY, path: &str) -> Option<HKEY> {
    let path = wide(path);
    let mut key: HKEY = std::ptr::null_mut();
    let status = unsafe { RegOpenKeyExW(parent, path.as_ptr(), 0, KEY_READ, &mut key) };
    (status == ERROR_SUCCESS).then_some(key)
}

/// Apps whose microphone capture is in progress right now: those Windows'
/// privacy records show as recording, plus processes with an active capture
/// session. The records miss some apps (Discord builds go unrecorded after
/// updates), so live sessions are checked as well.
pub(crate) fn active_microphone_apps() -> Vec<String> {
    let mut active = consent_store_apps();
    for app in capture_session_apps() {
        if !active.contains(&app) {
            active.push(app);
        }
    }
    active
}

/// Executable paths of processes with an active session on any microphone.
/// Runs on its own thread so the caller's COM apartment does not matter.
fn capture_session_apps() -> Vec<String> {
    std::thread::Builder::new()
        .name("meeting-capture-sessions".into())
        .spawn(|| {
            let mut paths = Vec::new();
            for process_id in capture_session_process_ids().unwrap_or_default() {
                if let Some(path) = process_path(process_id)
                    && !paths.contains(&path)
                {
                    paths.push(path);
                }
            }
            paths
        })
        .ok()
        .and_then(|thread| thread.join().ok())
        .unwrap_or_default()
}

fn capture_session_process_ids() -> windows::core::Result<Vec<u32>> {
    use windows::{
        Win32::{
            Media::Audio::{
                AudioSessionStateActive, DEVICE_STATE_ACTIVE, IAudioSessionControl2,
                IAudioSessionManager2, IMMDeviceEnumerator, MMDeviceEnumerator, eCapture,
            },
            System::Com::{
                CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
            },
        },
        core::Interface,
    };

    let mut ids = Vec::new();
    unsafe {
        let initialized = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
        let result = (|| {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let devices = enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)?;
            for index in 0..devices.GetCount()? {
                let Ok(device) = devices.Item(index) else {
                    continue;
                };
                let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
                    continue;
                };
                let Ok(sessions) = manager.GetSessionEnumerator() else {
                    continue;
                };
                for session in 0..sessions.GetCount().unwrap_or(0) {
                    let Ok(control) = sessions.GetSession(session) else {
                        continue;
                    };
                    if control.GetState().ok() != Some(AudioSessionStateActive) {
                        continue;
                    }
                    if let Ok(control) = control.cast::<IAudioSessionControl2>()
                        && let Ok(id) = control.GetProcessId()
                        && id != 0
                        && !ids.contains(&id)
                    {
                        ids.push(id);
                    }
                }
            }
            Ok(())
        })();
        if initialized {
            CoUninitialize();
        }
        result.map(|()| ids)
    }
}

fn process_path(process_id: u32) -> Option<String> {
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id);
        if process.is_null() {
            return None;
        }
        let mut buffer = [0_u16; 1024];
        let mut length = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            buffer.as_mut_ptr(),
            &mut length,
        ) != 0;
        CloseHandle(process);
        ok.then(|| String::from_utf16_lossy(&buffer[..length as usize]))
    }
}

fn consent_store_apps() -> Vec<String> {
    let mut active = Vec::new();
    unsafe {
        let Some(store) = open(HKEY_CURRENT_USER, CONSENT_STORE) else {
            return active;
        };
        let mut check = |key: HKEY, name: &str| {
            let start = qword(key, name, "LastUsedTimeStart").unwrap_or(0);
            let stop = qword(key, name, "LastUsedTimeStop").unwrap_or(1);
            if start != 0 && stop == 0 {
                active.push(name.to_owned());
            }
        };
        for name in subkeys(store) {
            if name == "NonPackaged" {
                if let Some(non_packaged) = open(store, "NonPackaged") {
                    for app in subkeys(non_packaged) {
                        check(non_packaged, &app);
                    }
                    RegCloseKey(non_packaged);
                }
            } else {
                check(store, &name);
            }
        }
        RegCloseKey(store);
    }
    active
}

unsafe extern "system" fn collect_title(hwnd: HWND, state: LPARAM) -> windows_sys::core::BOOL {
    unsafe {
        let titles = &mut *(state as *mut Vec<String>);
        if IsWindowVisible(hwnd) == 0 {
            return 1;
        }
        let length = GetWindowTextLengthW(hwnd);
        if length <= 0 {
            return 1;
        }
        let mut buffer = vec![0_u16; length as usize + 1];
        let copied = GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32);
        if copied > 0 {
            titles.push(String::from_utf16_lossy(&buffer[..copied as usize]));
        }
        1
    }
}

/// Titles of visible top-level windows.
pub(crate) fn window_titles() -> Vec<String> {
    let mut titles: Vec<String> = Vec::new();
    unsafe {
        EnumWindows(Some(collect_title), (&raw mut titles) as LPARAM);
    }
    titles
}
