# Park the app's biggest window at a known spot, TOPMOST it, then click points given in
# window-relative coordinates with a real (timed) mouse press, reporting after each click
# whether the settings window (520x344) became visible.
#
# Everything happens inside one process on purpose: shell commands between the move and
# the click steal focus / change the z-order, and the click lands somewhere else.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools/uiclick.ps1 -ClickXs 296,308,320 -ClickY 18
param(
  [string]$Process = "rgui-todo",
  [int]$ParkX = 1600,
  [int]$ParkY = 60,
  [string]$ClickXs = "296,308,320",
  [int]$ClickY = 18,
  # 只挑这个尺寸的窗口(0 = 不限):用来点设置窗口(520x344)而不是主窗口
  [int]$MatchWidth = 0,
  [int]$MatchHeight = 0
)

Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class UC {
  public delegate bool EnumProc(IntPtr h, IntPtr p);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr p);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool MoveWindow(IntPtr h, int x, int y, int w, int ht, bool repaint);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int w, int ht, uint flags);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, IntPtr e);
  [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(POINT p);
  [DllImport("user32.dll")] public static extern IntPtr GetAncestor(IntPtr h, uint f);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
}
"@

$procs = Get-Process -Name $Process -ErrorAction SilentlyContinue
if (-not $procs) { Write-Output "no process named $Process"; exit 1 }
$pids = @($procs | ForEach-Object { [uint32]$_.Id })

# 找主窗口(可见里最大的那个),顺便记下所有窗口,后面用面积判断状态
$script:best = [IntPtr]::Zero; $script:bestArea = 0; $script:br = $null
$script:MW = $MatchWidth; $script:MH = $MatchHeight
$script:settingsVisible = $false
$cb = [UC+EnumProc]{
  param($h, $p)
  $q = [uint32]0
  [UC]::GetWindowThreadProcessId($h, [ref]$q) | Out-Null
  if ($pids -contains $q) {
    $sb = New-Object System.Text.StringBuilder 256
    [UC]::GetWindowTextW($h, $sb, 256) | Out-Null
    if ($sb.Length -gt 0) {
      $r = New-Object UC+RECT
      [UC]::GetWindowRect($h, [ref]$r) | Out-Null
      $w = $r.R - $r.L; $ht = $r.B - $r.T
      # 设置窗口:宽度固定 520(高度见 ui/settings.slint)
      if ($w -eq 520 -and $ht -gt 300 -and [UC]::IsWindowVisible($h)) { $script:settingsVisible = $true }
      $sizeOk = ($script:MW -eq 0 -or $w -eq $script:MW) -and ($script:MH -eq 0 -or $ht -eq $script:MH)
      if ($sizeOk -and [UC]::IsWindowVisible($h) -and ($w * $ht) -gt $script:bestArea) {
        $script:best = $h; $script:bestArea = ($w * $ht); $script:br = $r
      }
    }
  }
  return $true
}

function Refresh-State {
  $script:settingsVisible = $false
  [UC]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
}

Refresh-State
$best = $script:best
$r = $script:br
$W = $r.R - $r.L; $H = $r.B - $r.T
Write-Output "main window $W x $H at $($r.L),$($r.T); settings visible = $($script:settingsVisible)"

[UC]::SetWindowPos($best, [IntPtr](-1), 0, 0, 0, 0, 0x0001 -bor 0x0010) | Out-Null
[UC]::MoveWindow($best, $ParkX, $ParkY, $W, $H, $true) | Out-Null
Start-Sleep -Milliseconds 700

foreach ($xs in ($ClickXs -split ',')) {
  $x = $ParkX + [int]$xs; $y = $ParkY + $ClickY
  $p = New-Object UC+POINT; $p.X = $x; $p.Y = $y
  $under = [UC]::GetAncestor([UC]::WindowFromPoint($p), 2)
  $q2 = [uint32]0
  [UC]::GetWindowThreadProcessId($under, [ref]$q2) | Out-Null
  $owner = (Get-Process -Id $q2 -ErrorAction SilentlyContinue).ProcessName

  [UC]::SetCursorPos($x, $y) | Out-Null
  Start-Sleep -Milliseconds 250
  [UC]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)
  Start-Sleep -Milliseconds 90
  [UC]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)
  Start-Sleep -Milliseconds 800

  Refresh-State
  Write-Output ("  click window+({0},{1}) -> screen({2},{3}); under=$owner; settings visible = {4}" -f $xs, $ClickY, $x, $y, $script:settingsVisible)
  if ($script:settingsVisible) { break }
}
