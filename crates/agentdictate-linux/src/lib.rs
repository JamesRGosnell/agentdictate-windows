//! Linux adapters for AgentDictate's runtime seams.

#[cfg(unix)]
pub mod audio_ducking;
#[cfg(unix)]
pub mod clipboard;
#[cfg(unix)]
pub mod command;
#[cfg(unix)]
pub mod focus;
pub mod hotkey;
#[cfg(unix)]
pub mod injection;
#[cfg(feature = "native-hotkey")]
#[cfg_attr(windows, path = "windows/native_hotkey.rs")]
pub mod native_hotkey;
#[cfg_attr(windows, path = "windows/overlay_placement.rs")]
pub mod overlay_placement;
pub mod paste;
#[cfg(unix)]
pub mod recorder;
