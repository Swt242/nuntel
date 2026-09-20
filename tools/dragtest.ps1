# Timed left-button drag with the real cursor (press, glide, release).
# A synthetic drag that goes down-move-up in one go is too fast: Windows' modal
# window-move loop only starts on WM_NCLBUTTONDOWN and stops as soon as the button
# is up, so an instant drag moves nothing even when the code is fine.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools/dragtest.ps1 -X1 2416 -Y1 1312 -X2 2000 -Y2 1150
param(
  [int]$X1 = 2416, [int]$Y1 = 1312,
  [int]$X2 = 2000, [int]$Y2 = 1150,
  [int]$Steps = 25, [int]$StepMs = 20
)

Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class MS {
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, IntPtr e);
  [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(POINT p);
  [DllImport("user32.dll")] public static extern IntPtr GetAncestor(IntPtr h, uint f);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
}
"@

$LEFTDOWN = 0x0002; $LEFTUP = 0x0004

function Report([string]$when) {
  $p = New-Object MS+POINT; $p.X = $X1; $p.Y = $Y1
  $h = [MS]::WindowFromPoint($p)
  $t = [MS]::GetAncestor($h, 2)
  $q = [uint32]0; [MS]::GetWindowThreadProcessId($t, [ref]$q) | Out-Null
  $sb = New-Object System.Text.StringBuilder 256
  [MS]::GetWindowTextW($t, $sb, 256) | Out-Null
  $name = (Get-Process -Id $q -ErrorAction SilentlyContinue).ProcessName
  Write-Output "$when : window under ($X1,$Y1) = $name  title='$($sb.ToString())'"
}

Report "before"
[MS]::SetCursorPos($X1, $Y1) | Out-Null
Start-Sleep -Milliseconds 200
[MS]::mouse_event($LEFTDOWN, 0, 0, 0, [IntPtr]::Zero)
Start-Sleep -Milliseconds 120
for ($i = 1; $i -le $Steps; $i++) {
  $x = [int]($X1 + ($X2 - $X1) * $i / $Steps)
  $y = [int]($Y1 + ($Y2 - $Y1) * $i / $Steps)
  [MS]::SetCursorPos($x, $y) | Out-Null
  Start-Sleep -Milliseconds $StepMs
}
Start-Sleep -Milliseconds 150
[MS]::mouse_event($LEFTUP, 0, 0, 0, [IntPtr]::Zero)
Start-Sleep -Milliseconds 300
Report "after"

$procs = Get-Process -Name rgui-todo -ErrorAction SilentlyContinue
foreach ($p in $procs) {
  $p.Refresh()
  Write-Output ("  rgui-todo pid {0} mainwindowhandle {1}" -f $p.Id, $p.MainWindowTitle)
}
