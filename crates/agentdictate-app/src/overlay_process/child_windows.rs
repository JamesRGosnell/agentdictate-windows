use std::{
    io::{self, Read, Write},
    os::windows::{
        io::AsRawHandle,
        process::{CommandExt, ExitStatusExt},
    },
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio},
    sync::mpsc::Sender,
    time::{Duration, Instant},
};

use super::{
    protocol::{OVERLAY_HELPER_ARGUMENT, OverlayHelperStatus, OverlayUpdate},
    supervisor::PresenterEvent,
};

const MAX_OVERLAY_STATUS_BYTES: usize = 64 * 1024;

fn wait_for_overlay_child(process_id: u32) -> io::Result<ExitStatus> {
    use windows_sys::Win32::{Foundation::*, System::Threading::*};
    unsafe {
        let handle = OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            process_id,
        );
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let wait = WaitForSingleObject(handle, INFINITE);
        let mut code = 0;
        let ok = GetExitCodeProcess(handle, &mut code);
        CloseHandle(handle);
        if wait == WAIT_FAILED || ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(ExitStatus::from_raw(code))
    }
}
fn monitor_overlay_status(
    mut output: ChildStdout,
    timeout: Duration,
    generation: u64,
    events: &Sender<PresenterEvent>,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let mut pending = Vec::new();
    let mut buffer = [0_u8; 1024];
    let mut submitted = false;
    let mut created = false;
    loop {
        if !submitted {
            wait_for_overlay_helper_status(&output, deadline)?;
        }
        match output.read(&mut buffer) {
            Ok(0) if !pending.is_empty() => {
                return Err("overlay helper status message was incomplete".into());
            }
            Ok(0) if !submitted => {
                return Err("overlay helper exited without submitting a frame".into());
            }
            Ok(0) => return Ok(()),
            Ok(read) => {
                pending.extend_from_slice(&buffer[..read]);
                if pending.len() > MAX_OVERLAY_STATUS_BYTES {
                    return Err("overlay helper status message was too large".into());
                }
                while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
                    let status: OverlayHelperStatus = serde_json::from_slice(&pending[..newline])
                        .map_err(|error| {
                        format!("overlay helper status message was invalid: {error}")
                    })?;
                    pending.drain(..=newline);
                    match &status {
                        OverlayHelperStatus::WindowCreated if !created && !submitted => {
                            created = true
                        }
                        OverlayHelperStatus::FrameSubmitted if !submitted => submitted = true,
                        OverlayHelperStatus::Error { .. } => {}
                        _ => {
                            return Err(
                                "overlay helper repeated or reordered a startup milestone".into()
                            );
                        }
                    }
                    let failed = matches!(status, OverlayHelperStatus::Error { .. });
                    let _ = events.send(PresenterEvent::HelperStatus {
                        generation,
                        status: Ok(status),
                    });
                    if failed {
                        return Ok(());
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(format!("overlay helper status could not be read: {error}")),
        }
    }
}

fn wait_for_overlay_helper_status(output: &ChildStdout, deadline: Instant) -> Result<(), String> {
    loop {
        let mut available = 0;
        let ok = unsafe {
            windows_sys::Win32::System::Pipes::PeekNamedPipe(
                output.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(format!(
                "overlay status pipe failed: {}",
                io::Error::last_os_error()
            ));
        }
        if available > 0 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("overlay did not submit a frame before the deadline".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
pub(super) struct OverlayChild {
    child: Child,
    input: Option<ChildStdin>,
    generation: u64,
    ready: bool,
}

impl OverlayChild {
    pub(super) fn launch(
        executable: &Path,
        ready_timeout: Duration,
        generation: u64,
        events: Sender<PresenterEvent>,
    ) -> io::Result<Self> {
        let mut command = Command::new(executable);
        command
            .arg(OVERLAY_HELPER_ARGUMENT)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .creation_flags(0x08000000);
        let mut child = command.spawn()?;
        let Some(input) = child.stdin.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other("overlay helper stdin is unavailable"));
        };
        let Some(output) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other(
                "overlay helper status pipe is unavailable",
            ));
        };
        let process_id = child.id();
        tracing::info!(generation, process_id, "recording overlay helper launched");
        if let Err(error) = std::thread::Builder::new()
            .name("agentdictate-overlay-monitor".into())
            .spawn(move || {
                if let Err(error) =
                    monitor_overlay_status(output, ready_timeout, generation, &events)
                {
                    let _ = events.send(PresenterEvent::HelperStatus {
                        generation,
                        status: Err(error),
                    });
                }
                let result = wait_for_overlay_child(process_id);
                let _ = events.send(PresenterEvent::HelperExited { generation, result });
            })
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        Ok(Self {
            child,
            input: Some(input),
            generation,
            ready: false,
        })
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    pub(super) fn mark_ready(&mut self) {
        self.ready = true;
    }

    pub(super) fn is_ready(&self) -> bool {
        self.ready
    }

    pub(super) fn send(&mut self, update: &OverlayUpdate) -> io::Result<()> {
        let input = self
            .input
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "overlay input is closed"))?;
        serde_json::to_writer(&mut *input, update).map_err(io::Error::other)?;
        input.write_all(b"\n")?;
        input.flush()
    }

    pub(super) fn finish(&mut self) {
        drop(self.input.take());
    }

    pub(super) fn terminate(&mut self) {
        drop(self.input.take());
        let _ = self.child.kill();
    }
}
