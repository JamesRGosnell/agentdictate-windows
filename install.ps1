[CmdletBinding()]
param([switch]$NoStart, [switch]$Force)
$ErrorActionPreference = 'Stop'
& (Join-Path $PSScriptRoot 'packaging\build-windows.ps1')
& (Join-Path $PSScriptRoot 'dist\AgentDictate-Windows-x64\install.ps1') -NoStart:$NoStart -Force:$Force
