use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::{
    io,
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, channel},
};
pub(super) struct DatabaseChangeWatcher {
    watcher: RecommendedWatcher,
    events: Receiver<notify::Result<Event>>,
    directories: Vec<PathBuf>,
    files: Vec<PathBuf>,
}
impl DatabaseChangeWatcher {
    pub fn new(path: &Path) -> io::Result<Self> {
        let (tx, events) = channel();
        let watcher = notify::recommended_watcher(move |event| {
            let _ = tx.send(event);
        })
        .map_err(io::Error::other)?;
        let mut result = Self {
            watcher,
            events,
            directories: vec![],
            files: vec![],
        };
        result.add_file(path)?;
        result
            .files
            .push(PathBuf::from(format!("{}-wal", path.display())));
        result
            .files
            .push(PathBuf::from(format!("{}-journal", path.display())));
        Ok(result)
    }
    pub fn with_catalog(path: &Path, catalog: &Path) -> io::Result<Self> {
        let mut result = Self::new(path)?;
        result.add_file(catalog)?;
        Ok(result)
    }
    pub fn add_file(&mut self, path: &Path) -> io::Result<()> {
        let dir = path
            .parent()
            .ok_or_else(|| io::Error::other("file has no directory"))?
            .to_owned();
        std::fs::create_dir_all(&dir)?;
        if !self.directories.contains(&dir) {
            self.watcher
                .watch(&dir, RecursiveMode::NonRecursive)
                .map_err(io::Error::other)?;
            self.directories.push(dir);
        }
        self.files.push(path.to_owned());
        Ok(())
    }
    pub fn wait_for_change(&mut self) -> io::Result<()> {
        loop {
            let event = self
                .events
                .recv()
                .map_err(io::Error::other)?
                .map_err(io::Error::other)?;
            if !event.kind.is_access()
                && (event.paths.is_empty()
                    || event.paths.iter().any(|path| self.files.contains(path)))
            {
                return Ok(());
            }
        }
    }
}
