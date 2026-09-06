$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
foreach ($file in @('install.ps1','run.ps1','run-tests.ps1','packaging/build-windows.ps1','packaging/windows/install.ps1','packaging/windows/uninstall.ps1','packaging/windows/shortcuts.ps1','scripts/test-windows-overlay.ps1')) {
    $tokens = $null; $parseErrors = $null
    [Management.Automation.Language.Parser]::ParseFile((Join-Path $root $file), [ref]$tokens, [ref]$parseErrors) | Out-Null
    if ($parseErrors) { throw "$file has PowerShell syntax errors: $parseErrors" }
}
$fixture = Join-Path $root ('target\windows-install-test-' + [guid]::NewGuid().ToString('N'))
$package = Join-Path $fixture 'package'
$destination = Join-Path $fixture 'installed with spaces'
New-Item -ItemType Directory -Force -Path $package | Out-Null
foreach ($name in @('agentdictate.exe','agentdictated.exe')) {
    Copy-Item -LiteralPath (Join-Path $root "target\debug\$name") -Destination $package
}
Copy-Item -LiteralPath (Join-Path $root 'assets\agentdictate.ico') -Destination $package
Copy-Item -LiteralPath (Join-Path $root 'LICENSE') -Destination $package
Get-ChildItem -LiteralPath (Join-Path $PSScriptRoot 'windows') -File | Copy-Item -Destination $package
$previous = $env:AGENTDICTATE_DATA_HOME
try {
    $env:AGENTDICTATE_DATA_HOME = Join-Path $fixture 'isolated-data'
    & (Join-Path $package 'install.ps1') -InstallDirectory $destination -NoStart -NoShortcuts
    . (Join-Path $package 'shortcuts.ps1')
    $testLink = Join-Path $fixture 'AgentDictate.lnk'
    New-AgentDictateShortcut $testLink $destination
    $testFolder = (New-Object -ComObject Shell.Application).Namespace($fixture)
    if ($testFolder.ParseName('AgentDictate.lnk').ExtendedProperty('System.AppUserModel.ID') -ne 'local.agentdictate.AgentDictate') { throw 'Shortcut identity was not persisted.' }
    if ((Get-Item -LiteralPath (Join-Path $destination 'agentdictate.exe')).VersionInfo.FileDescription -ne 'AgentDictate') { throw 'Executable application metadata is missing.' }
    foreach ($name in @('agentdictate.exe','agentdictated.exe','agentdictate.ico','uninstall.ps1')) {
        if ((Get-FileHash -LiteralPath (Join-Path $package $name)).Hash -ne (Get-FileHash -LiteralPath (Join-Path $destination $name)).Hash) { throw "Installed $name differs from package." }
    }
    Set-Content -LiteralPath (Join-Path $destination 'unrelated.txt') -Value 'preserve me'
    & (Join-Path $destination 'uninstall.ps1') -NoShortcuts
    if (Test-Path -LiteralPath (Join-Path $destination 'agentdictate.exe')) { throw 'Uninstall left the application executable.' }
    if (!(Test-Path -LiteralPath (Join-Path $destination 'unrelated.txt'))) { throw 'Uninstall removed an unrelated file.' }
    Write-Host 'Windows package syntax, installation with spaces, file integrity, and selective uninstall passed without starting the UI or changing registry/shortcuts.'
} finally { $env:AGENTDICTATE_DATA_HOME = $previous }
