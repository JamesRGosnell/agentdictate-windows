use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

macro_rules! icons {
    ($($file:literal),+ $(,)?) => {
        /// Every embedded icon as `(file name, SVG bytes)`.
        const ICONS: &[(&str, &[u8])] = &[
            $(($file, include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../assets/icons/",
                $file
            )))),+
        ];
    };
}

icons!(
    "activity.svg",
    "arrow-up.svg",
    "chevron-down.svg",
    "chevron-up.svg",
    "copy.svg",
    "eye-off.svg",
    "eye.svg",
    "file-text.svg",
    "globe.svg",
    "inbox.svg",
    "message-question.svg",
    "mic.svg",
    "minus.svg",
    "monitor.svg",
    "pause.svg",
    "play.svg",
    "plus.svg",
    "sliders.svg",
    "sparkles.svg",
    "square.svg",
    "x.svg",
);

/// Repository-owned desktop assets embedded in every AgentDictate executable.
///
/// GPUI Component resolves its control icons through the application's asset
/// source. Keeping this source beside the UI makes missing icon paths a
/// compile- and test-visible contract instead of a packaging concern.
#[derive(Clone, Copy, Debug, Default)]
pub struct AgentDictateAssets;

impl AssetSource for AgentDictateAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let Some(file) = path.strip_prefix("icons/") else {
            return Ok(None);
        };
        Ok(ICONS
            .iter()
            .find(|(name, _)| *name == file)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        if path.trim_end_matches('/') != "icons" {
            return Ok(Vec::new());
        }

        Ok(ICONS
            .iter()
            .map(|(name, _)| SharedString::from(*name))
            .collect())
    }
}
