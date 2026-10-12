# Builds an MSI installer. Run from a normal PowerShell on Windows:
#   .\scripts\build-msi.ps1
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

# WiX Toolset v3 provides candle.exe / light.exe and sets %WIX%.
if (-not $env:WIX -and -not (Get-Command candle.exe -ErrorAction SilentlyContinue)) {
    throw "WiX Toolset v3 not found. Install WiX 3.x (e.g. 'winget install WiXToolset.WiXToolset' or from wixtoolset.org), then open a NEW terminal and re-run."
}

if (-not (Get-Command cargo-wix -ErrorAction SilentlyContinue)) {
    Write-Host "Installing cargo-wix..."
    cargo install cargo-wix
}

# Builds in release mode, then packages using wix\main.wxs
cargo wix --nocapture

Write-Host ""
Get-ChildItem target\wix\*.msi | ForEach-Object { Write-Host $_.FullName }
