# Move the main window to a target rect and read the rect back over time, to see
# whether anything (the app itself, or Windows) changes it afterwards.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools/movetest.ps1 -X 1600 -Y 60
param(
  [string]$Process = "rgui-todo",
  [int]$X = 1600,
  [int]$Y = 60,
  [int]$W = 476,
  [int]$H = 826
)

Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class MV {
  public delegate bool EnumProc(IntPtr h, IntPtr p);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr p);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool MoveWindow(IntPtr h, int x, int y, int w, int ht, bool repaint);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
}
"@

$procs = Get-Process -Name $Process -ErrorAction SilentlyContinue
if (-not $procs) { Write-Output "no process named $Process"; exit 1 }
$pids = @($procs | ForEach-Object { [uint32]$_.Id })

$best = [IntPtr]::Zero; $bestArea = 0
$cb = [MV+EnumProc]{
  param($h, $p)
  $q = [uint32]0
  [MV]::GetWindowThreadProcessId($h, [ref]$q) | Out-Null
  if (($pids -contains $q) -and [MV]::IsWindowVisible($h)) {
    $sb = New-Object System.Text.StringBuilder 256
    [MV]::GetWindowTextW($h, $sb, 256) | Out-Null
    if ($sb.Length -gt 0) {
      $r = New-Object MV+RECT
      [MV]::GetWindowRect($h, [ref]$r) | Out-Null
      $a = ($r.R - $r.L) * ($r.B - $r.T)
      if ($a -gt $script:bestArea) { $script:best = $h; $script:bestArea = $a }
    }
  }
  return $true
}
[MV]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null

function ShowRect([string]$when) {
  $r = New-Object MV+RECT
  [MV]::GetWindowRect($best, [ref]$r) | Out-Null
  Write-Output ("  {0,-12} = {1},{2} {3}x{4}" -f $when, $r.L, $r.T, ($r.R - $r.L), ($r.B - $r.T))
}

ShowRect "before move"
[void][MV]::MoveWindow($best, $X, $Y, $W, $H, $true)
ShowRect "right after"
Start-Sleep -Milliseconds 150; ShowRect "+150ms"
Start-Sleep -Milliseconds 350; ShowRect "+500ms"
Start-Sleep -Milliseconds 600; ShowRect "+1.1s"
Start-Sleep -Milliseconds 1500; ShowRect "+2.6s"
