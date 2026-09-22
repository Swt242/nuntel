# Walk a grid of points across the main window and report, for each: the colour a
# screen capture would show there, and which window is on top at that point.
# Points come from the real window rect, so no argument parsing games.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools/whotop.ps1 -Process nuntel
#
# If a point reports top=nuntel but a colour that is NOT one of the app's dark
# colours, the app is leaving that part of its own window unpainted/transparent.
param([string]$Process = "nuntel")

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class WT {
  public delegate bool EnumProc(IntPtr h, IntPtr p);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr p);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(POINT p);
  [DllImport("user32.dll")] public static extern IntPtr GetAncestor(IntPtr h, uint flags);
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
}
"@

$procs = Get-Process -Name $Process -ErrorAction SilentlyContinue
if (-not $procs) { Write-Output "no process named $Process"; exit 1 }
$pids = @($procs | ForEach-Object { [uint32]$_.Id })

$best = [IntPtr]::Zero; $bestArea = 0; $script:br = $null
$cb = [WT+EnumProc]{
  param($h, $p)
  $q = [uint32]0
  [WT]::GetWindowThreadProcessId($h, [ref]$q) | Out-Null
  if (($pids -contains $q) -and [WT]::IsWindowVisible($h)) {
    $sb = New-Object System.Text.StringBuilder 256
    [WT]::GetWindowTextW($h, $sb, 256) | Out-Null
    if ($sb.Length -gt 0) {
      $r = New-Object WT+RECT
      [WT]::GetWindowRect($h, [ref]$r) | Out-Null
      $a = ($r.R - $r.L) * ($r.B - $r.T)
      if ($a -gt $script:bestArea) { $script:best = $h; $script:bestArea = $a; $script:br = $r }
    }
  }
  return $true
}
[WT]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
$r = $script:br
$W = $r.R - $r.L; $H = $r.B - $r.T
Write-Output ("window 0x{0:x} at {1},{2} size {3}x{4}" -f [int64]$best, $r.L, $r.T, $W, $H)

$bmp = New-Object System.Drawing.Bitmap 1, 1
$g = [System.Drawing.Graphics]::FromImage($bmp)
$size = New-Object System.Drawing.Size 1, 1

$row = $r.T + 20
while ($row -le ($r.B - 2)) {
  $line = "y={0,4} (rel {1,4}): " -f $row, ($row - $r.T)
  foreach ($frac in @(0.2, 0.5, 0.8)) {
    $x = $r.L + [int]($W * $frac)
    $p = New-Object WT+POINT; $p.X = $x; $p.Y = $row
    $h = [WT]::WindowFromPoint($p)
    $top = [WT]::GetAncestor($h, 2)
    $q = [uint32]0
    [WT]::GetWindowThreadProcessId($top, [ref]$q) | Out-Null
    $name = (Get-Process -Id $q -ErrorAction SilentlyContinue).ProcessName
    $g.CopyFromScreen($x, $row, 0, 0, $size)
    $c = $bmp.GetPixel(0, 0)
    $tag = if ($name -eq $Process) { "app " } else { $name }
    $line += ("{0}:#{1:x2}{2:x2}{3:x2}  " -f $tag, $c.R, $c.G, $c.B)
  }
  Write-Output $line
  $row += 40
}
$g.Dispose(); $bmp.Dispose()
