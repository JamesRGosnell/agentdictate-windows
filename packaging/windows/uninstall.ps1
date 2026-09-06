[CmdletBinding()]
param([switch]$NoShortcuts)
$ErrorActionPreference = 'Stop'
$directory = [IO.Path]::GetFullPath($PSScriptRoot)
$executable = Join-Path $directory 'agentdictate.exe'
if (!(Test-Path -LiteralPath $executable)) { throw 'Run the uninstaller from the installed AgentDictate directory.' }
$stop = Start-Process -FilePath $executable -ArgumentList 'quit' -WindowStyle Hidden -PassThru
if (!$stop.WaitForExit(15000)) { throw 'AgentDictate has not closed. Quit from the tray before uninstalling.' }
# Delete only known installed files. Never recursively remove a supplied path or user data.
foreach ($name in @('agentdictate.exe','agentdictated.exe','agentdictate.ico','README.txt','LICENSE','shortcuts.ps1','vcruntime140.dll','vcruntime140_1.dll')) {
    $path = [IO.Path]::GetFullPath((Join-Path $directory $name))
    if ([IO.Path]::GetDirectoryName($path) -ne $directory) { throw 'Invalid installation path.' }
    if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path -Force }
}
if (!$NoShortcuts) {
    $menu = Join-Path ([Environment]::GetFolderPath('Programs')) 'AgentDictate'
    foreach ($name in @('AgentDictate.lnk','Sign in with ChatGPT.lnk')) {
        $path = Join-Path $menu $name
        if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path -Force }
    }
    foreach ($folder in @([Environment]::GetFolderPath('Programs'), [Environment]::GetFolderPath('Desktop'))) {
        $shortcut = Join-Path $folder 'AgentDictate.lnk'
        if (Test-Path -LiteralPath $shortcut) {
            $link = (New-Object -ComObject WScript.Shell).CreateShortcut($shortcut)
            if ($link.TargetPath -eq $executable) { Remove-Item -LiteralPath $shortcut }
        }
    }
    Remove-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name AgentDictate -ErrorAction SilentlyContinue
    $registration = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\AgentDictate'
    if (Test-Path -LiteralPath $registration) { Remove-Item -LiteralPath $registration }
}
Write-Host 'AgentDictate was removed. Your recordings, history, settings, and sign-in are preserved in LocalAppData\AgentDictate.'
