[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
Push-Location $PSScriptRoot
try {
    if (Get-Process cargo,rustc,link -ErrorAction SilentlyContinue) { throw 'Another Rust/linker build is running. Wait before starting the final gate.' }
    if ((Get-PSDrive -Name ([IO.Path]::GetPathRoot($PSScriptRoot).Substring(0,1))).Free -lt 15GB) { throw 'At least 15 GB free is required for the full test gate.' }
    & cargo test --locked --workspace --all-targets --all-features
    if ($LASTEXITCODE) { throw 'Workspace tests failed.' }
    & cargo build --locked -p agentdictate-app --features desktop --bins --example verify_windows
    if ($LASTEXITCODE) { throw 'Native verification binaries failed to build.' }
    & 'target\debug\examples\verify_windows.exe' overlay
    if ($LASTEXITCODE) { throw 'Windows overlay lifecycle checks failed.' }
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/test-windows-overlay.ps1
    if ($LASTEXITCODE) { throw 'Isolated native overlay check failed.' }
    & cargo build --locked -p agentdictate-ui --features desktop --example verify_windows_settings
    if ($LASTEXITCODE) { throw 'Native Settings verification binary failed to build.' }
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/test-windows-settings.ps1
    if ($LASTEXITCODE) { throw 'Isolated native Settings check failed.' }
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File packaging/test-windows.ps1
    if ($LASTEXITCODE) { throw 'Windows packaging checks failed.' }
    if (Get-Command cargo-deny -ErrorAction SilentlyContinue) {
        & cargo deny check
        if ($LASTEXITCODE) { throw 'Dependency policy check failed.' }
    } else { Write-Host 'SKIPPED: cargo-deny is not installed.' }
    Write-Host 'Windows offline gate passed. Interactive microphone, paste, shortcut, ducking, and ChatGPT account checks are separate.'
} finally { Pop-Location }
