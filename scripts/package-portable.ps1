# Builds and packages the Onyx portable Windows x64 distribution.
# Usage:
#   .\scripts\package-portable.ps1                    # build + package into .\dist
#   .\scripts\package-portable.ps1 -SkipBuild         # package an existing release build
#   .\scripts\package-portable.ps1 -OutputDir D:\out   # custom output directory
[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [string]$OutputDir = "dist"
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
$OutputDir = Join-Path $repoRoot $OutputDir
$stageDir = Join-Path $OutputDir "onyx-portable-win64"
$zipPath = Join-Path $OutputDir "onyx-portable-win64.zip"

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

$exePath = Join-Path $repoRoot "target\release\onyx.exe"
if (-not (Test-Path $exePath)) {
    throw "Release binary not found at '$exePath'. Run without -SkipBuild or build manually first."
}

Write-Host "==> Staging portable package" -ForegroundColor Cyan
if (Test-Path $stageDir) {
    Remove-Item $stageDir -Recurse -Force
}
New-Item -ItemType Directory -Path $stageDir | Out-Null

Copy-Item $exePath -Destination $stageDir
Copy-Item (Join-Path $repoRoot "README.md") -Destination $stageDir
Copy-Item (Join-Path $repoRoot "LICENSE") -Destination $stageDir

$brandingDir = Join-Path $stageDir "assets\branding"
New-Item -ItemType Directory -Path $brandingDir | Out-Null
Copy-Item (Join-Path $repoRoot "assets\branding\onyx-icon.svg") -Destination $brandingDir
$icoPath = Join-Path $repoRoot "assets\branding\onyx.ico"
if (Test-Path $icoPath) {
    Copy-Item $icoPath -Destination $brandingDir
}

Write-Host "==> Creating ZIP archive" -ForegroundColor Cyan
Compress-Archive -Path (Join-Path $stageDir "*") -DestinationPath $zipPath -Force

$hash = (Get-FileHash -Path $zipPath -Algorithm SHA256).Hash
$sha256Path = "$zipPath.sha256"
"$hash  onyx-portable-win64.zip" | Out-File -FilePath $sha256Path -Encoding ascii

Write-Host ""
Write-Host "Package created:" -ForegroundColor Green
Write-Host "  $zipPath"
Write-Host "  $sha256Path"
Write-Host "SHA-256: $hash"
