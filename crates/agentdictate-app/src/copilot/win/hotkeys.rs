//! Global shortcuts registered only while the copilot runs.

use std::sync::mpsc;

use agentdictate_core::CopilotShortcuts;
use windows_sys::Win32::{
    System::Threading::GetCurrentThreadId,
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

use crate::copilot::engine::HotkeyCommand;

struct Candidate {
    modifiers: HOT_KEY_MODIFIERS,
    key: VIRTUAL_KEY,
    labels: &'static [&'static str],
}

const ASSIST: [Candidate; 2] = [
    Candidate {
        modifiers: MOD_CONTROL,
        key: VK_RETURN,
        labels: &["Ctrl", "\u{21b5}"],
    },
    Candidate {
        modifiers: MOD_CONTROL | MOD_ALT,
        key: VK_RETURN,
        labels: &["Ctrl", "Alt", "\u{21b5}"],
    },
];
const TOGGLE: [Candidate; 2] = [
    Candidate {
        modifiers: MOD_CONTROL,
        key: VK_OEM_5,
        labels: &["Ctrl", "\\"],
    },
    Candidate {
        modifiers: MOD_CONTROL | MOD_ALT,
        key: VK_OEM_5,
        labels: &["Ctrl", "Alt", "\\"],
    },
];
const ASK: [Candidate; 1] = [Candidate {
    modifiers: MOD_CONTROL | MOD_SHIFT,
    key: VK_RETURN,
    labels: &["Ctrl", "Shift", "\u{21b5}"],
}];

/// Owns the hotkey thread; dropping it unregisters every shortcut.
pub(crate) struct CopilotHotkeys {
    thread_id: u32,
}

impl Drop for CopilotHotkeys {
    fn drop(&mut self) {
        unsafe {
            PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0);
        }
    }
}

/// Registers the copilot shortcuts and reports which ones are active.
pub(crate) fn start(
    on_command: impl Fn(HotkeyCommand) + Send + 'static,
) -> Result<(CopilotHotkeys, CopilotShortcuts), String> {
    let (ready, registered) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("copilot-hotkeys".into())
        .spawn(move || unsafe {
            let mut message: MSG = std::mem::zeroed();
            PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);
            let mut shortcuts = CopilotShortcuts {
                assist: Vec::new(),
                toggle: Vec::new(),
                ask: Vec::new(),
            };
            for (id, candidates) in [(1, &ASSIST[..]), (2, &TOGGLE[..]), (3, &ASK[..])] {
                for candidate in candidates {
                    if RegisterHotKey(
                        std::ptr::null_mut(),
                        id,
                        candidate.modifiers | MOD_NOREPEAT,
                        u32::from(candidate.key),
                    ) != 0
                    {
                        let labels = candidate
                            .labels
                            .iter()
                            .map(|label| (*label).to_owned())
                            .collect();
                        match id {
                            1 => shortcuts.assist = labels,
                            2 => shortcuts.toggle = labels,
                            _ => shortcuts.ask = labels,
                        }
                        break;
                    }
                    tracing::warn!(id, "copilot shortcut is already in use; trying fallback");
                }
            }
            let _ = ready.send((GetCurrentThreadId(), shortcuts));
            while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
                if message.message == WM_HOTKEY {
                    match message.wParam {
                        1 => on_command(HotkeyCommand::Assist),
                        2 => on_command(HotkeyCommand::Toggle),
                        3 => on_command(HotkeyCommand::Ask),
                        _ => {}
                    }
                }
            }
            for id in 1..=3 {
                UnregisterHotKey(std::ptr::null_mut(), id);
            }
        })
        .map_err(|error| error.to_string())?;
    let (thread_id, shortcuts) = registered
        .recv()
        .map_err(|_| "copilot shortcuts could not start".to_owned())?;
    Ok((CopilotHotkeys { thread_id }, shortcuts))
}
