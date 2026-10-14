//! The copilot's notification-area icon. A session that is listening always
//! shows here, even while its window is hidden: a click shows or hides the
//! window, and the menu ends the session.

use std::{cell::RefCell, sync::mpsc};

use windows_sys::Win32::{
    Foundation::*,
    System::LibraryLoader::GetModuleHandleW,
    UI::{Shell::*, WindowsAndMessaging::*},
};

/// What the user asked for from the icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TrayCommand {
    ShowOrHide,
    End,
}

type CommandSink = Box<dyn Fn(TrayCommand)>;

thread_local! {
    static SINK: RefCell<Option<CommandSink>> = const { RefCell::new(None) };
    static ICON: RefCell<Option<NOTIFYICONDATAW>> = const { RefCell::new(None) };
}

const CALLBACK: u32 = WM_APP + 1;
const MENU_SHOW: usize = 1;
const MENU_END: usize = 2;

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn send(command: TrayCommand) {
    SINK.with(|slot| {
        if let Some(sink) = slot.borrow().as_ref() {
            sink(command);
        }
    });
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        // Explorer restarted: put the icon back.
        if message == RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) {
            ICON.with(|slot| {
                if let Some(icon) = slot.borrow().as_ref() {
                    Shell_NotifyIconW(NIM_ADD, icon);
                }
            });
            return 0;
        }
        if message == CALLBACK {
            match lparam as u32 {
                WM_LBUTTONUP => send(TrayCommand::ShowOrHide),
                WM_RBUTTONUP | WM_CONTEXTMENU => {
                    let menu = CreatePopupMenu();
                    AppendMenuW(
                        menu,
                        MF_STRING,
                        MENU_SHOW,
                        wide("Show or hide the copilot").as_ptr(),
                    );
                    AppendMenuW(
                        menu,
                        MF_STRING,
                        MENU_END,
                        wide("End session and save notes").as_ptr(),
                    );
                    let mut point = std::mem::zeroed();
                    GetCursorPos(&mut point);
                    SetForegroundWindow(hwnd);
                    let selected = TrackPopupMenu(
                        menu,
                        TPM_RETURNCMD | TPM_NONOTIFY,
                        point.x,
                        point.y,
                        0,
                        hwnd,
                        std::ptr::null(),
                    );
                    DestroyMenu(menu);
                    PostMessageW(hwnd, WM_NULL, 0, 0);
                    match selected as usize {
                        MENU_SHOW => send(TrayCommand::ShowOrHide),
                        MENU_END => send(TrayCommand::End),
                        _ => {}
                    }
                }
                _ => {}
            }
            return 0;
        }
        if message == WM_CLOSE {
            DestroyWindow(hwnd);
            return 0;
        }
        if message == WM_DESTROY {
            PostQuitMessage(0);
            return 0;
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

/// Owns the icon; dropping it removes the icon.
pub(crate) struct CopilotTray {
    window: usize,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for CopilotTray {
    fn drop(&mut self) {
        unsafe {
            PostMessageW(self.window as HWND, WM_CLOSE, 0, 0);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Shows the icon until the returned handle is dropped.
pub(crate) fn start(
    on_command: impl Fn(TrayCommand) + Send + 'static,
) -> std::io::Result<CopilotTray> {
    let (ready, initialized) = mpsc::sync_channel(1);
    let thread = std::thread::Builder::new()
        .name("copilot-tray".into())
        .spawn(move || unsafe {
            SINK.with(|slot| *slot.borrow_mut() = Some(Box::new(on_command)));
            let class = wide("AgentDictate.CopilotTray");
            let instance = GetModuleHandleW(std::ptr::null());
            let mut class_info: WNDCLASSW = std::mem::zeroed();
            class_info.lpfnWndProc = Some(window_proc);
            class_info.hInstance = instance;
            class_info.lpszClassName = class.as_ptr();
            RegisterClassW(&class_info);
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                wide("AgentDictate Copilot Tray").as_ptr(),
                0,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                let _ = ready.send(Err(std::io::Error::last_os_error()));
                return;
            }
            let mut icon: NOTIFYICONDATAW = std::mem::zeroed();
            icon.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            icon.hWnd = hwnd;
            icon.uID = 1;
            icon.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
            icon.uCallbackMessage = CALLBACK;
            // MAKEINTRESOURCEW(1): the application icon embedded in the executable.
            icon.hIcon = LoadIconW(instance, std::ptr::without_provenance::<u16>(1));
            let tip = wide("AgentDictate meeting copilot is listening. Click to show or hide.");
            let length = tip.len().min(icon.szTip.len());
            icon.szTip[..length].copy_from_slice(&tip[..length]);
            if Shell_NotifyIconW(NIM_ADD, &icon) == 0 {
                let _ = ready.send(Err(std::io::Error::last_os_error()));
                DestroyWindow(hwnd);
                return;
            }
            ICON.with(|slot| *slot.borrow_mut() = Some(icon));
            let _ = ready.send(Ok(hwnd as usize));
            let mut message = std::mem::zeroed();
            while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            Shell_NotifyIconW(NIM_DELETE, &icon);
            ICON.with(|slot| *slot.borrow_mut() = None);
            SINK.with(|slot| *slot.borrow_mut() = None);
        })?;
    let window = initialized.recv().map_err(std::io::Error::other)??;
    Ok(CopilotTray {
        window,
        thread: Some(thread),
    })
}
