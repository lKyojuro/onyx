# Stages and builds the Onyx Windows 10/11 x64 MSIX package using Windows SDK makeappx.exe.
# Usage:
#   .\scripts\package-msix.ps1
#   .\scripts\package-msix.ps1 -SkipBuild -OutputDir dist -Version 0.1.0
[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [string]$OutputDir = "dist",
    [string]$Version = "",
    [string]$BinaryPath = ""
)

$ErrorActionPreference = "Stop"

function New-PngFromIcon {
    param(
        [string]$IconPath,
        [string]$DestinationPath,
        [int]$Width,
        [int]$Height
    )

    Add-Type -AssemblyName System.Drawing
    $icon = [System.Drawing.Icon]::new($IconPath)
    $bitmap = [System.Drawing.Bitmap]::new($Width, $Height)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    try {
        $graphics.Clear([System.Drawing.Color]::Transparent)
        $graphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
        $graphics.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::HighQuality
        $side = [Math]::Min($Width, $Height)
        $pad = [Math]::Max(2, [int]($side * 0.1))
        $drawSize = $side - (2 * $pad)
        $x = [int](($Width - $drawSize) / 2)
        $y = [int](($Height - $drawSize) / 2)
        $iconBitmap = $icon.ToBitmap()
        try {
            $graphics.DrawImage($iconBitmap, $x, $y, $drawSize, $drawSize)
        }
        finally {
            $iconBitmap.Dispose()
        }
        $bitmap.Save($DestinationPath, [System.Drawing.Imaging.ImageFormat]::Png)
    }
    finally {
        $graphics.Dispose()
        $bitmap.Dispose()
        $icon.Dispose()
    }
}

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

$msixVersion = if ($Version -match '^\d+\.\d+\.\d+\.\d+$') {
    $Version
} elseif ($Version -match '^(\d+\.\d+\.\d+)') {
    "$($Matches[1]).0"
} else {
    "0.1.0.0"
}

$manifestTemplate = Join-Path $repoRoot "packaging\msix\AppxManifest.xml"
if (-not (Test-Path $manifestTemplate)) {
    throw "AppxManifest.xml not found at '$manifestTemplate'."
}

Write-Host "==> Validating AppxManifest.xml ($manifestTemplate)" -ForegroundColor Cyan
[xml]$manifestXml = Get-Content $manifestTemplate -Raw
if ($null -eq $manifestXml.Package -or $null -eq $manifestXml.Package.Identity -or $null -eq $manifestXml.Package.Applications) {
    throw "Invalid AppxManifest.xml: missing Package, Identity, or Applications elements."
}
Write-Host "AppxManifest.xml validation passed (Identity=$($manifestXml.Package.Identity.Name), Arch=$($manifestXml.Package.Identity.ProcessorArchitecture))." -ForegroundColor Green

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

$stageDir = Join-Path $OutputDir "onyx-msix-stage"
if (Test-Path $stageDir) {
    Remove-Item $stageDir -Recurse -Force
}
New-Item -ItemType Directory -Path $stageDir | Out-Null

Write-Host "==> Staging MSIX layout in '$stageDir'" -ForegroundColor Cyan
Copy-Item $exePath -Destination $stageDir
Copy-Item (Join-Path $repoRoot "README.md") -Destination $stageDir
Copy-Item (Join-Path $repoRoot "LICENSE") -Destination $stageDir

$brandingDir = Join-Path $stageDir "assets\branding"
New-Item -ItemType Directory -Path $brandingDir | Out-Null
$icoPath = Join-Path $repoRoot "assets\branding\onyx.ico"
Copy-Item $icoPath -Destination $brandingDir
Copy-Item (Join-Path $repoRoot "assets\branding\onyx-icon.svg") -Destination $brandingDir

$msixAssetsDir = Join-Path $stageDir "Assets"
New-Item -ItemType Directory -Path $msixAssetsDir -Force | Out-Null
New-PngFromIcon -IconPath $icoPath -DestinationPath (Join-Path $msixAssetsDir "StoreLogo.png") -Width 50 -Height 50
New-PngFromIcon -IconPath $icoPath -DestinationPath (Join-Path $msixAssetsDir "Square44x44Logo.png") -Width 44 -Height 44
New-PngFromIcon -IconPath $icoPath -DestinationPath (Join-Path $msixAssetsDir "Square150x150Logo.png") -Width 150 -Height 150
New-PngFromIcon -IconPath $icoPath -DestinationPath (Join-Path $msixAssetsDir "Wide310x150Logo.png") -Width 310 -Height 150

$manifestXml.Package.Identity.Version = $msixVersion
$stagedManifestPath = Join-Path $stageDir "AppxManifest.xml"
$manifestXml.Save($stagedManifestPath)

$makeappxCmd = Get-Command makeappx.exe -ErrorAction SilentlyContinue
$makeappxPath = if ($null -ne $makeappxCmd) { $makeappxCmd.Source } else { $null }

if ([string]::IsNullOrWhiteSpace($makeappxPath)) {
    $sdkCandidates = Get-ChildItem -Path "C:\Program Files (x86)\Windows Kits\10\bin\*\x64\makeappx.exe" -ErrorAction SilentlyContinue |
        Sort-Object FullName -Descending
    if ($sdkCandidates) {
        $makeappxPath = $sdkCandidates[0].FullName
    }
}

if ([string]::IsNullOrWhiteSpace($makeappxPath)) {
    Write-Warning "Windows SDK 'makeappx.exe' was not found on PATH or under Windows Kits 10."
    Write-Host "Install the Windows 10/11 SDK (winget install Microsoft.WindowsSDK.10.0.26100) to build .msix." -ForegroundColor Yellow
    Write-Host "MSIX layout staged and AppxManifest.xml validated at: $stageDir" -ForegroundColor Yellow
    exit 0
}

$msixName = "onyx-$Version-win64.msix"
$msixPath = Join-Path $OutputDir $msixName

Write-Host "==> Packing MSIX with '$makeappxPath'" -ForegroundColor Cyan
& $makeappxPath pack /d $stageDir /p $msixPath /o
if ($LASTEXITCODE -ne 0) {
    throw "makeappx.exe failed with exit code $LASTEXITCODE"
}
if (-not (Test-Path $msixPath)) {
    throw "Expected MSIX output was not created at '$msixPath'."
}

$hash = (Get-FileHash -Path $msixPath -Algorithm SHA256).Hash
$sha256Path = "$msixPath.sha256"
"$hash  $msixName" | Out-File -FilePath $sha256Path -Encoding ascii

Write-Host ""
Write-Host "MSIX package created:" -ForegroundColor Green
Write-Host "  $msixPath"
Write-Host "  $sha256Path"
Write-Host "SHA-256: $hash"
