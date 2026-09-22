# Park the main window somewhere clear, shoot it, then resize it by one pixel and
# back (which forces Slint to do a fresh layout + repaint) and shoot it again.
# If the two differ, the window was showing stale/unpainted pixels.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools/nudge.ps1 -Prefix e:\work\rgui\tmp
param(
  [string]$Process = "nuntel",
  [string]$Prefix = "$env:TEMP\rgui",
  [int]$ParkX = 1600,
  [int]$ParkY = 60
)

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class ND {
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
$cb = [ND+EnumProc]{
  param($h, $p)
  $q = [uint32]0
  [ND]::GetWindowThreadProcessId($h, [ref]$q) | Out-Null
  if (($pids -contains $q) -and [ND]::IsWindowVisible($h)) {
    $sb = New-Object System.Text.StringBuilder 256
    [ND]::GetWindowTextW($h, $sb, 256) | Out-Null
    if ($sb.Length -gt 0) {
      $r = New-Object ND+RECT
      [ND]::GetWindowRect($h, [ref]$r) | Out-Null
      $a = ($r.R - $r.L) * ($r.B - $r.T)
      if ($a -gt $script:bestArea) { $script:best = $h; $script:bestArea = $a; $script:br = $r }
    }
  }
  return $true
}
[ND]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
$r = $script:br
$W = $r.R - $r.L; $H = $r.B - $r.T
$saveL = $r.L; $saveT = $r.T

$TOPMOST = [IntPtr](-1); $NOTOPMOST = [IntPtr](-2)
$SWP_NOSIZE = 0x0001; $SWP_NOACTIVATE = 0x0010

function Shoot([string]$path) {
  $bmp = New-Object System.Drawing.Bitmap $W, $H
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.CopyFromScreen($ParkX, $ParkY, 0, 0, (New-Object System.Drawing.Size $W, $H))
  $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
  $g.Dispose(); $bmp.Dispose()
}

# Where does the content start? Scan down the middle column and report the first row
# whose colour differs a lot from the row at the very top (= background showing through).
function FirstPaintedRow {
  $col = $ParkX + [int]($W / 2)
  $bmp = New-Object System.Drawing.Bitmap 1, 1
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $sz = New-Object System.Drawing.Size 1, 1
  $g.CopyFromScreen($col, $ParkY, 0, 0, $sz); $top = $bmp.GetPixel(0, 0)
  for ($y = 0; $y -lt $H; $y++) {
    $g.CopyFromScreen($col, ($ParkY + $y), 0, 0, $sz)
    $c = $bmp.GetPixel(0, 0)
    $d = [Math]::Abs($c.R - $top.R) + [Math]::Abs($c.G - $top.G) + [Math]::Abs($c.B - $top.B)
    if ($d -gt 24) { $g.Dispose(); $bmp.Dispose(); return $y }
  }
  $g.Dispose(); $bmp.Dispose(); return -1
}

[ND]::SetWindowPos($best, $TOPMOST, 0, 0, 0, 0, $SWP_NOSIZE -bor $SWP_NOACTIVATE) | Out-Null
[ND]::MoveWindow($best, $ParkX, $ParkY, $W, $H, $true) | Out-Null
Start-Sleep -Milliseconds 900
Shoot "$Prefix-before.png"
Write-Output "before: first painted row = $(FirstPaintedRow)"

# one-pixel resize and back
[ND]::MoveWindow($best, $ParkX, $ParkY, $W, ($H + 1), $true) | Out-Null
Start-Sleep -Milliseconds 700
[ND]::MoveWindow($best, $ParkX, $ParkY, $W, $H, $true) | Out-Null
Start-Sleep -Milliseconds 900
Shoot "$Prefix-after.png"
Write-Output "after : first painted row = $(FirstPaintedRow)"

[ND]::MoveWindow($best, $saveL, $saveT, $W, $H, $true) | Out-Null
[ND]::SetWindowPos($best, $NOTOPMOST, 0, 0, 0, 0, $SWP_NOSIZE -bor $SWP_NOACTIVATE) | Out-Null
Write-Output "saved $Prefix-before.png / $Prefix-after.png, window restored to $saveL,$saveT"
