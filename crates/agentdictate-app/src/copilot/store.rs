//! `copilot.json` persistence beside the dictation settings.

use std::path::{Path, PathBuf};

use agentdictate_core::CopilotSettings;
use agentdictate_runtime::write_atomic;

pub(crate) const COPILOT_SETTINGS_FILE: &str = "copilot.json";

#[must_use]
pub(crate) fn settings_path(config_file: &Path) -> PathBuf {
    config_file.with_file_name(COPILOT_SETTINGS_FILE)
}

/// Loads copilot settings; a missing or unreadable file yields defaults so a
/// damaged file can never keep the copilot from opening.
#[must_use]
pub(crate) fn load(path: &Path) -> CopilotSettings {
    match std::fs::read_to_string(path) {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_else(|error| {
            tracing::warn!(%error, "copilot settings are invalid; using defaults");
            CopilotSettings::default()
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => CopilotSettings::default(),
        Err(error) => {
            tracing::warn!(%error, "copilot settings are unreadable; using defaults");
            CopilotSettings::default()
        }
    }
}

pub(crate) fn save(path: &Path, settings: &CopilotSettings) -> std::io::Result<()> {
    let mut contents = serde_json::to_string_pretty(settings).map_err(std::io::Error::other)?;
    contents.push('\n');
    write_atomic(path, contents.as_bytes(), 0o600)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentdictate_core::CopilotProvider;

    #[test]
    fn settings_round_trip_and_tolerate_damage() {
        let directory = tempfile::tempdir().unwrap();
        let path = settings_path(&directory.path().join("config.json"));
        assert_eq!(path.file_name().unwrap(), COPILOT_SETTINGS_FILE);
        assert_eq!(load(&path), CopilotSettings::default());
        let settings = CopilotSettings {
            provider: CopilotProvider::AnthropicApi,
            profile: "Founder".into(),
            ..CopilotSettings::default()
        };
        save(&path, &settings).unwrap();
        assert_eq!(load(&path), settings);
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(load(&path), CopilotSettings::default());
    }
}
