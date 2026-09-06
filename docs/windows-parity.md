# Windows parity verification

Baseline: `cd71aea56940303ff1c7efa2e33429614cb7253f` (upstream main).
Host: Windows x64; Rust 1.95.0 MSVC. Verification date: 2026-09-05.

Status: release installed and running. Toggle dictation in T3 Code, ChatGPT
subscription transcription, Escape cancellation, and playback ducking/restoration
were confirmed by the user. Manual Hold-mode acceptance was deferred by the user;
the full-parity acceptance record remains open for that check.

| Area | Windows implementation | Evidence |
| --- | --- | --- |
| Shared workflow, recovery, settings, history, usage, replacements, vocabulary | Original core/runtime/app code | Full workspace gate: 353 Rust tests passed; one live-account test intentionally excluded from offline execution |
| Interface and all routes | Original GPUI/component UI with Windows backend | 56 rendered/headless desktop tests and 48 UI contracts passed; eight native Settings fixtures cover both providers, empty/multiline Unicode values, and narrow/wide windows |
| ChatGPT subscription | Original Codex app-server authentication, refresh, endpoint, request and error handling; separate Windows login profile | Browser sign-in succeeded; live synthetic speech transcription returned the expected sentence |
| Paid API route, cleanup and streaming | Existing optional transports retained | Mock HTTP/WebSocket and failure/fallback tests passed; no paid key configured or paid calls made |
| Microphone | WASAPI through CPAL; 16 kHz mono PCM WAV | Real default microphone captured 0.51 s; audio deleted without upload; 16/44.1/48 kHz conversion tests passed |
| IPC and private files | Current-user named pipes, singleton lock, Windows ACLs | 59 runtime integration tests; protected current-user/System file ACL verified against the intended descriptor |
| Global shortcut | Low-level Windows keyboard hook, original edge tracker/dispatch logic | Scan-code and hold/toggle/cancel/reconfigure tests passed; real Ctrl+Space toggle dictation in T3 Code confirmed by the user |
| Overlay process | Hidden helper with readiness timeout, crash recovery and parent-pipe lifecycle | Six native process scenarios passed: normal, early exit, error, missing frame, partial frame, bounded repeated crash |
| Overlay window | Borderless native popup; no-activate/tool-window; primary taskbar placement and DPI tracking | Recording/transcribing/cleaning frames, exact bottom-center bounds, zero non-client inset, no caption/resize frame, no focus change, and clean teardown checked on an isolated desktop; native captures visually inspected |
| Clipboard and paste | Unicode clipboard readback and single tagged SendInput chord; terminal conventions | Dispatch conventions tested; real T3 Code paste confirmed by the user: one insertion, no message submission. Private clipboard probe could not create a fresh Windows window station, and did not touch the user's clipboard |
| Playback ducking | Windows audio-session volume control, fades, new-session discovery and conditional restoration | User confirmed playback lowers and returns after recording/cancellation |
| Tray | Native Windows notification icon, menu actions, Explorer restart registration | Shared action tests passed; actual executable icon resource verified; native tray initialized successfully in the installed daemon |
| Startup | HKCU Run registration; hidden background daemon | Argument quoting tested; installed HKCU Run command verified against the installed daemon path |
| Workspace refresh | Windows directory change notifications | Database/WAL, catalog, and overlay-health refresh tests passed |
| Installer/uninstaller | Portable ZIP, current-user installation, Start shortcuts, Installed apps registration | Syntax, install to path with spaces, file hashes, selective uninstall and unrelated-file preservation passed without opening UI or changing real shortcuts/registry |
| Lint | Windows application and platform libraries | Clippy passed with warnings denied |

The live subscription fixture was synthetic speech:

> Agent Dictate on Windows. The quick brown fox jumps over the lazy dog.

The response was:

> Agent dictate on Windows. The quick brown fox jumps over the lazy dog.

The normal Codex CLI profile was left unchanged. No API-key credentials were
printed, copied into this repository, or used for subscription transcription.

## Release and remaining acceptance

The optimized x64 release was built and installed in
`%LOCALAPPDATA%\Programs\AgentDictate`. The release overlay also passed its
isolated native placement/readiness/teardown check. The distributable ZIP,
SHA-256 checksum, source, and local build tooling are under the GitHub checkout.

The user reported that Start search initially showed only the source folder.
The Settings window was opened directly, a desktop shortcut was added, and
Windows executable/shortcut application metadata was added to packaging.
Native Settings visibility and its AgentDictate window title were verified.

The user subsequently found a Settings crash. The native regression probe
reproduced the exact DirectWrite panic: gpui-component 0.5.1 reused the full
multiline vocabulary placeholder's text-run length for each individual line.
The hint now uses one line; the adjacent instructions still explain one entry
per line, and actual multiline values remain supported. The probe renders the
production Settings form on an isolated Windows desktop, uses synthetic data
and a command sink that rejects writes, and is included in `run-tests.ps1`.

The recording bar was redesigned at the user's request: a 184 x 40 DIP charcoal
capsule inside a 200 x 56 DIP transparent window, rounded ends, restrained border
and shadow, Segoe UI on Windows, warm waveform/processing indicators, and a
separated timer. The native popup previously used zero (`WS_OVERLAPPED`) window
style and normal-window titlebar insets, producing the stray white frame and
offset content. It now uses `WS_POPUP`, skips that non-client path, and starts
with valid nonzero bounds before final placement. The full gate checks the
native frame contract and captures all three visible states for inspection.

Quick shortcut releases were producing valid 10 ms WAV files that received HTTP
500 from the subscription endpoint. Finalized PCM captures below 50 ms now
finish through the existing no-speech path before a finalized transcription
request, without paste or a new recovery error. The subscription route avoids
upload entirely; any active optional API stream is canceled. The guard validates actual PCM length and leaves
longer clips, unknown/corrupt files, and existing transcript checkpoints intact.
Unit boundaries and the daemon/pipeline integration test cover this behavior;
the existing 100 ms short-word and network-error regression still passes.

Remaining user check: choose Hold mode, hold Ctrl+Space while speaking in T3
Code, and release it. Confirm one paste. The user chose to perform this later.
The automated hold/release/queued-stop tests already pass. Record the manual
result here before closing full-parity acceptance.

The Linux-specific adapters remain in place behind Unix target gates. They have
not been rebuilt on Linux during this Windows verification. `cargo-deny` was
unavailable and its optional check was reported as skipped by the gate.
