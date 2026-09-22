# Capture a window's OWN surface with PrintWindow(PW_RENDERFULLCONTENT) -- that is
# what the window has actually drawn, independent of z-order and of what is behind it.
# Also reports the colour of a few probe points and of any fully transparent pixel
# (transparent shows up as alpha 0 / black in the DIB).
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools/printwin.ps1 -Process nuntel
param(
  [string]$Process = "nuntel",
  [string]$Out = "E:\work\rgui\tmp\printwin.png",
  # 只挑这个尺寸的窗口(0 = 不限)。用来截主窗口之外的窗口,比如设置窗口 520x344。
  [int]$MatchWidth = 0,
  [int]$MatchHeight = 0
)

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class PW {
  public delegate bool EnumProc(IntPtr h, IntPtr p);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr p);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
}
"@

$procs = Get-Process -Name $Process -ErrorAction SilentlyContinue
if (-not $procs) { Write-Output "no process named $Process"; exit 1 }
$pids = @($procs | ForEach-Object { [uint32]$_.Id })

$best = [IntPtr]::Zero; $bestArea = 0; $script:br = $null
$script:MW = $MatchWidth; $script:MH = $MatchHeight
$cb = [PW+EnumProc]{
  param($h, $p)
  $q = [uint32]0
  [PW]::GetWindowThreadProcessId($h, [ref]$q) | Out-Null
  if (($pids -contains $q) -and [PW]::IsWindowVisible($h)) {
    $sb = New-Object System.Text.StringBuilder 256
    [PW]::GetWindowTextW($h, $sb, 256) | Out-Null
    if ($sb.Length -gt 0) {
      $r = New-Object PW+RECT
      [PW]::GetWindowRect($h, [ref]$r) | Out-Null
      $a = ($r.R - $r.L) * ($r.B - $r.T)
      $sizeOk = ($script:MW -eq 0 -or ($r.R - $r.L) -eq $script:MW) -and ($script:MH -eq 0 -or ($r.B - $r.T) -eq $script:MH)
      if ($sizeOk -and $a -gt $script:bestArea) { $script:best = $h; $script:bestArea = $a; $script:br = $r }
    }
  }
  return $true
}
[PW]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
$r = $script:br
Write-Output "DEBUG-MW=$script:MW DEBUG-MH=$script:MH DEBUG-best=$script:bestArea"
$W = $r.R - $r.L; $H = $r.B - $r.T
Write-Output ("window at {0},{1} size {2}x{3}" -f $r.L, $r.T, $W, $H)

$bmp = New-Object System.Drawing.Bitmap $W, $H, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.Clear([System.Drawing.Color]::Magenta)          # so unprintable areas are obvious
$hdc = $g.GetHdc()
$ok = [PW]::PrintWindow($best, $hdc, 2)            # PW_RENDERFULLCONTENT
$g.ReleaseHdc($hdc)
$g.Dispose()
Write-Output "PrintWindow returned $ok"

$dir = Split-Path -Parent $Out
if ($dir -and -not (Test-Path $dir)) { New-Item -ItemType Directory -Force -Path $Out | Out-Null }
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)

$cx = [int]($W / 2)
foreach ($y in @(0, 40, 90, 130, 140, 200, 400, 700, 760, 770, ($H - 1))) {
  $c = $bmp.GetPixel($cx, $y)
  Write-Output ("  y={0,4} (rel {1,4}) = #{2:x2}{3:x2}{4:x2} a={5}" -f $y, $y, $c.R, $c.G, $c.B, $c.A)
}
$bmp.Dispose()
Write-Output "saved $Out"
