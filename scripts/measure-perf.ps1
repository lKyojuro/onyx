# Measures real release binary size, startup latency, and idle memory usage for Onyx.
# Usage:
#   .\scripts\measure-perf.ps1
#   .\scripts\measure-perf.ps1 -BinaryPath D:\Onyx\target\release\onyx.exe -Runs 5 -IdleSeconds 3
[CmdletBinding()]
param(
    [string]$BinaryPath = "",
    [int]$Runs = 5,
    [int]$IdleSeconds = 3
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot

if (-not [string]::IsNullOrWhiteSpace($BinaryPath)) {
    $exePath = $BinaryPath
} elseif (-not [string]::IsNullOrWhiteSpace($env:CARGO_TARGET_DIR)) {
    $exePath = Join-Path $env:CARGO_TARGET_DIR "release\onyx.exe"
} else {
    $exePath = Join-Path $repoRoot "target\release\onyx.exe"
}

if (-not (Test-Path $exePath)) {
    throw "Release binary not found at '$exePath'. Pass -BinaryPath or set CARGO_TARGET_DIR."
}

$exeInfo = Get-Item $exePath
$sizeBytes = $exeInfo.Length
$sizeMB = [math]::Round($sizeBytes / 1MB, 2)

Write-Host "=== Onyx Release Performance Measurement ===" -ForegroundColor Cyan
Write-Host "Binary: $exePath"
Write-Host "Size:   $sizeMB MB ($sizeBytes bytes)"

$versionOutput = & $exePath --version
if ($LASTEXITCODE -ne 0) {
    throw "onyx.exe --version failed with exit code $LASTEXITCODE"
}
Write-Host "Version: $versionOutput"

# System metadata
$os = Get-CimInstance Win32_OperatingSystem
$cpu = Get-CimInstance Win32_Processor | Select-Object -First 1
$ramGB = [math]::Round($os.TotalVisibleMemorySize / 1MB, 2)
Write-Host "Host:   $($os.Caption) ($($os.Version)) | $($cpu.Name.Trim()) ($($cpu.NumberOfCores)C/$($cpu.NumberOfLogicalProcessors)T) | ${ramGB} GB RAM"
Write-Host ""

# 1. Measure --smoke-test across N runs
Write-Host "==> Measuring --smoke-test ($Runs runs)..." -ForegroundColor Cyan
$smokeTimings = @()
for ($i = 1; $i -le $Runs; $i++) {
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $out = & $exePath --smoke-test 2>&1
    $sw.Stop()
    if ($LASTEXITCODE -ne 0) {
        throw "Smoke test run $i failed: $out"
    }
    $ms = [math]::Round($sw.Elapsed.TotalMilliseconds, 2)
    $smokeTimings += $ms
    Write-Host ("  Run {0}: {1} ms" -f $i, $ms)
}

$sortedSmoke = $smokeTimings | Sort-Object
$smokeMin = $sortedSmoke[0]
$smokeMax = $sortedSmoke[-1]
$mid = [math]::Floor($sortedSmoke.Count / 2)
$smokeMedian = if ($sortedSmoke.Count % 2 -eq 1) {
    $sortedSmoke[$mid]
} else {
    [math]::Round(($sortedSmoke[$mid - 1] + $sortedSmoke[$mid]) / 2, 2)
}

Write-Host ("  Smoke-test Min/Median/Max: {0} ms / {1} ms / {2} ms" -f $smokeMin, $smokeMedian, $smokeMax) -ForegroundColor Green
Write-Host ""

# 2. Measure real GUI launch (process start to MainWindowHandle ready) & idle memory
Write-Host "==> Launching Onyx GUI to measure startup-to-window-ready and idle RAM (${IdleSeconds}s)..." -ForegroundColor Cyan
$psi = [System.Diagnostics.ProcessStartInfo]::new($exePath)
$psi.UseShellExecute = $false
$psi.WorkingDirectory = $repoRoot

$guiSw = [System.Diagnostics.Stopwatch]::StartNew()
$proc = [System.Diagnostics.Process]::Start($psi)

$windowReadyMs = $null
try {
    $timeoutMs = 15000
    while ($guiSw.ElapsedMilliseconds -lt $timeoutMs) {
        if ($proc.HasExited) {
            throw "Onyx exited prematurely with code $($proc.ExitCode) before showing a main window."
        }
        $proc.Refresh()
        if ($proc.MainWindowHandle -ne [IntPtr]::Zero) {
            $guiSw.Stop()
            $windowReadyMs = [math]::Round($guiSw.Elapsed.TotalMilliseconds, 2)
            break
        }
        Start-Sleep -Milliseconds 10
    }

    if ($null -eq $windowReadyMs) {
        throw "Timed out waiting for Onyx main window handle after ${timeoutMs}ms."
    }

    Write-Host ("  Process start -> MainWindowHandle ready: {0} ms" -f $windowReadyMs) -ForegroundColor Green

    Start-Sleep -Seconds $IdleSeconds
    $proc.Refresh()
    if ($proc.HasExited) {
        throw "Onyx exited unexpectedly during idle measurement with code $($proc.ExitCode)."
    }

    $wsBytes = $proc.WorkingSet64
    $pmBytes = $proc.PrivateMemorySize64
    $wsMB = [math]::Round($wsBytes / 1MB, 2)
    $pmMB = [math]::Round($pmBytes / 1MB, 2)

    Write-Host ("  Idle WorkingSet64:      {0} MB ({1} bytes)" -f $wsMB, $wsBytes) -ForegroundColor Green
    Write-Host ("  Idle PrivateMemory64:   {0} MB ({1} bytes)" -f $pmMB, $pmBytes) -ForegroundColor Green
}
finally {
    if ($null -ne $proc -and -not $proc.HasExited) {
        $null = $proc.CloseMainWindow()
        if (-not $proc.WaitForExit(3000)) {
            Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
        }
    }
    if ($null -ne $proc) {
        $proc.Dispose()
    }
}

Write-Host ""
[PSCustomObject]@{
    BinaryPath            = $exePath
    SizeBytes             = $sizeBytes
    SizeMB                = $sizeMB
    SmokeRunsMs           = $smokeTimings
    SmokeMinMs            = $smokeMin
    SmokeMedianMs         = $smokeMedian
    SmokeMaxMs            = $smokeMax
    WindowReadyMs         = $windowReadyMs
    IdleWorkingSetBytes   = $wsBytes
    IdleWorkingSetMB      = $wsMB
    IdlePrivateBytes      = $pmBytes
    IdlePrivateMB         = $pmMB
    OS                    = "$($os.Caption) ($($os.Version))"
    CPU                   = "$($cpu.Name.Trim()) ($($cpu.NumberOfCores)C/$($cpu.NumberOfLogicalProcessors)T)"
    RAM_GB                = $ramGB
}
