//! Explicit native probes. No probe runs as part of the normal offline test suite.
//! `subscription <wav>` uploads only the explicitly provided fixture.
use agentdictate_app::{
    CodexSubscriptionTransport, RecordingController, SpeechTransport, SystemRecordingController,
    TranscriptionRequest,
};
use agentdictate_core::{Settings, TranscriptionProvider};
use agentdictate_runtime::{RecordingRequest, Runtime};
use std::{path::PathBuf, time::Duration};
fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--overlay-helper") => {
            return overlay_fixture();
        }
        Some("overlay") => {
            verify_overlay_lifecycle()?;
        }
        Some("subscription") => {
            let path = PathBuf::from(
                args.get(2)
                    .ok_or_else(|| anyhow::anyhow!("provide a WAV fixture"))?,
            );
            let mut transport = CodexSubscriptionTransport::new();
            let text = transport.transcribe_audio(TranscriptionRequest {
                keywords: &[],
                audio_path: &path,
                provider: TranscriptionProvider::ChatGptSubscription,
                model: "gpt-transcribe",
                language: "en",
                prompt: "",
                duration_seconds: 3.,
            })?;
            println!("Subscription fixture transcript: {text}");
            anyhow::ensure!(!text.trim().is_empty(), "transcription was empty");
        }
        #[cfg(windows)]
        Some("clipboard") => {
            verify_clipboard()?;
        }
        Some("microphone") => {
            let directory = tempfile::tempdir()?;
            let mut runtime = Runtime::open(directory.path().join("test.sqlite"))?;
            let request = RecordingRequest {
                options: None,
                audio_path: directory.path().join("microphone.wav"),
                started_at: chrono::Utc::now(),
                transcription_provider: TranscriptionProvider::ChatGptSubscription,
                transcription_model: "gpt-transcribe".into(),
            };
            let mut recorder = SystemRecordingController::for_system(
                &Settings {
                    audio_ducking_enabled: false,
                    ..Settings::default()
                },
                &directory.path().join("runtime"),
            );
            let job = runtime.start_recording(request, &mut recorder)?;
            std::thread::sleep(Duration::from_millis(500));
            let capture = recorder.finish(&job)?;
            anyhow::ensure!(
                capture.duration_seconds > 0.2,
                "microphone did not produce sufficient audio"
            );
            println!(
                "Microphone captured {:.2} seconds of 16 kHz mono PCM. Probe audio is deleted without uploading.",
                capture.duration_seconds
            );
        }
        _ => anyhow::bail!("Usage: verify_windows subscription <fixture.wav> | microphone"),
    }
    Ok(())
}

#[cfg(windows)]
fn verify_clipboard() -> anyhow::Result<()> {
    use windows_sys::Win32::System::StationsAndDesktops::*;
    // This explicit probe owns a private window station and desktop so the real
    // user's clipboard and foreground app cannot be touched by its native calls.
    unsafe {
        let previous_station = GetProcessWindowStation();
        let previous_desktop =
            GetThreadDesktop(windows_sys::Win32::System::Threading::GetCurrentThreadId());
        let station = CreateWindowStationW(std::ptr::null(), 1, 0x0000037F, std::ptr::null());
        anyhow::ensure!(
            !station.is_null(),
            "create private window station: {}",
            std::io::Error::last_os_error()
        );
        anyhow::ensure!(
            SetProcessWindowStation(station) != 0,
            "select private window station"
        );
        let desktop_name: Vec<u16> = "Default".encode_utf16().chain(Some(0)).collect();
        let desktop = CreateDesktopW(
            desktop_name.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            0x000F01FF,
            std::ptr::null(),
        );
        anyhow::ensure!(
            !desktop.is_null() && SetThreadDesktop(desktop) != 0,
            "select private desktop"
        );
        let result = agentdictate_app::SystemDeliverer::for_environment("Automatic").copy_text(
            "Windows clipboard: caf\u{e9}, \u{1f399}, \u{65e5}\u{672c}\u{8a9e}\nSecond line",
        );
        SetThreadDesktop(previous_desktop);
        SetProcessWindowStation(previous_station);
        CloseDesktop(desktop);
        CloseWindowStation(station);
        result?;
    }
    println!(
        "Unicode clipboard publication and readback passed in a private window station; the user's clipboard was untouched."
    );
    Ok(())
}

fn overlay_fixture() -> anyhow::Result<()> {
    use std::io::{BufRead, Write};
    let exe = std::env::current_exe()?;
    let directory = exe.parent().unwrap();
    let scenario = std::fs::read_to_string(directory.join("scenario"))?;
    let launches = directory.join("launches");
    let previous = std::fs::read_to_string(&launches).unwrap_or_default();
    let count = previous.lines().count() + 1;
    std::fs::write(&launches, format!("{previous}launch\n"))?;
    let mut input = std::io::stdin().lock().lines();
    let _ = input.next();
    if scenario == "crash" {
        std::process::exit(17);
    }
    if count == 1 {
        match scenario.trim() {
            "exit" => std::process::exit(17),
            "error" => println!("{{\"status\":\"error\",\"message\":\"fixture unavailable\"}}"),
            "created" => println!("{{\"status\":\"window_created\"}}"),
            "partial" => print!("{{\"status\":"),
            _ => {}
        }
        std::io::stdout().flush()?;
        if scenario != "normal" {
            for _ in input {}
            return Ok(());
        }
    }
    println!("{{\"status\":\"frame_submitted\"}}");
    std::io::stdout().flush()?;
    std::fs::write(directory.join("ready"), "ready")?;
    for _ in input {}
    std::fs::write(directory.join("exited"), "exited")?;
    Ok(())
}
fn verify_overlay_lifecycle() -> anyhow::Result<()> {
    use agentdictate_app::{OverlayUpdate, start_overlay_presenter_with_timeout};
    use agentdictate_core::{JobId, Workflow, WorkflowSignal};
    for scenario in ["normal", "exit", "error", "created", "partial", "crash"] {
        let directory = tempfile::tempdir()?;
        let helper = directory.path().join(if cfg!(windows) {
            "fixture.exe"
        } else {
            "fixture"
        });
        std::fs::copy(std::env::current_exe()?, &helper)?;
        std::fs::write(directory.path().join("scenario"), scenario)?;
        let (controller, presenter) =
            start_overlay_presenter_with_timeout(helper, Duration::from_millis(200))?;
        let job_id = JobId::new();
        let mut workflow = Workflow::new();
        workflow.apply(WorkflowSignal::StartRequested { job_id })?;
        workflow.apply(WorkflowSignal::FirstAudioFrameWritten { job_id })?;
        controller.update(OverlayUpdate {
            workflow: workflow.snapshot(),
            active_recording: None,
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        let passed = loop {
            let launches = std::fs::read_to_string(directory.path().join("launches"))
                .unwrap_or_default()
                .lines()
                .count();
            if scenario == "crash" && launches >= 2 {
                std::thread::sleep(Duration::from_millis(400));
                break std::fs::read_to_string(directory.path().join("launches"))?
                    .lines()
                    .count()
                    == 2;
            }
            if scenario != "crash" && directory.path().join("ready").exists() {
                break true;
            }
            if std::time::Instant::now() > deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let dismissed = controller.dismiss_and_wait();
        drop(controller);
        presenter
            .join()
            .map_err(|_| anyhow::anyhow!("presenter panicked"))?;
        anyhow::ensure!(
            passed,
            "overlay scenario {scenario} failed readiness/relaunch/bounded retry"
        );
        dismissed?;
        if scenario != "crash" {
            anyhow::ensure!(
                directory.path().join("exited").exists(),
                "overlay {scenario} did not finish before dismissal acknowledgment"
            );
        }
        println!("Overlay lifecycle: {scenario} passed");
    }
    Ok(())
}
