#!/usr/bin/env python3
"""验证桌宠那一圈「悬停弹出来的功能图标」。

    python tools/pet-ring.py hover          # 悬停一下并截图(写 tmp/ring-hover.png)
    python tools/pet-ring.py click list     # 悬停后点某个图标,报告哪个窗口被打开

几何和宿主的算法**必须一致**(见 src/main.rs 的 RING_* 常量),
所以这里的数字都摆在明面上,宿主一改这里也得跟着改。

为什么要在一个进程里一口气做完:宿主的悬停判定读的是**全局光标**,
而这台机器上真鼠标随时会被动一下 —— 分几步(先 This 再 That 再截图)
中间光标就跑掉了,环会自己收起来,看起来像「根本没弹」。
"""
import ctypes
import importlib.util
import os
import struct
import sys
import time
import zlib
from ctypes import wintypes

_spec = importlib.util.spec_from_file_location(
    "bc", os.path.join(os.path.dirname(os.path.abspath(__file__)), "badge-click.py")
)
bc = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(bc)
u32 = bc.u32

# ── 必须和 src/main.rs 里那组常量一致 ──────────────────────────────────
RING_ICON = 44.0
RING_GAP = 20.0
RING_SPREAD = 45.0

# 三个图标:动作名 → 角度(正上方为 0,左边为负)
ACTIONS = {"notes": -RING_SPREAD, "list": 0.0, "settings": RING_SPREAD}


def print_window(hwnd, extra=0):
    """拿窗口**自己画出来的**那一面(PrintWindow),返回 (宽, 高, BGRA 字节)。

    为什么不用屏幕截图:桌宠是置顶窗口,但 Windows 的通知横幅也是置顶的,
    而且能压在它上面 —— 实测截图里宠物的身子被一条「新通知」整个盖住,
    看起来像「宠物没画出来」。PrintWindow 与 z 序无关,拿到的一直是窗口自己的画面。
    代价:窗口没画到的地方是 alpha=0,拼出来是黑的(见下面合成到灰底那步)。
    """
    g32 = bc.g32
    _, _, w, h = bc.rect_of(hwnd)
    h += extra

    u32.PrintWindow.argtypes = [wintypes.HWND, wintypes.HDC, wintypes.UINT]
    u32.PrintWindow.restype = wintypes.BOOL
    src = u32.GetWindowDC(hwnd)
    mem = g32.CreateCompatibleDC(src)
    bmp = g32.CreateCompatibleBitmap(src, w, h)
    g32.SelectObject(mem, bmp)
    u32.PrintWindow(hwnd, mem, 2)  # PW_RENDERFULLCONTENT

    info = bc.BITMAPINFO()
    info.bmiHeader.biSize = ctypes.sizeof(bc.BITMAPINFOHEADER)
    info.bmiHeader.biWidth = w
    info.bmiHeader.biHeight = -h
    info.bmiHeader.biPlanes = 1
    info.bmiHeader.biBitCount = 32
    info.bmiHeader.biCompression = 0
    buf = ctypes.create_string_buffer(w * h * 4)
    g32.GetDIBits(mem, bmp, 0, h, buf, ctypes.byref(info), 0)

    g32.DeleteObject(bmp)
    g32.DeleteDC(mem)
    u32.ReleaseDC(hwnd, src)

    # 没画到的地方 alpha=0,拼出来是黑的 —— 合成到中灰上,才看得清图标描边
    out = bytearray(buf.raw)
    for i in range(0, len(out), 4):
        a = out[i + 3]
        if a != 255:
            for k in (0, 1, 2):
                out[i + k] = (out[i + k] * a + 90 * (255 - a)) // 255
            out[i + 3] = 255
    return (w, h, bytes(out))


def save_png(path, w, h, raw):
    """把 `grab()` 给的 BGRA 原始像素写成 PNG(不带 alpha)。

    没装 Pillow,BitBlt 又只能给原图 —— 那就自己拼一个最小 PNG:
    头 + IHDR + IDAT(zlib) + IEND。240x254 这种尺寸纯 Python 循环也就几十毫秒。
    """
    rows = bytearray()
    for y in range(h):
        rows.append(0)  # 每行的过滤器类型:0 = 不滤
        base = y * w * 4
        for x in range(w):
            i = base + x * 4
            rows += bytes((raw[i + 2], raw[i + 1], raw[i]))  # BGRA -> RGB

    def chunk(tag, data):
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(bytes(rows), 6))
    png += chunk(b"IEND", b"")
    with open(path, "wb") as f:
        f.write(png)


def pet_geometry():
    """返回 (窗口 rect, 宠物边长, 宠物中心屏幕坐标)。"""
    pet = bc.find(bc.PET_TITLE)
    if not pet:
        raise SystemExit("找不到桌宠窗口(应用没在跑?)")
    x, y, w, h = bc.rect_of(pet)
    # 宠物边长:环开着时窗口高 - 环那一条,否则就是窗口高 - 给角标留的 18px。
    # 直接从窗口反推不可靠,用宿主那份存盘值反而更直接 —— 但这里不想读数据文件,
    # 所以用「环没开时窗口高 - 18」这个式子(悬停之前测一次就够)。
    return pet, (x, y, w, h)


def hover_and_grab(settle=0.4):
    """把光标挪到宠物身上,等环弹出来,再把窗口那一块截下来。

    **先把光标挪远、等环收起来再量宠物大小**:环开着时窗口比宠物高一条
    (`pet-ring-band`),拿那个高度反推宠物边长会多算 64px,后面所有坐标全歪
    (实测就是这么点空了一次)。
    """
    pet = bc.find(bc.PET_TITLE)
    u32.SetCursorPos(60, 60)
    time.sleep(0.45)                          # 环收起来,窗口回到「宠物 + 角标余量」
    x, y, w, h = bc.rect_of(pet)
    pet_size = h - 18.0
    cx, cy = x + w / 2, y + h - pet_size / 2  # 贴底居中,圆心就是这个
    u32.SetCursorPos(int(cx), int(cy))
    time.sleep(settle)
    x2, y2, w2, h2 = bc.rect_of(pet)
    W, H, raw = print_window(pet)
    if (w2, h2) != (W, H):
        raw = bc.grab(x2, y2, int(w2), int(h2))
        W, H = int(w2), int(h2)
    return pet_size, (x2, y2, w2, h2), raw


def icon_screen_pos(rect, pet_size, angle_deg):
    """某个图标在屏幕上的中心坐标 —— 和 ui/pet.slint 里那两行公式一模一样。"""
    x, y, w, h = rect
    r = pet_size / 2 + RING_GAP + RING_ICON / 2
    rad = angle_deg * 3.141592653589793 / 180.0
    # 宠物中心(窗口里:横向居中、贴底)
    cx = x + w / 2
    cy = y + h - pet_size / 2
    # Slint 那边 y 往上为正,屏幕坐标往下为正
    return (cx + r * __import__("math").sin(rad), cy - r * __import__("math").cos(rad))


def main():
    cmd = sys.argv[1] if len(sys.argv) > 1 else "hover"

    if cmd == "hover":
        pet_size, rect, raw = hover_and_grab()
        out = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                           "tmp", "ring-hover.png")
        save_png(out, int(rect[2]), int(rect[3]), raw)
        print(f"宠物边长 {pet_size:.0f};悬停后窗口 {rect}")
        print(f"截图写到 {out}")
        return 0

    if cmd == "click":
        action = sys.argv[2] if len(sys.argv) > 2 else "list"
        angle = ACTIONS[action]

        pet_size, rect, raw = hover_and_grab()
        print(f"宠物边长 {pet_size:.0f};悬停后窗口 {rect}")
        out = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                           "tmp", f"ring-{action}.png")
        save_png(out, int(rect[2]), int(rect[3]), raw)

        ix, iy = icon_screen_pos(rect, pet_size, angle)
        print(f"点「{action}」(角度 {angle:+.0f}°)→ 屏幕 ({ix:.0f},{iy:.0f})")

        def vis(title):
            h = bc.find(title)
            return bool(h) and bool(u32.IsWindowVisible(h))

        watch = {"notes": "待办清单 · 笔记", "list": "待办清单",
                 "settings": "待办清单 · 设置"}[action]
        print(f"点之前 {watch} visible = {vis(watch)}")
        bc.click_at(int(ix), int(iy))
        time.sleep(1.0)
        print(f"点之后 {watch} visible = {vis(watch)}")
        print(f"环收起来了没有:{bc.rect_of(bc.find(bc.PET_TITLE))}")
        return 0

    print(__doc__)
    return 2


if __name__ == "__main__":
    sys.exit(main())
