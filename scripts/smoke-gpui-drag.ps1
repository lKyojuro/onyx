# Smoke test for the GPUI spike's external drag-out (`crates/onyx-ui-gpui`).
#
# The hand-over to OLE `DoDragDrop` depends on platform behaviour no unit test
# reaches: window capture, out-of-bounds pointer coordinates and the drop target
# answering. This script drives a real gesture with synthesized mouse input and
# drops one file from a throwaway folder onto the desktop, then checks that the
# file actually moved and that the app reported `Moved`.
#
# Everything happens in a temp folder plus one file on the desktop that the
# script removes again.
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File scripts/smoke-gpui-drag.ps1 [-ExePath <path>]
#
# Exit code 0 means the drag-out works, 1 means it failed.

param(
    [string]$ExePath = "crates/onyx-ui-gpui/target/debug/onyx-ui-gpui.exe"
)

$ErrorActionPreference = 'Stop'

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public class OnyxDragSmoke {
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, UIntPtr e);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
}
'@

$repo = Split-Path -Parent $PSScriptRoot
if (-not [System.IO.Path]::IsPathRooted($ExePath)) { $ExePath = Join-Path $repo $ExePath }
if (-not (Test-Path $ExePath)) { Write-Error "Executable not found: $ExePath"; exit 1 }

$work = Join-Path $env:TEMP 'onyx-drag-smoke'
$payload = Join-Path $work 'payload'
$fileName = 'onyx-drag-smoke.txt'
$desktop = Join-Path $env:USERPROFILE "Desktop\$fileName"
$log = Join-Path $env:APPDATA 'onyx\logs\drag-debug.log'

if (Test-Path $work) { Remove-Item $work -Recurse -Force }
New-Item -ItemType Directory -Path $payload -Force | Out-Null
'drag smoke' | Set-Content (Join-Path $payload $fileName)
Remove-Item $desktop -Force -ErrorAction SilentlyContinue
if (Test-Path $log) { Remove-Item $log -Force }

$env:ONYX_DEBUG_DRAG = '1'
$proc = Start-Process -FilePath $ExePath -ArgumentList $payload -PassThru
Start-Sleep -Seconds 4
$proc.Refresh()
$hwnd = $proc.MainWindowHandle
if ($hwnd -eq [IntPtr]::Zero) { Write-Error 'The app opened no window.'; exit 1 }

# A small, known window size so there is desktop next to it to drop onto.
[void][OnyxDragSmoke]::SetWindowPos($hwnd, [IntPtr]::Zero, 60, 60, 820, 560, 0x0040)
Start-Sleep -Milliseconds 700
[void][OnyxDragSmoke]::SetForegroundWindow($hwnd)
Start-Sleep -Milliseconds 500

$rect = New-Object OnyxDragSmoke+RECT
[void][OnyxDragSmoke]::GetWindowRect($hwnd, [ref]$rect)

$fromX = [int]($rect.Left + 320)
$fromY = [int]($rect.Top + 170)
$toX = [int]($rect.Right + 260)
$toY = [int]($rect.Top + 320)

[void][OnyxDragSmoke]::SetCursorPos($fromX, $fromY)
Start-Sleep -Milliseconds 350
[OnyxDragSmoke]::mouse_event(0x0002, 0, 0, 0, [UIntPtr]::Zero)   # LEFTDOWN
Start-Sleep -Milliseconds 250
for ($step = 1; $step -le 10; $step++) {
    [void][OnyxDragSmoke]::SetCursorPos(
        [int]($fromX + ($toX - $fromX) * $step / 10),
        [int]($fromY + ($toY - $fromY) * $step / 10)
    )
    Start-Sleep -Milliseconds 70
}
Start-Sleep -Milliseconds 900
[OnyxDragSmoke]::mouse_event(0x0004, 0, 0, 0, [UIntPtr]::Zero)   # LEFTUP
Start-Sleep -Milliseconds 1500

$dropped = Test-Path $desktop
$stillThere = Test-Path (Join-Path $payload $fileName)
$moved = (Test-Path $log) -and ((Get-Content $log) -match 'outcome=Ok\("Moved"\)')

Get-Process -Id $proc.Id -ErrorAction SilentlyContinue | Stop-Process -Force
Remove-Item $desktop -Force -ErrorAction SilentlyContinue
Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue

Write-Output "dropped on desktop : $dropped"
Write-Output "left in payload    : $stillThere"
Write-Output "app reported moved : $moved"

if ($dropped -and -not $stillThere -and $moved) {
    Write-Output 'PASS'
    exit 0
}
Write-Output 'FAIL'
if (Test-Path $log) { Write-Output '--- drag-debug.log ---'; Get-Content $log | Select-Object -Last 15 }
exit 1
