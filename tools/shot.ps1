# Shoot the main window into a PNG, with it forced to the top of the z-order and
# moved to a clear part of the screen first -- otherwise whatever editor happens to
# be maximised is what you photograph. Restores position and z-order afterwards.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools/shot.ps1 -Out e:\work\rgui\tmp\shot.png
param(
  [string]$Process = "rgui-todo",
  [string]$Out = "$env:TEMP\rgui-shot.png",
  [int]$ParkX = 1600,
  [int]$ParkY = 60
)

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class SH {
  public delegate bool EnumProc(IntPtr h, IntPtr p);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr p);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool MoveWindow(IntPtr h, int x, int y, int w, int ht, bool repaint);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int w, int ht, uint flags);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
}
"@

$procs = Get-Process -Name $Process -ErrorAction SilentlyContinue
if (-not $procs) { Write-Output "no process named $Process"; exit 1 }
$pids = @($procs | ForEach-Object { [uint32]$_.Id })

$best = [IntPtr]::Zero; $bestArea = 0; $script:br = $null
$cb = [SH+EnumProc]{
  param($h, $p)
  $q = [uint32]0
  [SH]::GetWindowThreadProcessId($h, [ref]$q) | Out-Null
  if (($pids -contains $q) -and [SH]::IsWindowVisible($h)) {
    $sb = New-Object System.Text.StringBuilder 256
    [SH]::GetWindowTextW($h, $sb, 256) | Out-Null
    if ($sb.Length -gt 0) {
      $r = New-Object SH+RECT
      [SH]::GetWindowRect($h, [ref]$r) | Out-Null
      $a = ($r.R - $r.L) * ($r.B - $r.T)
      if ($a -gt $script:bestArea) { $script:best = $h; $script:bestArea = $a; $script:br = $r }
    }
  }
  return $true
}
[SH]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
$r = $script:br
$W = $r.R - $r.L; $H = $r.B - $r.T
$saveL = $r.L; $saveT = $r.T

$HWND_TOPMOST = [IntPtr](-1); $HWND_NOTOPMOST = [IntPtr](-2)
$SWP_NOSIZE = 0x0001; $SWP_NOACTIVATE = 0x0010
[SH]::SetWindowPos($best, $HWND_TOPMOST, 0, 0, 0, 0, $SWP_NOSIZE -bor $SWP_NOACTIVATE) | Out-Null
[SH]::MoveWindow($best, $ParkX, $ParkY, $W, $H, $true) | Out-Null
Start-Sleep -Milliseconds 900

$bmp = New-Object System.Drawing.Bitmap $W, $H
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($ParkX, $ParkY, 0, 0, (New-Object System.Drawing.Size $W, $H))
$dir = Split-Path -Parent $Out
if ($dir -and -not (Test-Path $dir)) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose(); $bmp.Dispose()

[SH]::MoveWindow($best, $saveL, $saveT, $W, $H, $true) | Out-Null
[SH]::SetWindowPos($best, $HWND_NOTOPMOST, 0, 0, 0, 0, $SWP_NOSIZE -bor $SWP_NOACTIVATE) | Out-Null

Write-Output "saved $Out ($W x $H), window restored to $saveL,$saveT"
