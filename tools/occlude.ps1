# Test: is the window left unpainted when it is uncovered?
#   1. park the window in the clear, TOPMOST, let it settle fully, capture A
#   2. push it to the bottom of the z-order so whatever is behind covers it
#   3. raise it back to the top, capture B
# If A and B differ, being uncovered is what breaks the paint.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools/occlude.ps1
param(
  [string]$Process = "rgui-todo",
  [int]$X = 1600,
  [int]$Y = 300,
  [string]$Prefix = "E:\work\rgui\tmp\oc"
)

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class OC {
  public delegate bool EnumProc(IntPtr h, IntPtr p);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr p);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool MoveWindow(IntPtr h, int x, int y, int w, int ht, bool repaint);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int w, int ht, uint flags);
  [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(POINT p);
  [DllImport("user32.dll")] public static extern IntPtr GetAncestor(IntPtr h, uint flags);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
}
"@

$procs = Get-Process -Name $Process -ErrorAction SilentlyContinue
if (-not $procs) { Write-Output "no process named $Process"; exit 1 }
$pids = @($procs | ForEach-Object { [uint32]$_.Id })

$best = [IntPtr]::Zero; $bestArea = 0
$cb = [OC+EnumProc]{
  param($h, $p)
  $q = [uint32]0
  [OC]::GetWindowThreadProcessId($h, [ref]$q) | Out-Null
  if (($pids -contains $q) -and [OC]::IsWindowVisible($h)) {
    $sb = New-Object System.Text.StringBuilder 256
    [OC]::GetWindowTextW($h, $sb, 256) | Out-Null
    if ($sb.Length -gt 0) {
      $r = New-Object OC+RECT
      [OC]::GetWindowRect($h, [ref]$r) | Out-Null
      $a = ($r.R - $r.L) * ($r.B - $r.T)
      if ($a -gt $script:bestArea) { $script:best = $h; $script:bestArea = $a }
    }
  }
  return $true
}
[OC]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null

$r = New-Object OC+RECT; [OC]::GetWindowRect($best, [ref]$r) | Out-Null
$W = $r.R - $r.L; $H = $r.B - $r.T
$saveL = $r.L; $saveT = $r.T
Write-Output "window $W x $H, was at $saveL,$saveT"

$TOPMOST = [IntPtr](-1); $NOTOPMOST = [IntPtr](-2); $BOTTOM = [IntPtr](1)
$SWP_NOSIZE = 0x0001; $SWP_NOMOVE = 0x0002; $SWP_NOACTIVATE = 0x0010

function Shoot([string]$path) {
  $bmp = New-Object System.Drawing.Bitmap $W, $H
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.CopyFromScreen($X, $Y, 0, 0, (New-Object System.Drawing.Size $W, $H))
  $g.Dispose()
  $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
  return $bmp
}
function TopAt([int]$px, [int]$py) {
  $p = New-Object OC+POINT; $p.X = $px; $p.Y = $py
  $h = [OC]::WindowFromPoint($p); $t = [OC]::GetAncestor($h, 2)
  $q = [uint32]0; [OC]::GetWindowThreadProcessId($t, [ref]$q) | Out-Null
  (Get-Process -Id $q -ErrorAction SilentlyContinue).ProcessName
}

[OC]::SetWindowPos($best, $TOPMOST, 0, 0, 0, 0, $SWP_NOSIZE -bor $SWP_NOACTIVATE) | Out-Null
[OC]::MoveWindow($best, $X, $Y, $W, $H, $true) | Out-Null
Start-Sleep -Milliseconds 2500
$A = Shoot "$Prefix-a.png"
Write-Output "A: top window 40px down = $(TopAt ($X + 120) ($Y + 40))"

[OC]::SetWindowPos($best, $BOTTOM, 0, 0, 0, 0, $SWP_NOSIZE -bor $SWP_NOMOVE -bor $SWP_NOACTIVATE) | Out-Null
Start-Sleep -Milliseconds 900
Write-Output "covered: top window 40px down = $(TopAt ($X + 120) ($Y + 40))"

[OC]::SetWindowPos($best, $TOPMOST, 0, 0, 0, 0, $SWP_NOSIZE -bor $SWP_NOMOVE -bor $SWP_NOACTIVATE) | Out-Null
Start-Sleep -Milliseconds 900
$B = Shoot "$Prefix-b.png"
Write-Output "B: top window 40px down = $(TopAt ($X + 120) ($Y + 40))"

$cx = [int]($W / 2)
$firstDiff = -1; $lastDiff = -1; $count = 0
for ($y = 0; $y -lt $H; $y += 2) {
  $a = $A.GetPixel($cx, $y); $b = $B.GetPixel($cx, $y)
  $d = [Math]::Abs($a.R - $b.R) + [Math]::Abs($a.G - $b.G) + [Math]::Abs($a.B - $b.B)
  if ($d -gt 12) {
    if ($firstDiff -lt 0) { $firstDiff = $y }
    $lastDiff = $y; $count++
  }
}
Write-Output "rows differing A vs B (centre column, step 2): $count"
if ($firstDiff -ge 0) {
  $a = $A.GetPixel($cx, $firstDiff); $b = $B.GetPixel($cx, $firstDiff)
  Write-Output ("  first y=$firstDiff  #{0:x2}{1:x2}{2:x2} -> #{3:x2}{4:x2}{5:x2}" -f $a.R,$a.G,$a.B,$b.R,$b.G,$b.B)
}
$A.Dispose(); $B.Dispose()

[OC]::MoveWindow($best, $saveL, $saveT, $W, $H, $true) | Out-Null
[OC]::SetWindowPos($best, $NOTOPMOST, 0, 0, 0, 0, $SWP_NOSIZE -bor $SWP_NOACTIVATE) | Out-Null
Write-Output "restored to $saveL,$saveT; saved $Prefix-a.png / $Prefix-b.png"
