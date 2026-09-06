use agentdictate_core::Settings;
use std::{
    collections::HashMap,
    sync::mpsc,
    time::{Duration, Instant},
};
use windows::{
    Win32::{Media::Audio::*, System::Com::*},
    core::Interface,
};
enum Command {
    Duck(Box<Settings>),
    Restore,
    Quit,
}
pub struct PlaybackDucker {
    commands: mpsc::Sender<Command>,
    worker: Option<std::thread::JoinHandle<()>>,
}
struct Session {
    volume: ISimpleAudioVolume,
    original: f32,
    last: f32,
    from: f32,
    target: f32,
    start: Instant,
    duration: Duration,
}
impl PlaybackDucker {
    pub fn new() -> Self {
        let (commands, incoming) = mpsc::channel();
        let worker = std::thread::spawn(move || unsafe {
            if let Err(error) = CoInitializeEx(None, COINIT_MULTITHREADED).ok() {
                tracing::warn!(%error,"audio ducking COM initialization failed");
                return;
            }
            let mut sessions = HashMap::new();
            let mut active: Option<Settings> = None;
            let mut discovered = Instant::now() - Duration::from_secs(1);
            loop {
                match incoming.recv_timeout(Duration::from_millis(10)) {
                    Ok(Command::Duck(settings)) => {
                        if settings.audio_ducking_enabled {
                            active = Some(*settings);
                        } else {
                            restore(&mut sessions, 0);
                            active = None;
                        }
                    }
                    Ok(Command::Restore) => {
                        let fade = active.as_ref().map_or(0, |s| s.audio_ducking_fade_in_ms);
                        restore(&mut sessions, fade);
                        active = None;
                    }
                    Ok(Command::Quit) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                        restore(&mut sessions, 0);
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                if discovered.elapsed() >= Duration::from_millis(250) {
                    discovered = Instant::now();
                    if let Some(settings) = &active
                        && let Err(error) = discover(&mut sessions, settings)
                    {
                        tracing::debug!(%error,"audio ducking session discovery unavailable");
                    }
                }
                for session in sessions.values_mut() {
                    let fraction = (session.start.elapsed().as_secs_f32()
                        / session.duration.as_secs_f32().max(0.001))
                    .min(1.);
                    let next = session.from + (session.target - session.from) * fraction;
                    if (next - session.last).abs() > 0.0001 {
                        if session
                            .volume
                            .GetMasterVolume()
                            .is_ok_and(|current| (current - session.last).abs() < 0.002)
                        {
                            if session
                                .volume
                                .SetMasterVolume(next, std::ptr::null())
                                .is_ok()
                            {
                                session.last = next;
                            }
                        } else {
                            session.target = session.last;
                        }
                    }
                }
            }
            CoUninitialize();
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
unsafe fn discover(
    sessions: &mut HashMap<String, Session>,
    settings: &Settings,
) -> windows::core::Result<()> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let devices = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        for index in 0..devices.GetCount()? {
            let manager: IAudioSessionManager2 = devices.Item(index)?.Activate(CLSCTX_ALL, None)?;
            let list = manager.GetSessionEnumerator()?;
            for index in 0..list.GetCount()? {
                let control = list.GetSession(index)?;
                let extended: IAudioSessionControl2 = control.cast()?;
                if extended.GetProcessId()? == std::process::id() {
                    continue;
                }
                let pointer = extended.GetSessionInstanceIdentifier()?;
                let id = pointer.to_string();
                CoTaskMemFree(Some(pointer.0.cast()));
                let id = id?;
                if sessions.contains_key(&id) {
                    continue;
                }
                let volume: ISimpleAudioVolume = control.cast()?;
                let original = volume.GetMasterVolume()?;
                let target = original * (settings.audio_ducking_volume_percent as f32 / 100.);
                sessions.insert(
                    id,
                    Session {
                        volume,
                        original,
                        last: original,
                        from: original,
                        target,
                        start: Instant::now(),
                        duration: Duration::from_millis(settings.audio_ducking_fade_out_ms as u64),
                    },
                );
            }
        }
        Ok(())
    }
}
fn restore(sessions: &mut HashMap<String, Session>, millis: u32) {
    let started = Instant::now();
    let duration = Duration::from_millis(millis as u64);
    let origins: HashMap<String, f32> = sessions
        .iter()
        .map(|(id, session)| (id.clone(), session.last))
        .collect();
    loop {
        let fraction = if millis == 0 {
            1.
        } else {
            (started.elapsed().as_secs_f32() / duration.as_secs_f32()).min(1.)
        };
        for (id, session) in sessions.iter_mut() {
            unsafe {
                if session
                    .volume
                    .GetMasterVolume()
                    .is_ok_and(|current| (current - session.last).abs() < 0.002)
                {
                    let from = origins[id];
                    let next = from + (session.original - from) * fraction;
                    if session
                        .volume
                        .SetMasterVolume(next, std::ptr::null())
                        .is_ok()
                    {
                        session.last = next;
                    }
                }
            }
        }
        if fraction >= 1. {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    sessions.clear();
}
