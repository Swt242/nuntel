# List every top-level window owned by a process, with its client rect.
# Source is pure ASCII on purpose: Windows PowerShell reads BOM-less .ps1 as ANSI,
# so any non-ASCII literal here would be mangled. Titles are read at runtime.
#
#   powershell -ExecutionPolicy Bypass -File tools/winlist.ps1 -Process nuntel
param([string]$Process = "nuntel")

Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class WL {
  public delegate bool EnumProc(IntPtr h, IntPtr p);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr p);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
}
"@

$procs = Get-Process -Name $Process -ErrorAction SilentlyContinue
if (-not $procs) { Write-Output "no process named $Process"; exit 1 }
$pids = @($procs | ForEach-Object { [uint32]$_.Id })

$found = @()
$cb = [WL+EnumProc]{
  param($h, $p)
  $pid2 = [uint32]0
  [WL]::GetWindowThreadProcessId($h, [ref]$pid2) | Out-Null
  if ($pids -contains $pid2) {
    $sb = New-Object System.Text.StringBuilder 512
    [WL]::GetWindowTextW($h, $sb, 512) | Out-Null
    $r = New-Object WL+RECT
    [WL]::GetWindowRect($h, [ref]$r) | Out-Null
    $script:found += [pscustomobject]@{
      Hwnd    = ("0x{0:x}" -f [int64]$h)
      Visible = [WL]::IsWindowVisible($h)
      X = $r.L; Y = $r.T; W = ($r.R - $r.L); H = ($r.B - $r.T)
      Title   = $sb.ToString()
    }
  }
  return $true
}
[WL]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
$found | Where-Object { $_.Title -ne "" } | Format-Table -AutoSize | Out-String -Width 200
