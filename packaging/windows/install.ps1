[CmdletBinding()]
param([string]$InstallDirectory = (Join-Path $env:LOCALAPPDATA 'Programs\AgentDictate'), [switch]$NoStart, [switch]$NoShortcuts)
$ErrorActionPreference = 'Stop'
$destination = [IO.Path]::GetFullPath($InstallDirectory)
if ([IO.Path]::GetPathRoot($destination).TrimEnd('\') -eq $destination.TrimEnd('\')) { throw 'Choose an application directory, not a drive root.' }
foreach ($name in @('agentdictate.exe', 'agentdictated.exe', 'agentdictate.ico')) {
    if (!(Test-Path -LiteralPath (Join-Path $PSScriptRoot $name))) { throw "Package is missing $name. Extract the whole ZIP first." }
}
$existing = Join-Path $destination 'agentdictate.exe'
if (Test-Path -LiteralPath $existing) {
    $stop = Start-Process -FilePath $existing -ArgumentList 'quit' -WindowStyle Hidden -PassThru
    if (!$stop.WaitForExit(15000)) { throw 'AgentDictate is still closing. Quit it from the tray and run this installer again.' }
    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    while (Get-Process agentdictated -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq (Join-Path $destination 'agentdictated.exe') }) {
        if ([DateTime]::UtcNow -gt $deadline) { throw 'AgentDictate has not finished closing.' }
        Start-Sleep -Milliseconds 100
    }
}
New-Item -ItemType Directory -Force -Path $destination | Out-Null
foreach ($name in @('vcruntime140.dll','vcruntime140_1.dll')) {
    $library = Join-Path $PSScriptRoot $name
    if (Test-Path -LiteralPath $library) { Copy-Item -LiteralPath $library -Destination $destination -Force }
}
foreach ($name in @('agentdictate.exe', 'agentdictated.exe', 'agentdictate.ico', 'uninstall.ps1', 'shortcuts.ps1', 'README.txt', 'LICENSE')) {
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot $name) -Destination (Join-Path $destination $name) -Force
}
if (!$NoShortcuts) {
    $menu = Join-Path ([Environment]::GetFolderPath('Programs')) 'AgentDictate'
    New-Item -ItemType Directory -Force -Path $menu | Out-Null
    . (Join-Path $PSScriptRoot 'shortcuts.ps1')
    New-AgentDictateShortcut (Join-Path ([Environment]::GetFolderPath('Programs')) 'AgentDictate.lnk') $destination
    New-AgentDictateShortcut (Join-Path ([Environment]::GetFolderPath('Desktop')) 'AgentDictate.lnk') $destination
    New-AgentDictateShortcut (Join-Path $menu 'Sign in with ChatGPT.lnk') $destination 'login'
    $oldShortcut = Join-Path $menu 'AgentDictate.lnk'
    if (Test-Path -LiteralPath $oldShortcut) {
        $oldLink = (New-Object -ComObject WScript.Shell).CreateShortcut($oldShortcut)
        if ($oldLink.TargetPath -eq (Join-Path $destination 'agentdictate.exe')) { Remove-Item -LiteralPath $oldShortcut }
    }
    $uninstall = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\AgentDictate'
    New-Item -Path $uninstall -Force | Out-Null
    New-ItemProperty -Path $uninstall -Name DisplayName -Value 'AgentDictate' -Force | Out-Null
    New-ItemProperty -Path $uninstall -Name DisplayIcon -Value (Join-Path $destination 'agentdictate.ico') -Force | Out-Null
    New-ItemProperty -Path $uninstall -Name InstallLocation -Value $destination -Force | Out-Null
    $command = 'powershell.exe -NoProfile -ExecutionPolicy Bypass -File "' + (Join-Path $destination 'uninstall.ps1') + '"'
    New-ItemProperty -Path $uninstall -Name UninstallString -Value $command -Force | Out-Null
}
Write-Host "Installed AgentDictate in $destination"
if (!$NoStart) { Start-Process -FilePath (Join-Path $destination 'agentdictate.exe') -WindowStyle Normal }
