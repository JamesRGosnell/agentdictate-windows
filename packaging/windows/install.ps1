[CmdletBinding()]
param([string]$InstallDirectory = (Join-Path $env:LOCALAPPDATA 'Programs\AgentDictate'), [switch]$NoStart, [switch]$NoShortcuts, [switch]$Force)
$ErrorActionPreference = 'Stop'
$destination = [IO.Path]::GetFullPath($InstallDirectory)
if ([IO.Path]::GetPathRoot($destination).TrimEnd('\') -eq $destination.TrimEnd('\')) { throw 'Choose an application directory, not a drive root.' }
foreach ($name in @('agentdictate.exe', 'agentdictated.exe', 'agentdictate.ico')) {
    if (!(Test-Path -LiteralPath (Join-Path $PSScriptRoot $name))) { throw "Package is missing $name. Extract the whole ZIP first." }
}
$existing = Join-Path $destination 'agentdictate.exe'
$daemon = Join-Path $destination 'agentdictated.exe'
$files = @(@('vcruntime140.dll', 'vcruntime140_1.dll') | Where-Object { Test-Path -LiteralPath (Join-Path $PSScriptRoot $_) }) +
    @('agentdictate.exe', 'agentdictated.exe', 'agentdictate.ico', 'uninstall.ps1', 'shortcuts.ps1', 'README.txt', 'LICENSE')

function Get-InstalledDaemon {
    @(Get-Process agentdictated -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $daemon })
}
# Files replaced by an earlier install are renamed to <name>.<id>.old; remove the ones no longer in use.
function Remove-ReplacedFile {
    if (!(Test-Path -LiteralPath $destination)) { return }
    foreach ($name in $files) {
        Get-ChildItem -LiteralPath $destination -Filter "$name.*.old" -File -ErrorAction SilentlyContinue |
            ForEach-Object { Remove-Item -LiteralPath $_.FullName -Force -ErrorAction SilentlyContinue }
    }
}

# A running meeting copilot shares agentdictated.exe. Never end a live meeting unless asked to.
$copilots = @(Get-CimInstance Win32_Process -Filter "Name = 'agentdictated.exe'" -ErrorAction SilentlyContinue |
    Where-Object { $_.ExecutablePath -eq $daemon -and $_.CommandLine -like '*--copilot*' })
if ($copilots.Count -gt 0) {
    if (!$Force) { throw 'A meeting copilot is running. End the meeting copilot first, then run this installer again. To close it now and lose the live meeting, run the installer with -Force.' }
    foreach ($copilot in $copilots) { Stop-Process -Id $copilot.ProcessId -Force -ErrorAction SilentlyContinue }
}
Remove-ReplacedFile
$daemonStopped = $false
$replaced = New-Object System.Collections.Generic.List[object]
try {
    # Settings can start the daemon again, so close it first: ask its window to close, then stop it.
    foreach ($settings in @(Get-Process agentdictate -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $existing })) {
        if (!$settings.CloseMainWindow() -or !$settings.WaitForExit(5000)) {
            Stop-Process -Id $settings.Id -Force -ErrorAction SilentlyContinue
            [void]$settings.WaitForExit(5000)
        }
    }
    if (Test-Path -LiteralPath $existing) {
        $daemonStopped = (Get-InstalledDaemon).Count -gt 0
        $stop = Start-Process -FilePath $existing -ArgumentList 'quit' -WindowStyle Hidden -PassThru
        if (!$stop.WaitForExit(15000)) { throw 'AgentDictate is still closing. Quit it from the tray and run this installer again.' }
        $deadline = [DateTime]::UtcNow.AddSeconds(15)
        while ((Get-InstalledDaemon).Count -gt 0) {
            if ([DateTime]::UtcNow -gt $deadline) { throw 'AgentDictate has not finished closing.' }
            Start-Sleep -Milliseconds 100
        }
    }
    New-Item -ItemType Directory -Force -Path $destination | Out-Null
    foreach ($name in $files) {
        $target = Join-Path $destination $name
        $old = $null
        if (Test-Path -LiteralPath $target) {
            # NTFS renames a file that is still in use, so a straggling process cannot block the update.
            $old = '{0}.{1}.old' -f $target, [guid]::NewGuid().ToString('N')
            Move-Item -LiteralPath $target -Destination $old
        }
        $replaced.Add([pscustomobject]@{ Target = $target; Old = $old })
        Copy-Item -LiteralPath (Join-Path $PSScriptRoot $name) -Destination $target
    }
    if (!$NoShortcuts) {
        $menu = Join-Path ([Environment]::GetFolderPath('Programs')) 'AgentDictate'
        New-Item -ItemType Directory -Force -Path $menu | Out-Null
        . (Join-Path $PSScriptRoot 'shortcuts.ps1')
        New-AgentDictateShortcut (Join-Path ([Environment]::GetFolderPath('Programs')) 'AgentDictate.lnk') $destination
        New-AgentDictateShortcut (Join-Path ([Environment]::GetFolderPath('Desktop')) 'AgentDictate.lnk') $destination
        New-AgentDictateShortcut (Join-Path $menu 'Sign in with ChatGPT.lnk') $destination 'login'
        New-AgentDictateShortcut (Join-Path $menu 'Meeting Copilot.lnk') $destination 'copilot'
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
} catch {
    # Put the previous files back and restart their daemon, so a failed update never leaves dictation stopped.
    for ($index = $replaced.Count - 1; $index -ge 0; $index--) {
        $entry = $replaced[$index]
        Remove-Item -LiteralPath $entry.Target -Force -ErrorAction SilentlyContinue
        if ($entry.Old) { Move-Item -LiteralPath $entry.Old -Destination $entry.Target -Force -ErrorAction SilentlyContinue }
    }
    if ($daemonStopped -and (Test-Path -LiteralPath $daemon)) {
        # Explorer starts the daemon outside this shell's job object, so it outlives the installer.
        Start-Process -FilePath 'explorer.exe' -ArgumentList ('"{0}"' -f $daemon)
        Write-Warning 'The update failed; the previously installed AgentDictate was restarted.'
    }
    throw
}
Remove-ReplacedFile
Write-Host "Installed AgentDictate in $destination"
if (!$NoStart) { Start-Process -FilePath (Join-Path $destination 'agentdictate.exe') -WindowStyle Normal }
