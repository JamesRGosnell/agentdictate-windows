//! Windows platform services for the meeting copilot.

pub(crate) mod audio;
pub(crate) mod comms_mic;
pub(crate) mod desktop;
pub(crate) mod host;
pub(crate) mod hotkeys;
pub(crate) mod instance;
pub(crate) mod replay;
pub(crate) mod signals;

use super::engine::{AudioControl, AudioSink, CallShot, Platform};

pub(crate) struct WindowsPlatform;

impl Platform for WindowsPlatform {
    fn start_audio(
        &self,
        microphone: bool,
        system_audio: bool,
        sink: AudioSink,
    ) -> Result<Box<dyn AudioControl>, String> {
        if let Some(path) = std::env::var_os(replay::REPLAY_VARIABLE) {
            return replay::ReplayAudio::start(path.into(), sink)
                .map(|replay| Box::new(replay) as Box<dyn AudioControl>);
        }
        audio::WasapiCapture::start(microphone, system_audio, sink)
            .map(|capture| Box::new(capture) as Box<dyn AudioControl>)
    }

    fn capture_screen(&self) -> Result<Vec<u8>, String> {
        desktop::capture_primary_jpeg()
    }

    fn capture_call_window(&self) -> Result<CallShot, String> {
        desktop::capture_call_window()
    }

    fn copy_text(&self, text: &str) {
        desktop::copy_text(text);
    }

    fn open(&self, target: &str) {
        desktop::open(target);
    }
}
