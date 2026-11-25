# Builds the Onyx Windows Installer (.msi) package using WiX Toolset v4+.
# Usage:
#   .\scripts\package-wix.ps1
#   .\scripts\package-wix.ps1 -SkipBuild -OutputDir dist -Version 0.1.0
[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [string]$OutputDir = "dist",
    [string]$Version = "",
    [string]$BinaryPath = ""
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
if (-not [System.IO.Path]::IsPathRooted($OutputDir)) {
    $OutputDir = Join-Path $repoRoot $OutputDir
}

if ([string]::IsNullOrWhiteSpace($Version)) {
    $cargoToml = Get-Content (Join-Path $repoRoot "Cargo.toml") -Raw
    if ($cargoToml -match '(?m)^version\s*=\s*"([^"]+)"') {
        $Version = $Matches[1]
    } else {
        $Version = "0.1.0"
    }
}

$wxsPath = Join-Path $repoRoot "packaging\wix\onyx.wxs"
if (-not (Test-Path $wxsPath)) {
    throw "WiX source not found at '$wxsPath'."
}

Write-Host "==> Validating WiX XML ($wxsPath)" -ForegroundColor Cyan
[xml]$wxsXml = Get-Content $wxsPath -Raw
if ($null -eq $wxsXml.Wix -or $null -eq $wxsXml.Wix.Package) {
    throw "Invalid WiX manifest: missing <Wix> or <Package> root elements."
}
Write-Host "WiX XML validation passed (UpgradeCode=$($wxsXml.Wix.Package.UpgradeCode), Scope=$($wxsXml.Wix.Package.Scope))." -ForegroundColor Green

$wixCmd = Get-Command wix -ErrorAction SilentlyContinue
if ($null -eq $wixCmd) {
    Write-Warning "WiX Toolset v4+ ('wix' CLI) is not installed or not on PATH."
    Write-Host "To install WiX Toolset v4+, run one of:" -ForegroundColor Yellow
    Write-Host "  dotnet tool install --global wix"
    Write-Host "  winget install WiXToolset.WiXToolset"
    Write-Host "Exiting cleanly without building .msi (XML validation succeeded)." -ForegroundColor Yellow
    exit 0
}

if (-not $SkipBuild) {
    Write-Host "==> cargo build --release --bin onyx" -ForegroundColor Cyan
    Push-Location $repoRoot
    try {
        cargo build --release --bin onyx
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build failed with exit code $LASTEXITCODE"
        }
    }
    finally {
        Pop-Location
    }
}

if (-not [string]::IsNullOrWhiteSpace($BinaryPath)) {
    $exePath = $BinaryPath
} elseif (-not [string]::IsNullOrWhiteSpace($env:CARGO_TARGET_DIR)) {
    $exePath = Join-Path $env:CARGO_TARGET_DIR "release\onyx.exe"
} else {
    $exePath = Join-Path $repoRoot "target\release\onyx.exe"
}

if (-not (Test-Path $exePath)) {
    throw "Release binary not found at '$exePath'. Pass -BinaryPath, set CARGO_TARGET_DIR, or run without -SkipBuild."
}

if (-not (Test-Path $OutputDir)) {
    New-Item -ItemType Directory -Path $OutputDir | Out-Null
}

$msiName = "onyx-$Version-win64.msi"
$msiPath = Join-Path $OutputDir $msiName
$iconPath = Join-Path $repoRoot "assets\branding\onyx.ico"

Write-Host "==> Building MSI package with WiX ($($wixCmd.Source))" -ForegroundColor Cyan
& $wixCmd.Source build $wxsPath `
    -arch x64 `
    -d "ProductVersion=$Version" `
    -d "ExePath=$exePath" `
    -d "RepoRoot=$repoRoot" `
    -d "IconPath=$iconPath" `
    -o $msiPath

if ($LASTEXITCODE -ne 0) {
    throw "wix build failed with exit code $LASTEXITCODE"
}
if (-not (Test-Path $msiPath)) {
    throw "Expected MSI output was not created at '$msiPath'."
}

$hash = (Get-FileHash -Path $msiPath -Algorithm SHA256).Hash
$sha256Path = "$msiPath.sha256"
"$hash  $msiName" | Out-File -FilePath $sha256Path -Encoding ascii

Write-Host ""
Write-Host "MSI package created:" -ForegroundColor Green
Write-Host "  $msiPath"
Write-Host "  $sha256Path"
Write-Host "SHA-256: $hash"
