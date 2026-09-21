# 把应用某个窗口临时设为置顶 / 取消置顶。
#
# 用途:验证界面时,**对话窗按设计是不置顶的**,被编辑器或终端盖住之后,
# 后续的点击和输入就全落到盖住它的那个窗口上 —— 表现是「点了没反应 / 字跑到别处」。
#
# 为什么用置顶而不是 `SetForegroundWindow`:**后者从后台进程调用会被系统直接忽略**
# (前台锁定),试过,没用。置顶不受这个限制,而且点一下窗口自己就拿到焦点了。
#
# 按宽度认窗口,不按标题(标题含中文,命令行传参会乱码):
# 主窗口 476 / 设置 520 / 桌宠 240 / 对话 400
#
#   powershell -ExecutionPolicy Bypass -File tools/topmost.ps1 -Width 400 -On $true
#   powershell -ExecutionPolicy Bypass -File tools/topmost.ps1 -Width 400 -On $false

param(
    [int]$Width = 400,
    # "on" / "off" 用字符串收,不用 [bool] —— `-File` 传参是按字符串给的,
    # `-On 1` 会变成字符串 "1",[bool] 转换直接报错(踩过)
    [string]$Mode = "on",
    [string]$Process = "rgui-todo"
)

Add-Type @"
using System;
using System.Runtime.InteropServices;
public class TP {
  public delegate bool EnumProc(IntPtr h, IntPtr p);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr p);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int w, int ht, uint flags);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
}
"@

$script:hit = [IntPtr]::Zero
$script:w = $Width
# 委托跑在别的作用域里,看不到 param 变量,得先搬到 script: 上(这个坑栽过好几次)
$script:proc = $Process
$cb = [TP+EnumProc]{
    param($h, $p)
    $q = [uint32]0
    [TP]::GetWindowThreadProcessId($h, [ref]$q) | Out-Null
    $name = (Get-Process -Id $q -ErrorAction SilentlyContinue).ProcessName
    if ($name -eq $script:proc -and [TP]::IsWindowVisible($h)) {
        $r = New-Object TP+RECT
        [TP]::GetWindowRect($h, [ref]$r) | Out-Null
        if (($r.R - $r.L) -eq $script:w) { $script:hit = $h; return $false }
    }
    return $true
}
[TP]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null

if ($script:hit -eq [IntPtr]::Zero) { Write-Output "没找到宽度 $Width 的窗口"; exit 1 }

$on = ($Mode -ne "off")
$after = if ($on) { [IntPtr](-1) } else { [IntPtr](-2) }
$SWP_NOSIZE = 0x0001; $SWP_NOMOVE = 0x0002; $SWP_NOACTIVATE = 0x0010
[TP]::SetWindowPos($script:hit, $after, 0, 0, 0, 0, $SWP_NOSIZE -bor $SWP_NOMOVE -bor $SWP_NOACTIVATE) | Out-Null
Write-Output ("宽度 $Width 的窗口已" + $(if ($on) { "置顶" } else { "取消置顶" }))
