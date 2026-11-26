//! Lowers other applications' audio sessions while AgentDictate records.
//!
//! Every lowered session is saved to a small record before its volume first
//! changes, so a run that dies mid-recording (crash, forced stop, logoff) can
//! be undone by the next daemon start. Windows remembers per-application
//! volume, so without the record the lowered level would outlive the daemon.
use agentdictate_core::Settings;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io,
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};
use windows::{
    Win32::{Media::Audio::*, System::Com::*},
    core::{Interface, PWSTR},
};

/// Two volume readings closer than this are the same level.
const VOLUME_TOLERANCE: f32 = 0.002;
/// How often a fade advances.
const FADE_TICK: Duration = Duration::from_millis(10);
/// How often new audio sessions are looked for while ducking is active.
const DISCOVERY_INTERVAL: Duration = Duration::from_millis(250);

enum Command {
    Duck(Box<Settings>),
    Restore,
    Quit,
}
pub struct PlaybackDucker {
    commands: mpsc::Sender<Command>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl PlaybackDucker {
    /// Starts the ducking worker. A record left in `state_file` by an earlier
    /// run is settled first: sessions still at the level AgentDictate set get
    /// their original volume back, and later user changes are kept.
    pub fn new(state_file: PathBuf) -> Self {
        let (commands, incoming) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            // SAFETY: COM is initialized once for this worker thread and
            // uninitialized after every COM object below has been dropped.
            if let Err(error) = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok() {
                tracing::warn!(%error, "audio ducking COM initialization failed");
                return;
            }
            run_worker(&incoming, DuckingRecord::new(state_file));
            // SAFETY: paired with the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        });
        Self {
            commands,
            worker: Some(worker),
        }
    }
    pub fn duck(&self, settings: &Settings) {
        let _ = self
            .commands
            .send(Command::Duck(Box::new(settings.clone())));
    }
    pub fn restore(&self) {
        let _ = self.commands.send(Command::Restore);
    }
}
impl Drop for PlaybackDucker {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Quit);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run_worker(incoming: &mpsc::Receiver<Command>, record: DuckingRecord) {
    let mut ducking: Ducking<ISimpleAudioVolume> = Ducking::open(record);
    if ducking.has_leftovers() {
        match live_sessions() {
            Ok(live) => ducking.settle(
                &live
                    .into_iter()
                    .map(|session| (session.key, session.volume))
                    .collect::<Vec<_>>(),
            ),
            Err(error) => {
                tracing::debug!(%error, "audio ducking could not settle the earlier record yet");
            }
        }
    }
    let mut active: Option<Settings> = None;
    let mut discovered: Option<Instant> = None;
    loop {
        let wait = next_wait(
            active.is_some(),
            ducking.fading(),
            discovered.map(|at| at.elapsed()),
        );
        let received = match wait {
            Some(timeout) => incoming.recv_timeout(timeout),
            None => incoming
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
        };
        match received {
            Ok(Command::Duck(settings)) => {
                if settings.audio_ducking_enabled {
                    active = Some(*settings);
                } else {
                    ducking.restore(0);
                    active = None;
                    discovered = None;
                }
            }
            Ok(Command::Restore) => {
                let fade = active.as_ref().map_or(0, |s| s.audio_ducking_fade_in_ms);
                ducking.restore(fade);
                active = None;
                discovered = None;
            }
            Ok(Command::Quit) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                ducking.restore(0);
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if let Some(settings) = &active
            && discovered.is_none_or(|at| at.elapsed() >= DISCOVERY_INTERVAL)
        {
            discovered = Some(Instant::now());
            match live_sessions() {
                Ok(live) => {
                    for session in live {
                        if !ducking.contains(&session.instance) {
                            ducking.add(session.instance, session.key, session.volume, settings);
                        }
                    }
                }
                Err(error) => {
                    tracing::debug!(%error, "audio ducking session discovery unavailable");
                }
            }
        }
        ducking.step();
    }
}

/// How long the worker may wait for a command: a fade needs the next tick,
/// active ducking needs the next discovery, and otherwise nothing changes
/// until a command arrives (`None` blocks).
fn next_wait(active: bool, fading: bool, since_discovery: Option<Duration>) -> Option<Duration> {
    if fading {
        Some(FADE_TICK)
    } else if active {
        Some(since_discovery.map_or(Duration::ZERO, |elapsed| {
            DISCOVERY_INTERVAL.saturating_sub(elapsed)
        }))
    } else {
        None
    }
}

/// One audio session AgentDictate lowered, as saved for a later run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct DuckedVolume {
    /// `IAudioSessionControl2::GetSessionIdentifier`, stable across restarts.
    session: String,
    /// The volume to give back.
    original: f32,
    /// The volume AgentDictate last set.
    applied: f32,
}

/// A saved session is still at the level AgentDictate set, so the user did
/// not change it since.
fn untouched(entry: &DuckedVolume, current: f32) -> bool {
    (current - entry.applied).abs() < VOLUME_TOLERANCE
}

/// The level to restore to for a session found now at `current`. A level an
/// earlier run lowered is never the baseline.
fn baseline(current: f32, leftover: Option<&DuckedVolume>) -> f32 {
    match leftover {
        Some(entry) if untouched(entry, current) => entry.original,
        _ => current,
    }
}

#[derive(Default, Serialize, Deserialize)]
struct RecordFile {
    sessions: Vec<DuckedVolume>,
}

/// The on-disk record of lowered sessions; absent when nothing is lowered.
struct DuckingRecord {
    path: PathBuf,
}
impl DuckingRecord {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
    fn load(&self) -> Vec<DuckedVolume> {
        match std::fs::read(&self.path) {
            Ok(bytes) => match serde_json::from_slice::<RecordFile>(&bytes) {
                Ok(file) => file.sessions,
                Err(error) => {
                    tracing::warn!(%error, "audio ducking record is unreadable; ignoring it");
                    Vec::new()
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                tracing::warn!(%error, "audio ducking record could not be read");
                Vec::new()
            }
        }
    }
    /// Replaces the record atomically, or removes it when `sessions` is empty.
    fn save(&self, sessions: &[DuckedVolume]) -> io::Result<()> {
        if sessions.is_empty() {
            return match std::fs::remove_file(&self.path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
                _ => Ok(()),
            };
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec(&RecordFile {
            sessions: sessions.to_vec(),
        })?;
        // Written on every fade step, so no fsync: the rename alone keeps
        // the record whole when the process dies.
        let temporary = self.path.with_extension("json.tmp");
        std::fs::write(&temporary, bytes)?;
        std::fs::rename(&temporary, &self.path)
    }
}

/// A session volume the ducker reads and writes.
trait VolumeControl {
    /// The current level, or `None` when the session is gone.
    fn level(&self) -> Option<f32>;
    fn set_level(&self, level: f32) -> bool;
}
impl VolumeControl for ISimpleAudioVolume {
    fn level(&self) -> Option<f32> {
        // SAFETY: `self` is a live COM interface owned by the worker thread.
        unsafe { self.GetMasterVolume() }.ok()
    }
    fn set_level(&self, level: f32) -> bool {
        // SAFETY: as above; a null event context is documented as allowed.
        unsafe { self.SetMasterVolume(level, std::ptr::null()) }.is_ok()
    }
}

struct Session<V> {
    key: String,
    volume: V,
    original: f32,
    last: f32,
    from: f32,
    target: f32,
    start: Instant,
    duration: Duration,
}
impl<V> Session<V> {
    fn fading(&self) -> bool {
        (self.target - self.last).abs() > 0.0001
    }
    fn fraction(&self) -> f32 {
        if self.duration.is_zero() {
            1.
        } else {
            (self.start.elapsed().as_secs_f32() / self.duration.as_secs_f32()).min(1.)
        }
    }
}

/// Ducking state for one worker: live sessions keyed by instance identifier,
/// plus sessions an earlier run lowered that have not been seen again yet.
struct Ducking<V> {
    sessions: BTreeMap<String, Session<V>>,
    leftovers: Vec<DuckedVolume>,
    record: DuckingRecord,
}
impl<V: VolumeControl> Ducking<V> {
    fn open(record: DuckingRecord) -> Self {
        let leftovers = record.load();
        if !leftovers.is_empty() {
            tracing::info!(
                sessions = leftovers.len(),
                "audio ducking record found from an earlier run"
            );
        }
        Self {
            sessions: BTreeMap::new(),
            leftovers,
            record,
        }
    }
    fn has_leftovers(&self) -> bool {
        !self.leftovers.is_empty()
    }
    fn contains(&self, instance: &str) -> bool {
        self.sessions.contains_key(instance)
    }
    fn fading(&self) -> bool {
        self.sessions.values().any(Session::fading)
    }
    fn entries(&self) -> Vec<DuckedVolume> {
        let mut entries: Vec<DuckedVolume> = Vec::new();
        for session in self.sessions.values() {
            if !entries.iter().any(|entry| entry.session == session.key) {
                entries.push(DuckedVolume {
                    session: session.key.clone(),
                    original: session.original,
                    applied: session.last,
                });
            }
        }
        for leftover in &self.leftovers {
            if !entries
                .iter()
                .any(|entry| entry.session == leftover.session)
            {
                entries.push(leftover.clone());
            }
        }
        entries
    }
    fn save(&self) -> io::Result<()> {
        self.record.save(&self.entries())
    }
    fn save_or_log(&self) {
        if let Err(error) = self.save() {
            tracing::debug!(%error, "audio ducking record could not be updated");
        }
    }

    /// Gives back the original volume of sessions an earlier run lowered.
    /// `live` pairs each session's stable identifier with its volume.
    fn settle(&mut self, live: &[(String, V)]) {
        self.leftovers.retain(|entry| {
            let mut found = false;
            let mut pending = false;
            for (_, volume) in live.iter().filter(|(key, _)| *key == entry.session) {
                found = true;
                match volume.level() {
                    Some(current) if untouched(entry, current) => {
                        if volume.set_level(entry.original) {
                            tracing::info!(session = %entry.session, "audio ducking restored a volume left lowered");
                        } else {
                            pending = true;
                        }
                    }
                    Some(_) => {
                        tracing::info!(session = %entry.session, "audio ducking kept a volume the user changed");
                    }
                    None => pending = true,
                }
            }
            !found || pending
        });
        self.save_or_log();
    }

    /// Starts lowering a newly found session. The record is written before
    /// its volume first changes; without it the session is left alone.
    fn add(&mut self, instance: String, key: String, volume: V, settings: &Settings) {
        let Some(current) = volume.level() else {
            return;
        };
        let leftover = self.leftovers.iter().find(|entry| entry.session == key);
        let original = baseline(current, leftover);
        let target = original * (f32::from(settings.audio_ducking_volume_percent) / 100.);
        self.sessions.insert(
            instance.clone(),
            Session {
                key,
                volume,
                original,
                last: current,
                from: current,
                target,
                start: Instant::now(),
                duration: Duration::from_millis(u64::from(settings.audio_ducking_fade_out_ms)),
            },
        );
        if let Err(error) = self.save() {
            tracing::warn!(%error, "audio ducking skipped a session: could not save its volume");
            if let Some(session) = self.sessions.get_mut(&instance) {
                session.target = session.last;
            }
        }
    }

    /// Advances every fade one tick. A session whose volume someone else
    /// changed is left at that level.
    fn step(&mut self) {
        let mut changed = false;
        for session in self.sessions.values_mut() {
            if !session.fading() {
                continue;
            }
            let next = session.from + (session.target - session.from) * session.fraction();
            if (next - session.last).abs() <= 0.0001 {
                continue;
            }
            let untouched = session
                .volume
                .level()
                .is_some_and(|current| (current - session.last).abs() < VOLUME_TOLERANCE);
            if untouched && session.volume.set_level(next) {
                session.last = next;
                changed = true;
            } else {
                session.target = session.last;
            }
        }
        if changed {
            self.save_or_log();
        }
    }

    /// Fades every lowered session back to its original volume over
    /// `millis`, then forgets it. Sessions the user changed keep their level.
    fn restore(&mut self, millis: u32) {
        let started = Instant::now();
        let duration = Duration::from_millis(u64::from(millis));
        for session in self.sessions.values_mut() {
            session.from = session.last;
        }
        let mut gone: Vec<String> = Vec::new();
        loop {
            let fraction = if millis == 0 {
                1.
            } else {
                (started.elapsed().as_secs_f32() / duration.as_secs_f32()).min(1.)
            };
            let mut changed = false;
            for (instance, session) in &mut self.sessions {
                match session.volume.level() {
                    Some(current) if (current - session.last).abs() < VOLUME_TOLERANCE => {
                        let next = session.from + (session.original - session.from) * fraction;
                        if (next - session.last).abs() > 0.0001 && session.volume.set_level(next) {
                            session.last = next;
                            changed = true;
                        }
                    }
                    Some(_) => {}
                    None => {
                        if !gone.contains(instance) {
                            gone.push(instance.clone());
                        }
                    }
                }
            }
            if changed {
                self.save_or_log();
            }
            if fraction >= 1. {
                break;
            }
            std::thread::sleep(FADE_TICK);
        }
        let restored: Vec<String> = self.sessions.values().map(|s| s.key.clone()).collect();
        self.leftovers
            .retain(|entry| !restored.contains(&entry.session));
        // A session that vanished still lowered keeps its record, so its
        // next appearance is not mistaken for the original level.
        for instance in gone {
            if let Some(session) = self.sessions.get(&instance)
                && (session.last - session.original).abs() >= VOLUME_TOLERANCE
                && !self
                    .leftovers
                    .iter()
                    .any(|entry| entry.session == session.key)
            {
                self.leftovers.push(DuckedVolume {
                    session: session.key.clone(),
                    original: session.original,
                    applied: session.last,
                });
            }
        }
        self.sessions.clear();
        self.save_or_log();
    }
}

struct LiveSession {
    instance: String,
    key: String,
    volume: ISimpleAudioVolume,
}

/// Every render session of other processes on every active output device.
fn live_sessions() -> windows::core::Result<Vec<LiveSession>> {
    // SAFETY: called on the worker thread after CoInitializeEx; every
    // returned string is freed with CoTaskMemFree exactly once.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let devices = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        let mut sessions = Vec::new();
        for index in 0..devices.GetCount()? {
            let manager: IAudioSessionManager2 = devices.Item(index)?.Activate(CLSCTX_ALL, None)?;
            let list = manager.GetSessionEnumerator()?;
            for index in 0..list.GetCount()? {
                let control = list.GetSession(index)?;
                let extended: IAudioSessionControl2 = control.cast()?;
                if extended.GetProcessId()? == std::process::id() {
                    continue;
                }
                let instance = take_string(extended.GetSessionInstanceIdentifier()?)?;
                let key = take_string(extended.GetSessionIdentifier()?)?;
                sessions.push(LiveSession {
                    instance,
                    key,
                    volume: control.cast()?,
                });
            }
        }
        Ok(sessions)
    }
}

/// Copies and frees a COM-allocated string.
unsafe fn take_string(pointer: PWSTR) -> windows::core::Result<String> {
    // SAFETY: the caller passes a string allocated by COM for this call.
    unsafe {
        let text = pointer.to_string();
        CoTaskMemFree(Some(pointer.0.cast()));
        Ok(text?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, rc::Rc};

    #[derive(Clone)]
    struct FakeVolume(Rc<Cell<Option<f32>>>);
    impl FakeVolume {
        fn at(level: f32) -> Self {
            Self(Rc::new(Cell::new(Some(level))))
        }
        fn get(&self) -> f32 {
            self.0.get().expect("fake session is present")
        }
    }
    impl VolumeControl for FakeVolume {
        fn level(&self) -> Option<f32> {
            self.0.get()
        }
        fn set_level(&self, level: f32) -> bool {
            if self.0.get().is_none() {
                return false;
            }
            self.0.set(Some(level));
            true
        }
    }

    fn settings() -> Settings {
        Settings {
            audio_ducking_enabled: true,
            audio_ducking_volume_percent: 50,
            audio_ducking_fade_out_ms: 0,
            audio_ducking_fade_in_ms: 0,
            ..Settings::default()
        }
    }

    fn entry(session: &str, original: f32, applied: f32) -> DuckedVolume {
        DuckedVolume {
            session: session.into(),
            original,
            applied,
        }
    }

    fn record_with(directory: &tempfile::TempDir, entries: &[DuckedVolume]) -> DuckingRecord {
        let record = DuckingRecord::new(directory.path().join("runtime").join("ducking.json"));
        record.save(entries).unwrap();
        record
    }

    fn close(left: f32, right: f32) -> bool {
        (left - right).abs() < 1e-4
    }

    #[test]
    fn ducking_restores_a_leftover_volume_the_user_did_not_change() {
        let directory = tempfile::tempdir().unwrap();
        let record = record_with(&directory, &[entry("music", 0.8, 0.16)]);
        let path = record.path.clone();
        let music = FakeVolume::at(0.16);
        let mut ducking = Ducking::<FakeVolume>::open(record);
        ducking.settle(&[("music".into(), music.clone())]);
        assert!(close(music.get(), 0.8));
        assert!(!path.exists(), "the record is removed once settled");
    }

    #[test]
    fn ducking_keeps_a_leftover_volume_the_user_changed() {
        let directory = tempfile::tempdir().unwrap();
        let record = record_with(&directory, &[entry("music", 0.8, 0.16)]);
        let path = record.path.clone();
        let music = FakeVolume::at(0.5);
        let mut ducking = Ducking::<FakeVolume>::open(record);
        ducking.settle(&[("music".into(), music.clone())]);
        assert!(close(music.get(), 0.5));
        assert!(!path.exists(), "a user change settles the entry");
    }

    #[test]
    fn ducking_keeps_the_record_of_a_session_not_running_yet() {
        let directory = tempfile::tempdir().unwrap();
        let record = record_with(&directory, &[entry("music", 0.8, 0.16)]);
        let path = record.path.clone();
        let mut ducking = Ducking::<FakeVolume>::open(record);
        ducking.settle(&[("calls".into(), FakeVolume::at(1.))]);
        assert!(path.exists());
        assert_eq!(ducking.leftovers, vec![entry("music", 0.8, 0.16)]);
    }

    #[test]
    fn ducking_never_takes_a_reduced_level_as_the_baseline() {
        assert!(close(baseline(0.16, Some(&entry("music", 0.8, 0.16))), 0.8));
        assert!(close(baseline(0.5, Some(&entry("music", 0.8, 0.16))), 0.5));
        assert!(close(baseline(0.5, None), 0.5));

        let directory = tempfile::tempdir().unwrap();
        let record = record_with(&directory, &[entry("music", 0.8, 0.16)]);
        let path = record.path.clone();
        let music = FakeVolume::at(0.16);
        let mut ducking = Ducking::<FakeVolume>::open(record);
        ducking.add(
            "instance".into(),
            "music".into(),
            music.clone(),
            &settings(),
        );
        ducking.step();
        assert!(close(music.get(), 0.4), "lowered from the original 0.8");
        ducking.restore(0);
        assert!(close(music.get(), 0.8));
        assert!(!path.exists());
    }

    #[test]
    fn ducking_saves_the_original_volume_before_lowering_and_removes_it_after_restore() {
        let directory = tempfile::tempdir().unwrap();
        let record = DuckingRecord::new(directory.path().join("ducking.json"));
        let path = record.path.clone();
        let music = FakeVolume::at(0.5);
        let mut ducking = Ducking::<FakeVolume>::open(record);
        ducking.add(
            "instance".into(),
            "music".into(),
            music.clone(),
            &settings(),
        );
        assert!(close(music.get(), 0.5), "nothing changes before a step");
        assert_eq!(
            DuckingRecord::new(path.clone()).load(),
            vec![entry("music", 0.5, 0.5)]
        );
        ducking.step();
        assert!(close(music.get(), 0.25));
        assert_eq!(
            DuckingRecord::new(path.clone()).load(),
            vec![entry("music", 0.5, 0.25)]
        );
        ducking.restore(0);
        assert!(close(music.get(), 0.5));
        assert!(!path.exists());
    }

    #[test]
    fn ducking_leaves_a_session_alone_when_its_volume_cannot_be_saved() {
        let directory = tempfile::tempdir().unwrap();
        let blocker = directory.path().join("not-a-directory");
        std::fs::write(&blocker, b"").unwrap();
        let record = DuckingRecord::new(blocker.join("ducking.json"));
        let music = FakeVolume::at(0.5);
        let mut ducking = Ducking::<FakeVolume>::open(record);
        ducking.add(
            "instance".into(),
            "music".into(),
            music.clone(),
            &settings(),
        );
        ducking.step();
        assert!(close(music.get(), 0.5));
        assert!(!ducking.fading());
    }

    #[test]
    fn ducking_restore_keeps_a_volume_the_user_changed_while_recording() {
        let directory = tempfile::tempdir().unwrap();
        let record = DuckingRecord::new(directory.path().join("ducking.json"));
        let path = record.path.clone();
        let music = FakeVolume::at(0.5);
        let mut ducking = Ducking::<FakeVolume>::open(record);
        ducking.add(
            "instance".into(),
            "music".into(),
            music.clone(),
            &settings(),
        );
        ducking.step();
        music.0.set(Some(0.3));
        ducking.restore(0);
        assert!(close(music.get(), 0.3));
        assert!(!path.exists());
    }

    #[test]
    fn ducking_remembers_a_session_that_vanished_while_lowered() {
        let directory = tempfile::tempdir().unwrap();
        let record = DuckingRecord::new(directory.path().join("ducking.json"));
        let path = record.path.clone();
        let music = FakeVolume::at(0.5);
        let mut ducking = Ducking::<FakeVolume>::open(record);
        ducking.add(
            "instance".into(),
            "music".into(),
            music.clone(),
            &settings(),
        );
        ducking.step();
        music.0.set(None);
        ducking.restore(0);
        assert_eq!(
            DuckingRecord::new(path).load(),
            vec![entry("music", 0.5, 0.25)]
        );
    }

    #[test]
    fn ducking_worker_blocks_only_when_nothing_is_ducked_or_fading() {
        assert_eq!(next_wait(false, false, None), None);
        assert_eq!(next_wait(false, false, Some(Duration::from_secs(5))), None);
        assert_eq!(next_wait(false, true, None), Some(FADE_TICK));
        assert_eq!(
            next_wait(true, true, Some(Duration::from_millis(100))),
            Some(FADE_TICK)
        );
        assert_eq!(next_wait(true, false, None), Some(Duration::ZERO));
        assert_eq!(
            next_wait(true, false, Some(Duration::from_millis(100))),
            Some(Duration::from_millis(150))
        );
        assert_eq!(
            next_wait(true, false, Some(Duration::from_secs(1))),
            Some(Duration::ZERO)
        );
    }
}
