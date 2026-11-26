use std::{fs, io, path::Path};

use tracing_appender::{
    non_blocking::WorkerGuard,
    rolling::{RollingFileAppender, Rotation},
};

/// Daily log files kept per prefix; older ones are deleted on rollover.
const RETAINED_LOG_FILES: usize = 14;

/// Installs process-wide structured logging and returns the flush guard that
/// must live for the remainder of the process.
pub fn init_file_logging(directory: &Path, file_prefix: &str) -> io::Result<WorkerGuard> {
    let appender = daily_log_appender(directory, file_prefix)?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_target(true)
        .with_thread_ids(true)
        .with_thread_names(true)
        .with_writer(writer)
        .try_init()
        .map_err(|error| io::Error::other(error.to_string()))?;
    std::panic::set_hook(Box::new(|panic| {
        tracing::error!(panic = %panic, "process panicked");
    }));
    Ok(guard)
}

/// Rolls `file_prefix` daily and keeps only the newest log files with that
/// prefix; files with other prefixes in the same directory are untouched.
fn daily_log_appender(directory: &Path, file_prefix: &str) -> io::Result<RollingFileAppender> {
    fs::create_dir_all(directory)?;
    RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(file_prefix)
        .max_log_files(RETAINED_LOG_FILES)
        .build(directory)
        .map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_logs_keep_two_weeks_per_prefix_and_leave_other_prefixes_alone() {
        let directory = tempfile::tempdir().unwrap();
        for day in 1..=20 {
            let name = format!("agentdictated.log.2000-01-{day:02}");
            fs::write(directory.path().join(name), b"old").unwrap();
        }
        fs::write(directory.path().join("copilot.log.2000-01-01"), b"old").unwrap();

        drop(daily_log_appender(directory.path(), "agentdictated.log").unwrap());

        let names = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        let daemon_logs = names
            .iter()
            .filter(|name| name.starts_with("agentdictated.log"))
            .count();
        assert_eq!(daemon_logs, RETAINED_LOG_FILES);
        assert!(names.iter().any(|name| name == "copilot.log.2000-01-01"));
        assert!(
            !names
                .iter()
                .any(|name| name == "agentdictated.log.2000-01-01")
        );
    }
}
