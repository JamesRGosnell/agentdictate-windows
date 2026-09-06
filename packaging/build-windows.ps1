[CmdletBinding()]
param([switch]$SkipBuild)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
Push-Location $root
try {
    if (!$SkipBuild) {
        if (Get-Process cargo,rustc,link -ErrorAction SilentlyContinue) { throw 'Another Rust/linker build is running. Wait for it before packaging.' }
        if ((Get-PSDrive -Name ([IO.Path]::GetPathRoot($root).Substring(0,1))).Free -lt 15GB) { throw 'At least 15 GB free is required for a release build.' }
        & cargo build --locked --release -p agentdictate-app --features desktop --bins
        if ($LASTEXITCODE) { throw 'Release build failed.' }
    }
    $stage = Join-Path $root 'dist\AgentDictate-Windows-x64'
    New-Item -ItemType Directory -Force -Path $stage | Out-Null
    foreach ($name in @('agentdictate.exe','agentdictated.exe')) {
        Copy-Item -LiteralPath (Join-Path $root "target\release\$name") -Destination $stage -Force
    }
    Copy-Item -LiteralPath (Join-Path $root 'assets\agentdictate.ico') -Destination $stage -Force
    Copy-Item -LiteralPath (Join-Path $root 'LICENSE') -Destination $stage -Force
    # Deploy the release VC runtime beside the app so a fresh PC does not need
    # an administrator to install a machine-wide redistributable first.
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    $visualStudio = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if (!$visualStudio) { throw 'Could not locate the Visual C++ redistributable directory.' }
    $crt = Get-ChildItem -LiteralPath (Join-Path $visualStudio 'VC\Redist\MSVC') -Directory |
        ForEach-Object { Get-ChildItem -Path (Join-Path $_.FullName 'x64\Microsoft.VC*.CRT\vcruntime140.dll') -ErrorAction SilentlyContinue } |
        Sort-Object FullName -Descending | Select-Object -First 1
    if (!$crt) { throw 'Visual C++ release redistributable files are missing.' }
    foreach ($name in @('vcruntime140.dll','vcruntime140_1.dll')) {
        $library = Join-Path $crt.DirectoryName $name
        if (Test-Path -LiteralPath $library) { Copy-Item -LiteralPath $library -Destination $stage -Force }
    }
    Get-ChildItem -LiteralPath (Join-Path $PSScriptRoot 'windows') -File | Copy-Item -Destination $stage -Force
    $zip = Join-Path $root 'dist\AgentDictate-Windows-x64.zip'
    Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip -Force
    (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash + '  AgentDictate-Windows-x64.zip' | Set-Content -LiteralPath ($zip + '.sha256') -Encoding ascii
    Write-Host "Package: $zip"
} finally { Pop-Location }
