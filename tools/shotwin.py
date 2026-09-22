#!/usr/bin/env python3
"""按窗口标题截图(PrintWindow,不怕被别的窗口盖住)。

    python tools/shotwin.py "待办清单 · 设置" tmp/settings.png
    python tools/shotwin.py "待办清单 · 笔记" tmp/notes.png

为什么不抓屏:Windows 的通知横幅是置顶的,能压在别的窗口上面(桌宠上踩过这个坑,
见 pet-ring.py 的说明)。PrintWindow 与 z 序无关,拿到的一直是窗口自己画的画面。

宿主那一侧的底座(找窗口、取矩形、读像素、写 PNG)都复用 pet-ring.py ——
它已经把那套 ctypes 声明和 alpha 合成写好了。
"""
import importlib.util
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)


def _load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


pr = _load("pr", os.path.join(HERE, "pet-ring.py"))
bc = pr.bc


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    title = sys.argv[1]
    out = sys.argv[2] if len(sys.argv) > 2 else os.path.join(ROOT, "tmp", "shot.png")

    hwnd = bc.find(title)
    if not hwnd:
        raise SystemExit(f"找不到窗口「{title}」(应用没在跑?窗口没开?)")
    if not bc.u32.IsWindowVisible(hwnd):
        raise SystemExit(f"窗口「{title}」存在但不可见")

    w, h, raw = pr.print_window(hwnd)
    os.makedirs(os.path.dirname(out), exist_ok=True)
    pr.save_png(out, w, h, raw)
    print(f"{title}: {bc.rect_of(hwnd)} -> {out} ({w}x{h})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
