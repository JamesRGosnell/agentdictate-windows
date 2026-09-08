use std::{
    io,
    process::Command,
    sync::{Arc, Mutex, MutexGuard},
    thread,
    time::{Duration, Instant},
};

use agentdictate_core::Settings;

const RAMP_STEP_MS: u32 = 50;
const MAX_RAMP_STEPS: u32 = 100;

pub trait Pactl {
    fn default_sink(&mut self) -> io::Result<String>;
    fn sink_volume(&mut self, name: &str) -> io::Result<Vec<u32>>;
    fn set_sink_volume(&mut self, name: &str, volumes: &[u32]) -> io::Result<()>;
}

#[derive(Default)]
pub struct SystemPactl;

fn pactl_output(args: &[&str]) -> io::Result<String> {
    let output = Command::new("pactl")
        .env("LC_ALL", "C")
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "pactl {} failed: {}",
            args[0],
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

impl Pactl for SystemPactl {
    fn default_sink(&mut self) -> io::Result<String> {
        let name = pactl_output(&["get-default-sink"])?;
        if name.is_empty() {
            return Err(io::Error::other("no default audio output"));
        }
        Ok(name)
    }

    fn sink_volume(&mut self, name: &str) -> io::Result<Vec<u32>> {
        parse_volume(&pactl_output(&["get-sink-volume", name])?)
    }

    fn set_sink_volume(&mut self, name: &str, volumes: &[u32]) -> io::Result<()> {
        let volumes = volumes.iter().map(u32::to_string).collect::<Vec<_>>();
        let mut args = vec!["set-sink-volume", name];
        args.extend(volumes.iter().map(String::as_str));
        pactl_output(&args).map(|_| ())
    }
}

fn parse_volume(output: &str) -> io::Result<Vec<u32>> {
    let volumes = output
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("Volume:"))
        .and_then(|line| {
            line.split(',')
                .map(|channel| {
                    channel
                        .split_once(':')?
                        .1
                        .split_whitespace()
                        .next()?
                        .parse()
                        .ok()
                })
                .collect::<Option<Vec<u32>>>()
        })
        .filter(|volumes| !volumes.is_empty());
    volumes.ok_or_else(|| io::Error::other("pactl returned an invalid output volume"))
}

struct SavedOutput {
    name: String,
    original: Vec<u32>,
    applied: Vec<u32>,
}

struct Inner<P: Pactl> {
    pactl: P,
    saved: Option<SavedOutput>,
    generation: u64,
    fade_in_ms: u32,
}

/// Duck the default output selected at recording start. App stream volumes are
/// never changed: replacing a tab cannot inherit or compound a ducked baseline.
/// Restore that same output even if the default changes in the meantime.
pub struct PlaybackDucker<P: Pactl + Send + 'static = SystemPactl> {
    inner: Arc<Mutex<Inner<P>>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Default for PlaybackDucker<SystemPactl> {
    fn default() -> Self {
        Self::with_pactl(SystemPactl)
    }
}

impl<P: Pactl + Send + 'static> PlaybackDucker<P> {
    fn with_pactl(pactl: P) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                pactl,
                saved: None,
                generation: 0,
                fade_in_ms: 0,
            })),
            worker: None,
        }
    }

    pub fn duck(&mut self, settings: &Settings) {
        self.restore_immediately();
        if !settings.audio_ducking_enabled {
            return;
        }
        let mut inner = lock_unpoisoned(&self.inner);
        // A failed restore keeps its original. Never snapshot a reduced volume
        // as the baseline for another recording, including on a different output.
        if inner.saved.is_some() {
            return;
        }
        let snapshot = (|| {
            let name = inner.pactl.default_sink()?;
            let original = inner.pactl.sink_volume(&name)?;
            Ok::<_, io::Error>(SavedOutput {
                name,
                applied: original.clone(),
                original,
            })
        })();
        let saved = match snapshot {
            Ok(saved) => saved,
            Err(error) => {
                tracing::warn!(%error, "audio ducking output snapshot failed");
                return;
            }
        };
        let target = ducking_target_volumes(&saved.original, settings.audio_ducking_volume_percent);
        tracing::info!(sink = %saved.name, original = ?saved.original, ?target, "audio output ducking started");
        inner.fade_in_ms = settings.audio_ducking_fade_in_ms;
        inner.saved = Some(saved);
        drop(inner);
        self.ramp(target, settings.audio_ducking_fade_out_ms, false);
    }

    pub fn restore(&mut self) {
        let fade_in_ms = lock_unpoisoned(&self.inner).fade_in_ms;
        self.restore_with_fade(fade_in_ms);
    }

    fn restore_immediately(&mut self) {
        self.restore_with_fade(0);
    }

    fn restore_with_fade(&mut self, fade_ms: u32) {
        let mut inner = lock_unpoisoned(&self.inner);
        inner.generation = inner.generation.wrapping_add(1);
        self.worker = None;
        let Inner { pactl, saved, .. } = &mut *inner;
        let Some(output) = saved else { return };
        match pactl.sink_volume(&output.name) {
            Ok(current) if current != output.applied => {
                // A volume key or mixer change is the user's new preference.
                tracing::info!(sink = %output.name, ?current, "audio ducking preserved external volume change");
                *saved = None;
                return;
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(sink = %output.name, %error, "audio ducking restore read failed");
                return;
            }
        }
        let target = output.original.clone();
        drop(inner);
        self.ramp(target, fade_ms, true);
    }

    // One ramp implementation owns writes in both directions. Generation changes
    // cancel detached workers before they can write over a newer recording.
    fn ramp(&mut self, target: Vec<u32>, fade_ms: u32, restoring: bool) {
        let mut inner = lock_unpoisoned(&self.inner);
        let Some(saved) = &inner.saved else { return };
        let steps = ramp_plan(&saved.applied, &target, fade_ms);
        if fade_ms == 0 {
            write_volume(&mut inner, &target, restoring);
            return;
        }
        let generation = inner.generation;
        drop(inner);
        let step_delay = Duration::from_millis(u64::from(fade_ms.div_ceil(steps.len() as u32)));
        let shared = Arc::clone(&self.inner);
        match thread::Builder::new()
            .name("agentdictate-audio-ducking".into())
            .spawn(move || {
                let started_at = Instant::now();
                for (index, volumes) in steps.iter().enumerate() {
                    wait_for_ramp_step(started_at, step_delay, index as u32 + 1);
                    let mut inner = lock_unpoisoned(&shared);
                    if inner.generation != generation
                        || !write_volume(&mut inner, volumes, restoring && index + 1 == steps.len())
                    {
                        return;
                    }
                }
            }) {
            Ok(worker) => self.worker = Some(worker),
            Err(error) => {
                tracing::warn!(%error, "audio ducking fade worker unavailable");
                if restoring {
                    write_volume(&mut lock_unpoisoned(&self.inner), &target, true);
                }
            }
        }
    }
}

impl<P: Pactl + Send + 'static> Drop for PlaybackDucker<P> {
    fn drop(&mut self) {
        self.restore_immediately();
    }
}

fn write_volume<P: Pactl>(inner: &mut Inner<P>, volumes: &[u32], restored: bool) -> bool {
    let Some(saved) = &mut inner.saved else {
        return false;
    };
    if let Err(error) = inner.pactl.set_sink_volume(&saved.name, volumes) {
        tracing::warn!(sink = %saved.name, ?volumes, %error, "audio ducking volume write failed");
        return false;
    }
    saved.applied = volumes.to_vec();
    if restored {
        tracing::info!(sink = %saved.name, ?volumes, "audio output volume restored");
        inner.saved = None;
    }
    true
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn ducking_target_volumes(volumes: &[u32], percent: u8) -> Vec<u32> {
    // PulseAudio maps raw software volume to linear amplitude cubically, so a
    // perceived loudness fraction needs its cube root in raw-volume space.
    let raw_scale = (f64::from(percent.min(100)) / 100.0).cbrt();
    volumes
        .iter()
        .map(|volume| (f64::from(*volume) * raw_scale).round() as u32)
        .collect()
}

fn ramp_plan(start: &[u32], target: &[u32], fade_ms: u32) -> Vec<Vec<u32>> {
    let step_count = if fade_ms == 0 {
        1
    } else {
        fade_ms.div_ceil(RAMP_STEP_MS).min(MAX_RAMP_STEPS) as usize
    };

    (1..=step_count)
        .map(|step| {
            if step == step_count {
                return target.to_vec();
            }
            let progress = step as f64 / step_count as f64;
            start
                .iter()
                .zip(target)
                .map(|(start, target)| {
                    (f64::from(*start) + (f64::from(*target) - f64::from(*start)) * progress)
                        .round() as u32
                })
                .collect()
        })
        .collect()
}

fn remaining_until_ramp_step(
    started_at: Instant,
    now: Instant,
    step_delay: Duration,
    step_number: u32,
) -> Duration {
    (started_at + step_delay * step_number).saturating_duration_since(now)
}

fn wait_for_ramp_step(started_at: Instant, step_delay: Duration, step_number: u32) {
    let remaining = remaining_until_ramp_step(started_at, Instant::now(), step_delay, step_number);
    if !remaining.is_zero() {
        thread::sleep(remaining);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct FakeState {
        default: String,
        volumes: HashMap<String, Vec<u32>>,
        writes: Vec<(String, Vec<u32>)>,
        fail_write: Option<Vec<u32>>,
    }

    struct FakePactl(Arc<Mutex<FakeState>>);

    impl Pactl for FakePactl {
        fn default_sink(&mut self) -> io::Result<String> {
            Ok(lock_unpoisoned(&self.0).default.clone())
        }

        fn sink_volume(&mut self, name: &str) -> io::Result<Vec<u32>> {
            lock_unpoisoned(&self.0)
                .volumes
                .get(name)
                .cloned()
                .ok_or_else(|| io::Error::other("output unavailable"))
        }

        fn set_sink_volume(&mut self, name: &str, volumes: &[u32]) -> io::Result<()> {
            let mut state = lock_unpoisoned(&self.0);
            if state.fail_write.as_deref() == Some(volumes) {
                return Err(io::Error::other("volume write failed"));
            }
            let current = state
                .volumes
                .get_mut(name)
                .ok_or_else(|| io::Error::other("output unavailable"))?;
            *current = volumes.to_vec();
            state.writes.push((name.to_owned(), volumes.to_vec()));
            Ok(())
        }
    }

    fn fixture() -> (PlaybackDucker<FakePactl>, Arc<Mutex<FakeState>>) {
        let state = Arc::new(Mutex::new(FakeState {
            default: "headphones".into(),
            volumes: HashMap::from([
                ("headphones".into(), vec![65_536, 32_768]),
                ("speakers".into(), vec![40_000, 40_000]),
            ]),
            writes: Vec::new(),
            fail_write: None,
        }));
        (
            PlaybackDucker::with_pactl(FakePactl(Arc::clone(&state))),
            state,
        )
    }

    fn settings(fade_out_ms: u32, fade_in_ms: u32) -> Settings {
        Settings {
            audio_ducking_enabled: true,
            audio_ducking_volume_percent: 15,
            audio_ducking_fade_out_ms: fade_out_ms,
            audio_ducking_fade_in_ms: fade_in_ms,
            ..Settings::default()
        }
    }

    fn join(ducker: &mut PlaybackDucker<FakePactl>) {
        if let Some(worker) = ducker.worker.take() {
            worker.join().unwrap();
        }
    }

    #[test]
    fn output_volume_parser_preserves_channels_and_rejects_partial_data() {
        assert_eq!(parse_volume("Volume: front-left: 65536 / 100% / 0 dB, front-right: 32768 / 50% / -18 dB\n        balance -0.5").unwrap(), vec![65_536, 32_768]);
        assert_eq!(
            parse_volume("Volume: mono: 0 / 0% / -inf dB").unwrap(),
            vec![0]
        );
        assert!(parse_volume("Volume: front-left: 123 / 1%, front-right: invalid").is_err());
        assert!(parse_volume("").is_err());
    }

    #[test]
    fn repeated_recordings_restore_exact_channel_volumes_with_or_without_fades() {
        for fade_ms in [0, 100] {
            let (mut ducker, state) = fixture();
            for _ in 0..3 {
                ducker.duck(&settings(fade_ms, fade_ms));
                join(&mut ducker);
                assert_eq!(
                    lock_unpoisoned(&state).volumes["headphones"],
                    vec![34_821, 17_411]
                );
                ducker.restore();
                join(&mut ducker);
                assert_eq!(
                    lock_unpoisoned(&state).volumes["headphones"],
                    vec![65_536, 32_768]
                );
                assert!(lock_unpoisoned(&ducker.inner).saved.is_none());
            }
        }
    }

    #[test]
    fn default_output_change_does_not_redirect_restoration() {
        let (mut ducker, state) = fixture();
        ducker.duck(&settings(0, 0));
        lock_unpoisoned(&state).default = "speakers".into();
        ducker.restore();
        {
            let state = lock_unpoisoned(&state);
            assert_eq!(state.volumes["headphones"], vec![65_536, 32_768]);
            assert_eq!(state.volumes["speakers"], vec![40_000, 40_000]);
            assert!(state.writes.iter().all(|(name, _)| name == "headphones"));
        }
        ducker.duck(&settings(0, 0));
        assert_eq!(
            lock_unpoisoned(&state).volumes["speakers"],
            vec![21_253, 21_253]
        );
        drop(ducker);
        assert_eq!(
            lock_unpoisoned(&state).volumes["speakers"],
            vec![40_000, 40_000]
        );
    }

    #[test]
    fn failed_restore_never_becomes_a_new_ducking_baseline() {
        let (mut ducker, state) = fixture();
        ducker.duck(&settings(0, 0));
        lock_unpoisoned(&state).fail_write = Some(vec![65_536, 32_768]);
        ducker.restore();
        ducker.duck(&settings(0, 0));
        assert_eq!(lock_unpoisoned(&state).writes.len(), 1);
        assert_eq!(
            lock_unpoisoned(&ducker.inner)
                .saved
                .as_ref()
                .unwrap()
                .original,
            vec![65_536, 32_768]
        );
        lock_unpoisoned(&state).fail_write = None;
        ducker.duck(&settings(0, 0));
        ducker.restore();
        assert_eq!(
            lock_unpoisoned(&state).volumes["headphones"],
            vec![65_536, 32_768]
        );
    }

    #[test]
    fn disappeared_output_keeps_its_original_without_touching_another_device() {
        let (mut ducker, state) = fixture();
        ducker.duck(&settings(0, 0));
        let disconnected = {
            let mut state = lock_unpoisoned(&state);
            state.default = "speakers".into();
            state.volumes.remove("headphones").unwrap()
        };
        ducker.restore();
        ducker.duck(&settings(0, 0));
        assert_eq!(lock_unpoisoned(&state).writes.len(), 1);
        lock_unpoisoned(&state)
            .volumes
            .insert("headphones".into(), disconnected);
        ducker.restore();
        assert_eq!(
            lock_unpoisoned(&state).volumes["headphones"],
            vec![65_536, 32_768]
        );
    }

    #[test]
    fn user_volume_change_is_preserved_on_stop() {
        let (mut ducker, state) = fixture();
        ducker.duck(&settings(0, 0));
        lock_unpoisoned(&state)
            .volumes
            .insert("headphones".into(), vec![20_000, 10_000]);
        ducker.restore();
        assert_eq!(
            lock_unpoisoned(&state).volumes["headphones"],
            vec![20_000, 10_000]
        );
        assert!(lock_unpoisoned(&ducker.inner).saved.is_none());
    }

    #[test]
    fn new_recording_cancels_restore_worker_before_it_can_overwrite_ducking() {
        let (mut ducker, state) = fixture();
        ducker.duck(&settings(0, 100));
        ducker.restore();
        let old_worker = ducker.worker.take().unwrap();
        ducker.duck(&settings(0, 0));
        old_worker.join().unwrap();
        assert_eq!(
            lock_unpoisoned(&state).volumes["headphones"],
            vec![34_821, 17_411]
        );
        drop(ducker);
        assert_eq!(
            lock_unpoisoned(&state).volumes["headphones"],
            vec![65_536, 32_768]
        );
    }

    #[test]
    fn stop_during_fade_out_cancels_all_later_ducking_writes() {
        let (mut ducker, state) = fixture();
        ducker.duck(&settings(100, 0));
        let old_worker = ducker.worker.take().unwrap();
        ducker.restore();
        old_worker.join().unwrap();
        assert_eq!(
            lock_unpoisoned(&state).volumes["headphones"],
            vec![65_536, 32_768]
        );
    }

    #[test]
    fn failed_fade_restoration_retains_original_for_retry() {
        let (mut ducker, state) = fixture();
        ducker.duck(&settings(0, 100));
        lock_unpoisoned(&state).fail_write = Some(vec![65_536, 32_768]);
        ducker.restore();
        join(&mut ducker);
        assert!(lock_unpoisoned(&ducker.inner).saved.is_some());
        lock_unpoisoned(&state).fail_write = None;
        drop(ducker);
        assert_eq!(
            lock_unpoisoned(&state).volumes["headphones"],
            vec![65_536, 32_768]
        );
    }

    #[test]
    fn disabled_ducking_leaves_output_untouched() {
        let (mut ducker, state) = fixture();
        ducker.duck(&Settings {
            audio_ducking_enabled: false,
            ..settings(0, 0)
        });
        assert!(lock_unpoisoned(&state).writes.is_empty());
    }

    #[test]
    fn ramp_is_bounded_monotonic_and_finishes_at_exact_target() {
        for (start, target) in [(vec![100, 50], vec![0, 25]), (vec![0, 25], vec![100, 50])] {
            let ramp = ramp_plan(&start, &target, 200);
            assert_eq!(ramp.len(), 4);
            assert_eq!(ramp.last(), Some(&target));
            for steps in ramp.windows(2) {
                for channel in 0..2 {
                    assert_eq!(
                        steps[1][channel].cmp(&steps[0][channel]),
                        target[channel].cmp(&start[channel])
                    );
                }
            }
        }
        assert_eq!(ramp_plan(&[100], &[0], 0), vec![vec![0]]);
        assert_eq!(
            ramp_plan(&[100], &[0], RAMP_STEP_MS * (MAX_RAMP_STEPS + 1)).len(),
            MAX_RAMP_STEPS as usize
        );
        let start = Instant::now();
        assert_eq!(
            remaining_until_ramp_step(
                start,
                start + Duration::from_millis(80),
                Duration::from_millis(50),
                2
            ),
            Duration::from_millis(20)
        );
        assert_eq!(
            remaining_until_ramp_step(
                start,
                start + Duration::from_millis(120),
                Duration::from_millis(50),
                2
            ),
            Duration::ZERO
        );
    }
}
