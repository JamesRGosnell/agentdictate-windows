# Windows

This port targets Windows 10/11 x64 and retains the existing Rust application,
GPUI interface, workflow, history, settings, recovery, and transcription routes.
The source baseline is `cd71aea56940303ff1c7efa2e33429614cb7253f`.

## Install and sign in

Extract `AgentDictate-Windows-x64.zip` and double-click `install.cmd`. It installs
for the current user and adds Start menu and desktop shortcuts. Administrator access and
Rust are not required to run the packaged application. A compatible Direct3D
graphics driver and a microphone are required. The package includes the release
Visual C++ runtime beside the executables, following Microsoft's
[application-local deployment guidance](https://learn.microsoft.com/en-us/cpp/windows/walkthrough-deploying-a-visual-cpp-application-to-an-application-local-folder).

Open AgentDictate, select **ChatGPT subscription** in Settings, and click
**Sign in with ChatGPT**. Finish the browser flow. An installed Codex CLI or a
Codex desktop distribution providing `codex.exe` is required. You can also use
the **Sign in with ChatGPT** Start menu shortcut or `agentdictate.exe login`.

AgentDictate runs the existing Codex authentication flow and retains the
upstream ChatGPT transcription request, refresh behavior, and error handling.
It never falls back from subscription dictation to paid API transcription.
The route is experimental and undocumented upstream; account entitlement and
service availability still apply. Optional API transcription and AI cleanup
remain available when the user explicitly configures an API key.

Windows browser sign-in uses `%LOCALAPPDATA%\AgentDictate\codex-login` and leaves
the normal Codex CLI profile alone. Before a separate profile is created, an
existing Codex ChatGPT login can be reused. `CODEX_BINARY` selects a specific
Codex executable; `AGENTDICTATE_CODEX_HOME` selects a dedicated auth directory.
Codex supports separate credential locations through `CODEX_HOME`, as described
in its [authentication documentation](https://learn.chatgpt.com/docs/auth).

## Dictate

The default shortcut is **Ctrl+Space**: press once to start, speak, and press
again to stop. Change the shortcut or choose Hold mode in Settings. Escape
cancels. Keep the destination text field focused when stopping the recording.
The overlay stays centered above the primary monitor's taskbar and does not
take focus. Dictate and Literal modes, replacements, vocabulary, recording
retention, recovery, history search, usage, and available streaming/API options
use the existing shared application code.

Automatic paste uses Ctrl+V in ordinary applications, Ctrl+Shift+V in Windows
Terminal, and Shift+Insert in PuTTY. Settings can force standard or terminal
paste. Windows prevents a normal process from injecting input into elevated
applications and the secure desktop. Copy from Recovery and paste manually
in those destinations, or run the applications at matching permissions.

The tray offers Settings, toggle dictation, literal dictation, and Quit.
**Launch at login** uses the current user's Windows Run registration.
The microphone uses Windows' default input device. Audio ducking temporarily
reduces other playback sessions and restores their prior volume, respecting
volume changes the user makes during dictation.

## Files and removal

Application files live in `%LOCALAPPDATA%\Programs\AgentDictate`.
Settings, history, recordings, logs, and runtime files live in
`%LOCALAPPDATA%\AgentDictate`. Private files and named pipes have a protected
current-user/System ACL. `AGENTDICTATE_DATA_HOME` overrides the data directory
for isolated testing or a separate installation.

Uninstall from Windows Installed apps. The uninstaller retains personal data
and removes only known application files. Portable use is also supported:
run `agentdictate.exe` directly from the extracted directory.

## Build and verify

Install Rust 1.95.0 with the MSVC target and Visual Studio C++ Build Tools with
the Windows SDK. From the repository directory:

```powershell
cargo build --locked -p agentdictate-app --features desktop --bins
.\run-tests.ps1
.\packaging\build-windows.ps1
```

`run.ps1` builds and opens Settings; `run.ps1 -Background` starts the daemon.
`install.ps1` builds a release package and installs it. Packaging creates a ZIP
and SHA-256 checksum in `dist`. The release workflow packages Windows on tags.

The offline gate runs all workspace tests, a native helper process fixture
covering crashes/readiness/teardown, a real overlay on an isolated desktop, and
installer/uninstaller checks in a disposable directory. It does not change the
active desktop or upload audio. Explicit additional probes are available:

```powershell
cargo run --locked -p agentdictate-app --example verify_windows -- microphone
cargo run --locked -p agentdictate-app --example verify_windows -- subscription path\to\fixture.wav
```

The microphone probe deletes its audio without uploading. The subscription
probe uploads only the specified file using the ChatGPT route.
See [Windows parity verification](windows-parity.md) for completed and pending
acceptance checks.
