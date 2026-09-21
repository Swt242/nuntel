# 双击桌宠打开 AI 对话窗。
#
# 为什么按尺寸找窗口而不是写死坐标:桌宠能被拖动,位置一变写死的坐标就点空了
# (或者更糟 —— 点成拖动,把宠物挪走)。进程里每个窗口宽度都不同,按宽度认最稳:
# 主窗口 476 / 设置 520 / 桌宠 240 / 对话 400 / 笔记 820
#
#   powershell -ExecutionPolicy Bypass -File tools/open-chat.ps1

Add-Type @"
using System;
using System.Runtime.InteropServices;
public class OCP {
  public delegate bool EnumProc(IntPtr h, IntPtr p);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr p);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, IntPtr e);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
}
"@

$script:rect = $null
function Find-AppWindow([int]$width) {
    $script:hit = [IntPtr]::Zero
    $script:w = $width
    $cb = [OCP+EnumProc]{
        param($h, $p)
        $q = [uint32]0
        [OCP]::GetWindowThreadProcessId($h, [ref]$q) | Out-Null
        $name = (Get-Process -Id $q -ErrorAction SilentlyContinue).ProcessName
        if ($name -eq "rgui-todo" -and [OCP]::IsWindowVisible($h)) {
            $r = New-Object OCP+RECT
            [OCP]::GetWindowRect($h, [ref]$r) | Out-Null
            if (($r.R - $r.L) -eq $script:w) { $script:hit = $h; $script:rect = $r; return $false }
        }
        return $true
    }
    [OCP]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
    return $script:hit
}

$pet = Find-AppWindow 240
if ($pet -eq [IntPtr]::Zero) { Write-Output "找不到桌宠窗口"; exit 1 }
$r = $script:rect
# 点宠物本体(不是头顶的角标/输入条):底边往上 56px 就是身体中心
$px = $r.L + 120
$py = $r.B - 56

# 两次点击必须**挨得够近**:Slint 判定双击有窗口期,隔太久就只当成两次单击
# (第一次还会把「快速添加」输入条开一下)。实测 300ms 就太远了,这里 ~100ms。
[OCP]::SetCursorPos($px, $py) | Out-Null
Start-Sleep -Milliseconds 200
for ($i = 0; $i -lt 2; $i++) {
    [OCP]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 35
    [OCP]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 60
}
Start-Sleep -Milliseconds 900

$chat = Find-AppWindow 400
if ($chat -eq [IntPtr]::Zero) { Write-Output "对话窗没出来"; exit 1 }
Write-Output ("对话窗在 " + $script:rect.L + "," + $script:rect.T)
