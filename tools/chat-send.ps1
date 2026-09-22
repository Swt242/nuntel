# 往 AI 对话窗里发一条消息:双击桌宠(顺便让应用自己把对话窗提到前台)→ 点输入框
# → 输入文本 → 回车。
#
# **为什么必须一个进程里做完**,两个原因:
#
# 1. 对话窗是不置顶的(只有桌宠置顶),中间插一条 bash 命令,终端/编辑器就会把它
#    盖住,后面的点击和输入全打到别的窗口上 —— 表现是「点了没反应 / 字跑到别处去了」,
#    特别容易误判成控件坏了(这个坑在桌宠那边也踩过,见 docs/calendar-reminders.md §23.4)。
# 2. **`SetForegroundWindow` 从后台进程调用会被系统直接忽略**(前台锁定),所以不能
#    「自己把窗口提到前面」。改成双击桌宠,让**应用自己**去调 —— 它刚收到点击,
#    系统允许它设置前台窗口。这一步绕不开:试过直接 SetForegroundWindow,没用。
#
# 中文用 keybd_event + KEYEVENTF_UNICODE 送 UTF-16 码元:走虚拟键码那条路只能打 ASCII。
#
#   powershell -ExecutionPolicy Bypass -File tools/chat-send.ps1 -Text "你好"

param(
    [Parameter(Mandatory = $true)][string]$Text,
    # 输入框相对对话窗的位置(对话窗 400x520,输入框在最底下那条)
    [int]$InputDx = 200,
    [int]$InputDy = 494
)

Add-Type @"
using System;
using System.Runtime.InteropServices;
public class CS {
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, IntPtr e);
  [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
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
$script:rect = $null

# 按「进程 + 宽度」找窗口。**不按标题找** —— 标题里有中文,命令行传参会乱码,
# 脚本里的字面量还得跟 .slint 里那个中间点字符逐字节一致,太脆。
# 进程里没有第二个这么宽的窗口:主窗口 476、设置 520、桌宠 240、对话 400。
function Find-AppWindow([int]$width) {
    $script:hit = [IntPtr]::Zero
    $script:w = $width
    $cb = [CS+EnumProc]{
        param($h, $p)
        $q = [uint32]0
        [CS]::GetWindowThreadProcessId($h, [ref]$q) | Out-Null
        $name = (Get-Process -Id $q -ErrorAction SilentlyContinue).ProcessName
        if ($name -eq "nuntel" -and [CS]::IsWindowVisible($h)) {
            $r = New-Object CS+RECT
            [CS]::GetWindowRect($h, [ref]$r) | Out-Null
            if (($r.R - $r.L) -eq $script:w) {
                $script:hit = $h
                $script:rect = $r
                return $false
            }
        }
        return $true
    }
    [CS]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
    return $script:hit
}

function Click([int]$x, [int]$y) {
    [CS]::SetCursorPos($x, $y) | Out-Null
    Start-Sleep -Milliseconds 180
    [CS]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 50
    [CS]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)
}

# 1. 找到对话窗(得先双击桌宠把它显示出来)
$chat = Find-AppWindow 400
if ($chat -eq [IntPtr]::Zero) { Write-Output "对话窗还没打开 —— 先双击桌宠"; exit 1 }

# 2. **临时置顶**再点。
#
# 对话窗按设计是不置顶的,所以它随时可能被编辑器/终端盖住 —— 那时我们的点击会
# 落到盖住它的那个窗口上,输入更是不知道跑哪去了(实测:字全打进了 WebStorm)。
# 置顶不属于「被系统限制」的操作(受限的是 SetForegroundWindow),所以这一步管用;
# 点一下之后窗口自己也拿到焦点了,紧接着打字就行。完事再取消置顶。
$TOPMOST = [IntPtr](-1); $NOTOPMOST = [IntPtr](-2)
$SWP_NOSIZE = 0x0001; $SWP_NOMOVE = 0x0002; $SWP_NOACTIVATE = 0x0010
[CS]::SetWindowPos($chat, $TOPMOST, 0, 0, 0, 0, $SWP_NOSIZE -bor $SWP_NOMOVE -bor $SWP_NOACTIVATE) | Out-Null
Start-Sleep -Milliseconds 250

$r = $script:rect
Click ($r.L + $InputDx) ($r.T + $InputDy)
Start-Sleep -Milliseconds 300

# 3. 输入文本(UTF-16 码元直送)
$KEYEVENTF_UNICODE = 0x0004
$KEYEVENTF_KEYUP = 0x0002
foreach ($ch in $Text.ToCharArray()) {
    $code = [int][char]$ch
    [CS]::keybd_event(0, [byte]($code -band 0xFF), $KEYEVENTF_UNICODE, [UIntPtr]::Zero)
    [CS]::keybd_event(0, [byte](($code -shr 8) -band 0xFF), $KEYEVENTF_UNICODE, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 10
    [CS]::keybd_event(0, [byte]($code -band 0xFF), $KEYEVENTF_UNICODE -bor $KEYEVENTF_KEYUP, [UIntPtr]::Zero)
    [CS]::keybd_event(0, [byte](($code -shr 8) -band 0xFF), $KEYEVENTF_UNICODE -bor $KEYEVENTF_KEYUP, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 10
}
Start-Sleep -Milliseconds 200

# 4. 回车发送
[CS]::keybd_event(0x0D, 0, 0, [UIntPtr]::Zero)
Start-Sleep -Milliseconds 40
[CS]::keybd_event(0x0D, 0, $KEYEVENTF_KEYUP, [UIntPtr]::Zero)

Start-Sleep -Milliseconds 200
[CS]::SetWindowPos($chat, $NOTOPMOST, 0, 0, 0, 0, $SWP_NOSIZE -bor $SWP_NOMOVE -bor $SWP_NOACTIVATE) | Out-Null

Write-Output "已发送: $Text"
