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
    key: Key,
    labels: &'static [&'static str],
}

#[derive(Clone, Copy)]
enum Key {
    Fixed(VIRTUAL_KEY),
    /// Whichever key types `\` on the active keyboard layout.
    Backslash,
}

const ASSIST: [Candidate; 2] = [
    Candidate {
        modifiers: MOD_CONTROL,
        key: Key::Fixed(VK_RETURN),
        labels: &["Ctrl", "\u{21b5}"],
    },
    Candidate {
        modifiers: MOD_CONTROL | MOD_ALT,
        key: Key::Fixed(VK_RETURN),
        labels: &["Ctrl", "Alt", "\u{21b5}"],
    },
];
const TOGGLE: [Candidate; 2] = [
    Candidate {
        modifiers: MOD_CONTROL,
        key: Key::Backslash,
        labels: &["Ctrl", "\\"],
    },
    Candidate {
        modifiers: MOD_CONTROL | MOD_ALT,
        key: Key::Backslash,
        labels: &["Ctrl", "Alt", "\\"],
    },
];
const ASK: [Candidate; 1] = [Candidate {
    modifiers: MOD_CONTROL | MOD_SHIFT,
    key: Key::Fixed(VK_RETURN),
    labels: &["Ctrl", "Shift", "\u{21b5}"],
}];

const TOGGLE_ID: i32 = 2;
/// How often the foreground keyboard layout is checked for changes.
const LAYOUT_POLL_MS: u32 = 500;

/// Reads `VkKeyScanExW` output for `\`: the key, plus Shift when the layout
/// needs it (as on Canadian French keyboards). Layouts that need AltGr, or have
/// no backslash key, keep the US key position, since Ctrl+AltGr would collide
/// with typing the character.
fn backslash_key_from_scan(scan: i16) -> (HOT_KEY_MODIFIERS, VIRTUAL_KEY) {
    if scan == -1 {
        return (0, VK_OEM_5);
    }
    let key = (scan as u16) & 0xff;
    match ((scan as u16) >> 8) & 0xff {
        0 => (0, key),
        1 => (MOD_SHIFT, key),
        _ => (0, VK_OEM_5),
    }
}

/// The keyboard layout of the window the user is typing in.
fn foreground_layout() -> HKL {
    unsafe {
        let thread = GetWindowThreadProcessId(GetForegroundWindow(), std::ptr::null_mut());
        GetKeyboardLayout(thread)
    }
}

fn resolve(
    modifiers: HOT_KEY_MODIFIERS,
    key: Key,
    layout: HKL,
) -> (HOT_KEY_MODIFIERS, VIRTUAL_KEY) {
    match key {
        Key::Fixed(key) => (modifiers, key),
        Key::Backslash => {
            let (shift, key) =
                backslash_key_from_scan(unsafe { VkKeyScanExW(u16::from(b'\\'), layout) });
            (modifiers | shift, key)
        }
    }
}

fn register(id: i32, (modifiers, key): (HOT_KEY_MODIFIERS, VIRTUAL_KEY)) -> bool {
    unsafe {
        RegisterHotKey(
            std::ptr::null_mut(),
            id,
            modifiers | MOD_NOREPEAT,
            u32::from(key),
        ) != 0
    }
}

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
            let mut layout = foreground_layout();
            // The show/hide modifiers in use, re-registered on layout changes.
            let mut toggle_modifiers = None;
            for (id, candidates) in [(1, &ASSIST[..]), (TOGGLE_ID, &TOGGLE[..]), (3, &ASK[..])] {
                for candidate in candidates {
                    if register(id, resolve(candidate.modifiers, candidate.key, layout)) {
                        let labels = candidate
                            .labels
                            .iter()
                            .map(|label| (*label).to_owned())
                            .collect();
                        match id {
                            1 => shortcuts.assist = labels,
                            TOGGLE_ID => {
                                shortcuts.toggle = labels;
                                toggle_modifiers = Some(candidate.modifiers);
                            }
                            _ => shortcuts.ask = labels,
                        }
                        break;
                    }
                    tracing::warn!(id, "copilot shortcut is already in use; trying fallback");
                }
            }
            let _ = ready.send((GetCurrentThreadId(), shortcuts));
            let timer = SetTimer(std::ptr::null_mut(), 0, LAYOUT_POLL_MS, None);
            while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
                match message.message {
                    WM_HOTKEY => match message.wParam {
                        1 => on_command(HotkeyCommand::Assist),
                        2 => on_command(HotkeyCommand::Toggle),
                        3 => on_command(HotkeyCommand::Ask),
                        _ => {}
                    },
                    WM_TIMER => {
                        let current = foreground_layout();
                        if current == layout || current.is_null() {
                            continue;
                        }
                        let Some(modifiers) = toggle_modifiers else {
                            continue;
                        };
                        let before = resolve(modifiers, Key::Backslash, layout);
                        let after = resolve(modifiers, Key::Backslash, current);
                        layout = current;
                        if before == after {
                            continue;
                        }
                        UnregisterHotKey(std::ptr::null_mut(), TOGGLE_ID);
                        if register(TOGGLE_ID, after) {
                            tracing::info!(
                                key = after.1,
                                "show/hide follows the new keyboard layout"
                            );
                        } else if !register(TOGGLE_ID, before) {
                            tracing::warn!("show/hide shortcut could not be re-registered");
                        }
                    }
                    _ => {}
                }
            }
            if timer != 0 {
                KillTimer(std::ptr::null_mut(), timer);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backslash_follows_the_layout_key_and_its_shift() {
        // US: the OEM_5 key, unshifted.
        assert_eq!(backslash_key_from_scan(0x00dc), (0, VK_OEM_5));
        // Canadian French: Shift plus the key left of 1.
        assert_eq!(backslash_key_from_scan(0x01de), (MOD_SHIFT, VK_OEM_7));
        // AltGr layouts (German) and missing keys keep the US position.
        assert_eq!(backslash_key_from_scan(0x06db), (0, VK_OEM_5));
        assert_eq!(backslash_key_from_scan(-1), (0, VK_OEM_5));
    }
}
