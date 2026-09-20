#!/usr/bin/env python
"""生成 assets/icon.ico(多尺寸)。

图标是现画的:圆角方块 + 对勾,和托盘图标、标题栏图标同一套设计。
改设计只改这个脚本重跑,不用找美术资源:

    python tools/make_icon.py
"""
import struct
import zlib

SIZES = [16, 24, 32, 48, 64, 128, 256]
# 主题色和托盘图标保持一致(ui/widgets.slint 里的 Theme.accent)
ACCENT = (0x3B, 0x74, 0xF2)
MARK = (0xFF, 0xFF, 0xFF)


def seg_dist(p, a, b):
    """点到线段的距离"""
    (px, py), (ax, ay), (bx, by) = p, a, b
    dx, dy = bx - ax, by - ay
    len2 = dx * dx + dy * dy
    t = max(0.0, min(1.0, ((px - ax) * dx + (py - ay) * dy) / len2))
    cx, cy = ax + t * dx, ay + t * dy
    return ((px - cx) ** 2 + (py - cy) ** 2) ** 0.5


def render(size):
    """画一张 size×size 的 RGBA 图(4 倍超采样抗锯齿)"""
    ss = 4
    radius = size * 0.22
    # 对勾两段线,坐标是 0..1
    a, b, c = (0.30, 0.52), (0.44, 0.68), (0.72, 0.33)
    stroke = 0.075
    px = bytearray()
    for y in range(size):
        px.append(0)  # PNG 每行的 filter 字节
        for x in range(size):
            r = g = bl = a_acc = 0
            for sy in range(ss):
                for sx in range(ss):
                    fx = (x + (sx + 0.5) / ss) / size
                    fy = (y + (sy + 0.5) / ss) / size
                    # 圆角矩形内部判定
                    cx = min(max(fx * size, radius), size - radius)
                    cy = min(max(fy * size, radius), size - radius)
                    inside = ((fx * size - cx) ** 2 + (fy * size - cy) ** 2) ** 0.5 <= radius
                    if not inside:
                        continue
                    on_mark = min(seg_dist((fx, fy), a, b), seg_dist((fx, fy), b, c)) < stroke
                    col = MARK if on_mark else ACCENT
                    r += col[0]
                    g += col[1]
                    bl += col[2]
                    a_acc += 255
            n = ss * ss
            if a_acc == 0:
                px += bytes((0, 0, 0, 0))
            else:
                # 按覆盖的采样数加权平均颜色
                cov = a_acc // 255
                px += bytes((r // cov, g // cov, bl // cov, a_acc // n))
    return bytes(px)


def png(size, rgba):
    raw = rgba

    def chunk(typ, data):
        return (struct.pack(">I", len(data)) + typ + data
                + struct.pack(">I", zlib.crc32(typ + data) & 0xFFFFFFFF))

    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw, 9))
            + chunk(b"IEND", b""))


def main():
    images = [(s, png(s, render(s))) for s in SIZES]
    header = struct.pack("<HHH", 0, 1, len(images))
    offset = len(header) + 16 * len(images)
    entries, blobs = b"", b""
    for size, data in images:
        dim = 0 if size >= 256 else size
        entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset)
        blobs += data
        offset += len(data)
    out = r"E:\work\rgui\assets\icon.ico"
    open(out, "wb").write(header + entries + blobs)
    print(f"已生成 {out}({len(SIZES)} 个尺寸:{SIZES})")


main()
