# Park the main window at a known clear spot, TOPMOST it, screen-capture it, then force
# a fresh full repaint (one-pixel resize and back) and screen-capture again. Reports how
# many pixels changed and the first row that was "background" before but painted after.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools/repaint.ps1
param(
  [string]$Process = "rgui-todo",
  [int]$X = 1600,
  [int]$Y = 60,
  [string]$Prefix = "E:\work\rgui\tmp\rp"
)

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class RP {
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
$cb = [RP+EnumProc]{
  param($h, $p)
  $q = [uint32]0
  [RP]::GetWindowThreadProcessId($h, [ref]$q) | Out-Null
  if (($pids -contains $q) -and [RP]::IsWindowVisible($h)) {
    $sb = New-Object System.Text.StringBuilder 256
    [RP]::GetWindowTextW($h, $sb, 256) | Out-Null
    if ($sb.Length -gt 0) {
      $r = New-Object RP+RECT
      [RP]::GetWindowRect($h, [ref]$r) | Out-Null
      $a = ($r.R - $r.L) * ($r.B - $r.T)
      if ($a -gt $script:bestArea) { $script:best = $h; $script:bestArea = $a }
    }
  }
  return $true
}
[RP]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null

$r = New-Object RP+RECT; [RP]::GetWindowRect($best, [ref]$r) | Out-Null
$W = $r.R - $r.L; $H = $r.B - $r.T
$saveL = $r.L; $saveT = $r.T
Write-Output "window $W x $H, was at $saveL,$saveT"

[RP]::SetWindowPos($best, [IntPtr](-1), 0, 0, 0, 0, 0x0001 -bor 0x0010) | Out-Null
[RP]::MoveWindow($best, $X, $Y, $W, $H, $true) | Out-Null
Start-Sleep -Milliseconds 900

$bmpA = New-Object System.Drawing.Bitmap $W, $H
$ga = [System.Drawing.Graphics]::FromImage($bmpA)
$ga.CopyFromScreen($X, $Y, 0, 0, (New-Object System.Drawing.Size $W, $H))
$ga.Dispose()
$bmpA.Save("$Prefix-before.png", [System.Drawing.Imaging.ImageFormat]::Png)

# who is on top in the top band?
function TopAt([int]$px, [int]$py) {
  $p = New-Object RP+POINT; $p.X = $px; $p.Y = $py
  $h = [RP]::WindowFromPoint($p)
  $t = [RP]::GetAncestor($h, 2)
  $q = [uint32]0; [RP]::GetWindowThreadProcessId($t, [ref]$q) | Out-Null
  $name = (Get-Process -Id $q -ErrorAction SilentlyContinue).ProcessName
  return $name
}
Write-Output ("top window at (X+120,Y+40) = " + (TopAt ($X + 120) ($Y + 40)))
Write-Output ("top window at (X+120,Y+400) = " + (TopAt ($X + 120) ($Y + 400)))

# force a fresh full repaint: one pixel taller, then back
[RP]::MoveWindow($best, $X, $Y, $W, ($H + 1), $true) | Out-Null
Start-Sleep -Milliseconds 600
[RP]::MoveWindow($best, $X, $Y, $W, $H, $true) | Out-Null
Start-Sleep -Milliseconds 900

$bmpB = New-Object System.Drawing.Bitmap $W, $H
$gb = [System.Drawing.Graphics]::FromImage($bmpB)
$gb.CopyFromScreen($X, $Y, 0, 0, (New-Object System.Drawing.Size $W, $H))
$gb.Dispose()
$bmpB.Save("$Prefix-after.png", [System.Drawing.Imaging.ImageFormat]::Png)

$diffRows = @{}
$cx = [int]($W / 2)
for ($y = 0; $y -lt $H; $y += 4) {
  $a = $bmpA.GetPixel($cx, $y); $b = $bmpB.GetPixel($cx, $y)
  $d = [Math]::Abs($a.R - $b.R) + [Math]::Abs($a.G - $b.G) + [Math]::Abs($a.B - $b.B)
  if ($d -gt 12) { $diffRows[$y] = ("#{0:x2}{1:x2}{2:x2} -> #{3:x2}{4:x2}{5:x2}" -f $a.R,$a.G,$a.B,$b.R,$b.G,$b.B) }
}
Write-Output "rows differing along the centre column (step 4): $($diffRows.Count) of $([int]($H/4))"
$keys = $diffRows.Keys | Sort-Object
if ($keys.Count -gt 0) {
  Write-Output ("  first: y=$($keys[0])  $($diffRows[$keys[0]])")
  Write-Output ("  last : y=$($keys[-1]) $($diffRows[$keys[-1]])")
}
$bmpA.Dispose(); $bmpB.Dispose()

[RP]::MoveWindow($best, $saveL, $saveT, $W, $H, $true) | Out-Null
[RP]::SetWindowPos($best, [IntPtr](-2), 0, 0, 0, 0, 0x0001 -bor 0x0010) | Out-Null
Write-Output "restored to $saveL,$saveT; images $Prefix-before.png / $Prefix-after.png"
