# Exact geometry for every top-level window of a process: window rect, client rect,
# client origin in screen coords, and the DWM "extended frame bounds" (what the
# compositor actually treats as the visible frame -- differs from GetWindowRect when
# the window has an invisible resize border).
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools/wingeom.ps1 -Process nuntel
param([string]$Process = "nuntel")

Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class WG {
  public delegate bool EnumProc(IntPtr h, IntPtr p);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr p);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
  [DllImport("user32.dll")] public static extern long GetWindowLongPtrW(IntPtr h, int i);
  [DllImport("dwmapi.dll")] public static extern int DwmGetWindowAttribute(IntPtr h, int attr, out RECT r, int size);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
}
"@

$procs = Get-Process -Name $Process -ErrorAction SilentlyContinue
if (-not $procs) { Write-Output "no process named $Process"; exit 1 }
$pids = @($procs | ForEach-Object { [uint32]$_.Id })

$rows = @()
$cb = [WG+EnumProc]{
  param($h, $p)
  $pid2 = [uint32]0
  [WG]::GetWindowThreadProcessId($h, [ref]$pid2) | Out-Null
  if ($pids -contains $pid2) {
    $sb = New-Object System.Text.StringBuilder 512
    [WG]::GetWindowTextW($h, $sb, 512) | Out-Null
    if ($sb.Length -eq 0) { return $true }
    $wr = New-Object WG+RECT; [WG]::GetWindowRect($h, [ref]$wr) | Out-Null
    $cr = New-Object WG+RECT; [WG]::GetClientRect($h, [ref]$cr) | Out-Null
    $pt = New-Object WG+POINT; [WG]::ClientToScreen($h, [ref]$pt) | Out-Null
    $ef = New-Object WG+RECT
    $hr = [WG]::DwmGetWindowAttribute($h, 9, [ref]$ef, 16)   # DWMWA_EXTENDED_FRAME_BOUNDS
    $style = [WG]::GetWindowLongPtrW($h, -16)
    $ex = [WG]::GetWindowLongPtrW($h, -20)
    $script:rows += [pscustomobject]@{
      Visible  = [WG]::IsWindowVisible($h)
      Title    = $sb.ToString()
      WinRect  = "$($wr.L),$($wr.T) $($wr.R - $wr.L)x$($wr.B - $wr.T)"
      CliAt    = "$($pt.X),$($pt.Y)"
      Client   = "$($cr.R - $cr.L)x$($cr.B - $cr.T)"
      DwmFrame = if ($hr -eq 0) { "$($ef.L),$($ef.T) $($ef.R - $ef.L)x$($ef.B - $ef.T)" } else { "err=$hr" }
      Style    = ("0x{0:x8}" -f $style)
      ExStyle  = ("0x{0:x8}" -f $ex)
    }
  }
  return $true
}
[WG]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
$rows | Format-List | Out-String -Width 200
