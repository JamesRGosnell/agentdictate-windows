[CmdletBinding()]
param([switch]$Background)
$ErrorActionPreference = 'Stop'
Push-Location $PSScriptRoot
try {
    & cargo build --locked -p agentdictate-app --features desktop --bins
    if ($LASTEXITCODE) { throw 'Build failed.' }
    if ($Background) { Start-Process -FilePath 'target\debug\agentdictated.exe' -ArgumentList '--service' -WindowStyle Hidden }
    else { Start-Process -FilePath 'target\debug\agentdictate.exe' -WindowStyle Normal }
} finally { Pop-Location }
