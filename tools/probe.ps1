# Sample actual screen pixels around the app's windows to find out which parts of a
# window are opaque and which let the desktop through.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools/probe.ps1 -Process rgui-todo
#
# Prints "#rrggbb" hex per sample point. ASCII-only source (see winlist.ps1).
param([string]$Process = "rgui-todo")

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class PB {
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

# Pick the visible window with the largest area = the main window.
$best = [IntPtr]::Zero; $bestArea = 0; $bestRect = $null
$cb = [PB+EnumProc]{
  param($h, $p)
  $pid2 = [uint32]0
  [PB]::GetWindowThreadProcessId($h, [ref]$pid2) | Out-Null
  if (($pids -contains $pid2) -and [PB]::IsWindowVisible($h)) {
    $sb = New-Object System.Text.StringBuilder 512
    [PB]::GetWindowTextW($h, $sb, 512) | Out-Null
    if ($sb.Length -gt 0) {
      $r = New-Object PB+RECT
      [PB]::GetWindowRect($h, [ref]$r) | Out-Null
      $area = ($r.R - $r.L) * ($r.B - $r.T)
      if ($area -gt $bestArea) { $script:best = $h; $script:bestArea = $area; $script:bestRect = $r }
    }
  }
  return $true
}
[PB]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
if ($best -eq [IntPtr]::Zero) { Write-Output "no visible window"; exit 1 }
$r = $bestRect
Write-Output ("main hwnd=0x{0:x} rect={1},{2} {3}x{4} area={5}" -f [int64]$best, $r.L, $r.T, ($r.R-$r.L), ($r.B-$r.T), $bestArea)

$bmp = New-Object System.Drawing.Bitmap 1, 1
$g = [System.Drawing.Graphics]::FromImage($bmp)
$size = New-Object System.Drawing.Size 1, 1
function Px([int]$x, [int]$y, [string]$what) {
  $g.CopyFromScreen($x, $y, 0, 0, $size)
  $c = $bmp.GetPixel(0, 0)
  Write-Output ("  {0,-26} ({1},{2}) = #{3:x2}{4:x2}{5:x2} a={6}" -f $what, $x, $y, $c.R, $c.G, $c.B, $c.A)
}

$L = $r.L; $T = $r.T; $W = $r.R - $r.L; $H = $r.B - $r.T
$cx = $L + [int]($W / 2)

# Bring the window to the very front so we are sampling IT, not whatever was on top.
[PB]::SetWindowPos($best, [IntPtr](-1), 0, 0, 0, 0, 0x0001 -bor 0x0002 -bor 0x0010) | Out-Null   # TOPMOST|NOSIZE|NOACTIVATE
Start-Sleep -Milliseconds 500

Write-Output "--- sampling inside the window rect (window forced topmost) ---"
Px $cx ($T + 2)  "top edge +2"
Px $cx ($T + 22) "title bar mid"
Px $cx ($T + 60) "below titlebar"
Px $cx ($T + 100) "input bar area"
Px ($L + 20) ($T + 150) "left margin +150"
Px $cx ($T + 300) "middle"
Px $cx ($H + $T - 60) "bottom -60"
Px $cx ($H + $T - 20) "bottom -20"
Px $cx ($H + $T - 2)  "bottom edge -2"

Write-Output "--- same pixels after moving the window away by +400x ---"
[PB]::MoveWindow($best, $L + 400, $T, $W, $H, $false) | Out-Null
Start-Sleep -Milliseconds 500
Px $cx ($T + 2)  "top edge +2 (now desktop?)"
Px $cx ($T + 300) "middle (now desktop?)"
Px $cx ($H + $T - 20) "bottom -20 (now desktop?)"

Write-Output "--- empty desktop sample for reference ---"
Px 900 700 "desktop far from windows"

[PB]::MoveWindow($best, $L, $T, $W, $H, $true) | Out-Null
[PB]::SetWindowPos($best, [IntPtr](-2), 0, 0, 0, 0, 0x0001 -bor 0x0002 -bor 0x0010) | Out-Null   # NOTOPMOST
$g.Dispose(); $bmp.Dispose()
