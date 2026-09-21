#!/usr/bin/env python3
"""点桌宠右上角那个红点(未完成数角标),验证主窗口会被打开。

红点的位置是布局算出来的,拿不到现成的坐标 —— 但它的颜色是固定的语义色
(Theme.danger,#d94742 或 #ff8078),所以直接**从屏幕上找那块红**:
截下桌宠窗口,在右上角搜这个颜色的连通区域,取重心点下去。

    python tools/badge-click.py            # 只报告,不点
    python tools/badge-click.py --click    # 真的点

一起报出来的还有主窗口的可见状态,调用方据此判断点完有没有效果。
"""
import ctypes
import sys
from ctypes import wintypes

u32 = ctypes.windll.user32
g32 = ctypes.windll.gdi32

PET_TITLE = "待办清单 · 桌宠"
MAIN_TITLE = "待办清单"


class RECT(ctypes.Structure):
    _fields_ = [("left", ctypes.c_long), ("top", ctypes.c_long),
                ("right", ctypes.c_long), ("bottom", ctypes.c_long)]


class BITMAPINFOHEADER(ctypes.Structure):
    _fields_ = [("biSize", wintypes.DWORD), ("biWidth", ctypes.c_long),
                ("biHeight", ctypes.c_long), ("biPlanes", wintypes.WORD),
                ("biBitCount", wintypes.WORD), ("biCompression", wintypes.DWORD),
                ("biSizeImage", wintypes.DWORD), ("biXPelsPerMeter", ctypes.c_long),
                ("biYPelsPerMeter", ctypes.c_long), ("biClrUsed", wintypes.DWORD),
                ("biClrImportant", wintypes.DWORD)]


class BITMAPINFO(ctypes.Structure):
    _fields_ = [("bmiHeader", BITMAPINFOHEADER), ("bmiColors", wintypes.DWORD * 3)]


# 拿的是物理像素坐标 —— 本进程必须先声明自己 DPI 感知,否则 Windows 会把坐标
# 按 96dpi 缩放后交给我们,点下去就偏了。
try:
    ctypes.windll.shcore.SetProcessDpiAwareness(2)  # PROCESS_PER_MONITOR_DPI_AWARE
except OSError:
    u32.SetProcessDPIAware()

# **必须声明 argtypes**。不声明的话 ctypes 把 Python 的 int 按 32 位传,
# `HWND_TOPMOST`(-1)会变成 0x00000000ffffffff 这个无效句柄,SetWindowPos 直接
# 返回 0 失败 —— 而它不抛异常,很容易当成「置顶了但没生效」去别处找原因(实测)。
u32.SetWindowPos.argtypes = [wintypes.HWND, wintypes.HWND, ctypes.c_int, ctypes.c_int,
                             ctypes.c_int, ctypes.c_int, wintypes.UINT]
u32.SetWindowPos.restype = wintypes.BOOL
u32.ShowWindow.argtypes = [wintypes.HWND, ctypes.c_int]
u32.ShowWindow.restype = wintypes.BOOL

HWND_TOPMOST = wintypes.HWND(-1)
HWND_NOTOPMOST = wintypes.HWND(-2)
SWP_NOSIZE, SWP_NOMOVE, SWP_NOACTIVATE = 0x0001, 0x0002, 0x0010


def raise_window(hwnd):
    """把窗口弄到最上层,好让屏幕截图和合成点击都能落在它身上。

    为什么不用 `SetForegroundWindow`:调用方是本进程、不是当前前台进程时,
    Windows 会**静默拒绝**(实测返回 0)。置顶没有这个限制。
    """
    u32.ShowWindow(hwnd, 9)   # SW_RESTORE:最小化的也拉回来
    return u32.SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0,
                            SWP_NOSIZE | SWP_NOMOVE | SWP_NOACTIVATE)


def find(title):
    h = u32.FindWindowW(None, title)
    return h or None


def rect_of(hwnd):
    r = RECT()
    u32.GetWindowRect(hwnd, ctypes.byref(r))
    return r.left, r.top, r.right - r.left, r.bottom - r.top


def visible(hwnd):
    return bool(hwnd) and bool(u32.IsWindowVisible(hwnd))


def grab(x, y, w, h):
    """从屏幕截一块,返回 (宽, 高, 按行 RGB 的 bytes)。"""
    src = u32.GetDC(0)          # 整个屏幕的 DC:桌宠是置顶的,截到的就是它
    mem = g32.CreateCompatibleDC(src)
    bmp = g32.CreateCompatibleBitmap(src, w, h)
    g32.SelectObject(mem, bmp)
    g32.BitBlt(mem, 0, 0, w, h, src, x, y, 0x00CC0020)  # SRCCOPY

    info = BITMAPINFO()
    info.bmiHeader.biSize = ctypes.sizeof(BITMAPINFOHEADER)
    info.bmiHeader.biWidth = w
    info.bmiHeader.biHeight = -h        # 负数 = 自上而下,省得自己翻
    info.bmiHeader.biPlanes = 1
    info.bmiHeader.biBitCount = 32
    info.bmiHeader.biCompression = 0    # BI_RGB
    buf = ctypes.create_string_buffer(w * h * 4)
    g32.GetDIBits(mem, bmp, 0, h, buf, ctypes.byref(info), 0)

    g32.DeleteObject(bmp)
    g32.DeleteDC(mem)
    u32.ReleaseDC(0, src)
    return buf.raw   # BGRA,每像素 4 字节


def find_badge(raw, w, h):
    """在右上角找那块红色的角标,返回 (中心 x, 中心 y, 像素数, bbox)。

    只搜右上角:w >= 60% 宽、y <= 40% 高。**还要按行分带**:桌宠本体也有红色
    (实测头饰/衣服都会命中),不分带的话重心会被拉到宠物身上去。角标永远是最上面
    那一带,所以取「第一段连续出现的红行」就够了 —— 上下差 2 行以内算同一带,
    容忍圆形药丸边缘的锯齿。
    """
    minx, miny, maxx, maxy = w, h, -1, -1
    xs, ys, n = 0, 0, 0
    band_open = False
    gap = 0
    for y in range(0, int(h * 0.4)):
        row = y * w * 4
        hits = []
        for x in range(int(w * 0.6), w):
            i = row + x * 4
            b, g, r = raw[i], raw[i + 1], raw[i + 2]
            # Theme.danger 的两个取值(#d94742 / #ff8078)都满足:偏红、明显压过绿蓝
            if r >= 170 and r - g >= 60 and r - b >= 60:
                hits.append(x)
        if hits:
            gap = 0
            if not band_open:
                band_open = True       # 第一带从这里开始
            if band_open:
                xs += sum(hits)
                ys += y * len(hits)
                n += len(hits)
                minx, miny = min(minx, min(hits)), min(miny, y)
                maxx, maxy = max(maxx, max(hits)), max(maxy, y)
        elif band_open:
            gap += 1
            if gap > 2:                # 断开两行以上 = 这一带领完了
                break

    if n < 20:
        return None
    return (xs // n, ys // n, n, (minx, miny, maxx, maxy))


def click_at(x, y):
    u32.SetCursorPos(x, y)
    ctypes.windll.kernel32.Sleep(200)
    u32.mouse_event(0x0002, 0, 0, 0, None)   # LEFTDOWN
    ctypes.windll.kernel32.Sleep(80)
    u32.mouse_event(0x0004, 0, 0, 0, None)   # LEFTUP


def main():
    do_click = "--click" in sys.argv
    pet = find(PET_TITLE)
    main_w = find(MAIN_TITLE)
    if not pet:
        print("找不到桌宠窗口")
        return 1

    print(f"主窗口 visible={visible(main_w)}")

    # 截之前置顶:桌宠本来就是 always-on-top,但别的置顶窗口可能压着它
    raise_window(pet)
    ctypes.windll.kernel32.Sleep(400)

    x, y, w, h = rect_of(pet)
    raw = grab(x, y, w, h)
    found = find_badge(raw, w, h)
    if not found:
        print(f"桌宠窗口 {w}x{h} at {x},{y} 的右上角没有红色角标")
        return 1

    bx, by, n, (minx, miny, maxx, maxy) = found
    print(f"桌宠 {w}x{h} at {x},{y};角标 bbox=({minx},{miny})-({maxx},{maxy}) "
          f"重心=({bx},{by}) 像素 {n}")
    print(f"点击点(屏幕物理坐标)= {x + bx},{y + by}")

    if do_click:
        click_at(x + bx, y + by)
        ctypes.windll.kernel32.Sleep(900)
        print(f"点完:主窗口 visible={visible(main_w)};桌宠尺寸={rect_of(pet)[2:]}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
