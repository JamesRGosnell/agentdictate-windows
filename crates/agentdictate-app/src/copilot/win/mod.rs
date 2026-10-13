//! Windows platform services for the meeting copilot.

pub(crate) mod audio;
pub(crate) mod desktop;
pub(crate) mod host;
pub(crate) mod hotkeys;
pub(crate) mod instance;
pub(crate) mod signals;

use super::engine::{AudioControl, AudioSink, Platform};

pub(crate) struct WindowsPlatform;

impl Platform for WindowsPlatform {
    fn start_audio(
        &self,
        microphone: bool,
        system_audio: bool,
        sink: AudioSink,
    ) -> Result<Box<dyn AudioControl>, String> {
        audio::WasapiCapture::start(microphone, system_audio, sink)
            .map(|capture| Box::new(capture) as Box<dyn AudioControl>)
    }

    fn capture_screen(&self) -> Result<Vec<u8>, String> {
        desktop::capture_primary_jpeg()
    }

    fn copy_text(&self, text: &str) {
        desktop::copy_text(text);
    }

    fn open(&self, target: &str) {
        desktop::open(target);
    }
}
