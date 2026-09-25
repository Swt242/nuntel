# 截取主窗口,存成 PNG(README 里那三张就是这么来的)。
#
# 为什么不用现成的截图工具:窗口是无边框的,形状由 DWM 裁出来,随手框选很容易
# 带上周围桌面的一圈边;按窗口矩形精确截就没有这个问题。
#
# 用法(在仓库根目录):
#   powershell -ExecutionPolicy Bypass -File tools/screenshot.ps1 screenshot-light.png
#   powershell -ExecutionPolicy Bypass -File tools/screenshot.ps1 -Title "..." settings.png
#
# 不传 -Title 就截主窗口(按窗口面积挑最大的那个,比按进程名拿 MainWindowHandle 靠谱:
# 本进程有两个顶层窗口,MainWindowHandle 给谁全看系统心情)。
#
# ⚠️ -Title 里的中文**别从命令行传**:PowerShell 5.1 经 argv 收到中文会变成乱码,
# 结果就是「明明窗口在那却找不到」。要么干脆不传,要么从 UTF-8 文件读进来再传
# (见 README 里生成设置窗口截图那条命令)。
#
# 注意:必须是窗口在前台时截。另外这脚本是按 100% 缩放写的,高 DPI 环境下
# 下面按 GetWindowRect 拿到的物理像素和逻辑像素会对不上,需要自己乘缩放比。

param(
    [Parameter(Mandatory = $true, Position = 0)][string]$Out,
    [string]$Title = ""
)

Add-Type -AssemblyName System.Drawing

Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public class Win {
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left, Top, Right, Bottom; }
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT r);
}
"@

$appPid = (Get-Process -Name "nuntel" -ErrorAction SilentlyContinue | Select-Object -First 1).Id
if (-not $appPid) {
    Write-Error "no 'nuntel' process - start the app first"
    exit 1
}

# 按进程名收集窗口,而不是按标题找 —— 标题是中文,PowerShell 5.1 读没有 BOM 的 .ps1
# 时会当成 ANSI,中文字面量直接变乱码,FindWindow 自然找不到。
# 用 ArrayList 而不是 `$found = @()` + `$found += ...`:
# 回调是给 .NET 调的委托,跑在**另一个作用域**里,`+=` 赋的是那个作用域的副本,
# 外面这个 $found 一直是空的(实测报 "visible: 0")。`.Add()` 是改对象本身,不受作用域影响。
$found = New-Object System.Collections.ArrayList
$sb = New-Object System.Text.StringBuilder 512
$cb = [Win+EnumProc] {
    param($h, $l)
    $owner = 0
    [void][Win]::GetWindowThreadProcessId($h, [ref]$owner)
    if ($owner -eq $appPid -and [Win]::IsWindowVisible($h)) {
        [void]$sb.Clear(); [void][Win]::GetWindowTextW($h, $sb, 512)
        $r = New-Object Win+RECT
        [void][Win]::GetWindowRect($h, [ref]$r)
        [void]$found.Add([pscustomobject]@{
            Handle = $h
            Title  = $sb.ToString()
            Rect   = $r
            Area   = ($r.Right - $r.Left) * ($r.Bottom - $r.Top)
        })
    }
    return $true
}
[void][Win]::EnumWindows($cb, [IntPtr]::Zero)

# 有 -Title 就按标题精确匹配,否则挑最大的那个 —— 本进程有个 16x16 的托盘消息窗口,
# 按面积挑正好把它排除掉
$target = if ($Title) { $found | Where-Object { $_.Title -eq $Title } | Select-Object -First 1 }
          else { $found | Sort-Object Area -Descending | Select-Object -First 1 }
if (-not $target) {
    Write-Error "no matching window (visible: $($found.Count))"
    exit 1
}
$hwnd = $target.Handle
$r = $target.Rect
$w = $r.Right - $r.Left
$h = $r.Bottom - $r.Top

$bmp = New-Object System.Drawing.Bitmap($w, $h)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($r.Left, $r.Top, 0, 0, $bmp.Size)
$bmp.Save([System.IO.Path]::GetFullPath($Out), [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose()
$bmp.Dispose()

Write-Host "已保存 $Out  ($w x $h @ $($r.Left),$($r.Top))"
