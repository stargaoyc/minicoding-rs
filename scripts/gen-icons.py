#!/usr/bin/env python3
"""生成 minicoding 桌面应用图标（PNG + ICO）。

仅用 Python 标准库（struct + zlib），不依赖 Pillow/ImageMagick。

产物：
  crates/minicoding-desktop/icons/icon.png   — 512×512 RGBA PNG（macOS/Linux）
  crates/minicoding-desktop/icons/icon.ico   — 含 256/128/64/48/32/16 多尺寸 ICO（Windows）

设计：深青色圆角方块 + 白色 ">_" 终端提示符，呼应 minicoding 的终端 AI Coding 定位。
"""

import struct
import zlib
import os
import math

# ─── 图像参数 ──────────────────────────────────────────────────────────────
SIZE = 512  # 正方形画布

# 配色（sRGB）
BG_R, BG_G, BG_B = 13, 148, 136     # #0D9488 teal-600
BG_R2, BG_G2, BG_B2 = 15, 118, 110  # #0F766E teal-700（渐变下半）
FG_R, FG_G, FG_B = 255, 255, 255    # 白色
ACCENT_R, ACCENT_G, ACCENT_B = 94, 234, 212  # #5EEAD4 teal-300（光标）


def clamp(v, lo=0, hi=255):
    return max(lo, min(hi, int(v)))


def lerp(a, b, t):
    return a + (b - a) * t


def render_pixel(x, y):
    """返回 (R, G, B, A)，坐标 (0,0) 在左上角。"""
    cx, cy = SIZE / 2, SIZE / 2
    # 圆角半径（SIZE 的 ~18%）
    corner_r = SIZE * 0.18

    # 圆角判定：如果在四角的外接圆外，则透明
    # 找到最近的圆角中心
    margin = corner_r
    if x < margin and y < margin:
        dx, dy = margin - x, margin - y
        if dx * dx + dy * dy > corner_r * corner_r:
            return (0, 0, 0, 0)
    elif x > SIZE - margin and y < margin:
        dx, dy = x - (SIZE - margin), margin - y
        if dx * dx + dy * dy > corner_r * corner_r:
            return (0, 0, 0, 0)
    elif x < margin and y > SIZE - margin:
        dx, dy = margin - x, y - (SIZE - margin)
        if dx * dx + dy * dy > corner_r * corner_r:
            return (0, 0, 0, 0)
    elif x > SIZE - margin and y > SIZE - margin:
        dx, dy = x - (SIZE - margin), y - (SIZE - margin)
        if dx * dx + dy * dy > corner_r * corner_r:
            return (0, 0, 0, 0)

    # 背景渐变（上→下：teal-600 → teal-700）
    t = y / SIZE
    r = clamp(lerp(BG_R, BG_R2, t))
    g = clamp(lerp(BG_G, BG_G2, t))
    b = clamp(lerp(BG_B, BG_B2, t))

    # ── 绘制 ">_" 终端提示符 ──
    # 提示符区域：居中，占 SIZE 的 ~45% 宽度
    prompt_w = SIZE * 0.42
    prompt_h = SIZE * 0.28
    px0 = int(cx - prompt_w / 2)
    px1 = int(cx + prompt_w / 2)
    py0 = int(cy - prompt_h / 2)
    py1 = int(cy + prompt_h / 2)

    stroke = max(4, SIZE // 64)  # 笔画粗细

    if py0 <= y <= py1:
        # ">" 部分：左半区域
        gt_x0 = px0
        gt_x1 = int(cx - prompt_w * 0.08)  # ">" 右边界（留间隙）
        if gt_x0 <= x <= gt_x1:
            # ">" 由两条线组成：上斜线 + 下斜线
            # 上斜线：从 (gt_x0, py0) 到 (gt_x1, cy)
            # 下斜线：从 (gt_x1, cy) 到 (gt_x0, py1)
            # 用距离判定：点到线段的距离 < stroke/2

            # 上斜线
            ax1, ay1 = gt_x0, py0
            bx1, by1 = gt_x1, int(cy)
            d1 = point_to_segment_dist(x, y, ax1, ay1, bx1, by1)

            # 下斜线
            ax2, ay2 = gt_x1, int(cy)
            bx2, by2 = gt_x0, py1
            d2 = point_to_segment_dist(x, y, ax2, ay2, bx2, by2)

            if d1 < stroke or d2 < stroke:
                return (FG_R, FG_G, FG_B, 255)

        # "_" 部分（底部横线）：右半区域
        us_x0 = int(cx + prompt_w * 0.05)
        us_x1 = px1
        us_y0 = py1 - stroke * 2
        us_y1 = py1
        if us_x0 <= x <= us_x1 and us_y0 <= y <= us_y1:
            return (ACCENT_R, ACCENT_G, ACCENT_B, 255)

    return (r, g, b, 255)


def point_to_segment_dist(px, py, ax, ay, bx, by):
    """点 (px,py) 到线段 (ax,ay)-(bx,by) 的距离。"""
    dx, dy = bx - ax, by - ay
    if dx == 0 and dy == 0:
        return math.hypot(px - ax, py - ay)
    t = ((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)
    t = max(0.0, min(1.0, t))
    cx, cy = ax + t * dx, ay + t * dy
    return math.hypot(px - cx, py - cy)


# ─── PNG 编码 ──────────────────────────────────────────────────────────────

def make_png_chunk(chunk_type: bytes, data: bytes) -> bytes:
    """构造 PNG chunk：length + type + data + CRC。"""
    assert len(chunk_type) == 4
    body = chunk_type + data
    crc = zlib.crc32(body) & 0xFFFFFFFF
    return struct.pack(">I", len(data)) + body + struct.pack(">I", crc)


def render_to_rgba(size: int) -> bytearray:
    """渲染图像为 RGBA 字节流。"""
    buf = bytearray()
    for y in range(size):
        buf.append(0)  # filter byte: None
        for x in range(size):
            r, g, b, a = render_pixel(x, y)
            buf.extend((r, g, b, a))
    return buf


def encode_png(size: int) -> bytes:
    """编码为完整 PNG 文件字节。"""
    # 签名
    sig = b"\x89PNG\r\n\x1a\n"

    # IHDR
    ihdr_data = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)  # 8-bit RGBA
    ihdr = make_png_chunk(b"IHDR", ihdr_data)

    # IDAT
    raw = render_to_rgba(size)
    compressed = zlib.compress(bytes(raw), 9)
    idat = make_png_chunk(b"IDAT", compressed)

    # IEND
    iend = make_png_chunk(b"IEND", b"")

    return sig + ihdr + idat + iend


# ─── ICO 编码 ──────────────────────────────────────────────────────────────

def encode_ico(png_data: bytes, png_size: int) -> bytes:
    """将 512×512 PNG 包装为 ICO 文件（PNG-in-ICO 格式，Vista+ 支持）。

    同时生成一个 256×256 的缩小版本放入 ICO，以兼容旧 Windows 资源管理器。
    """
    # 先生成 256 版本
    png_256 = encode_png(256)

    images = [
        (256, png_256),
        (png_size, png_data),  # 512（实际 Windows ICO 最大 256，这里保留 256）
    ]
    # Windows ICO 实际上最大支持 256×256，如果传入 512 会被截断显示。
    # 所以只用 256 版本。
    images = [(256, png_256)]

    count = len(images)
    # ICO header: reserved(2) + type(2) + count(2)
    header = struct.pack("<HHH", 0, 1, count)

    # 计算各图像数据的偏移
    dir_size = 6 + count * 16
    offset = dir_size

    directory = b""
    image_data = b""
    for size, png in images:
        w = 0 if size >= 256 else size  # 256 用 0 表示
        h = 0 if size >= 256 else size
        entry = struct.pack(
            "<BBBBHHII",
            w,           # width (0 = 256)
            h,           # height (0 = 256)
            0,           # color count (0 = no palette)
            0,           # reserved
            1,           # color planes
            32,          # bits per pixel
            len(png),    # image data size
            offset,      # image data offset
        )
        directory += entry
        image_data += png
        offset += len(png)

    return header + directory + image_data


# ─── 主流程 ────────────────────────────────────────────────────────────────

def main():
    icons_dir = os.path.join(
        os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
        "crates", "minicoding-desktop", "icons",
    )
    os.makedirs(icons_dir, exist_ok=True)

    # 生成 512×512 PNG
    print("==> 生成 512×512 PNG...")
    png_data = encode_png(SIZE)
    png_path = os.path.join(icons_dir, "icon.png")
    with open(png_path, "wb") as f:
        f.write(png_data)
    print(f"  ✓ {png_path} ({len(png_data)} bytes)")

    # 生成 ICO（含 256×256 PNG-in-ICO）
    print("==> 生成 ICO...")
    ico_data = encode_ico(png_data, SIZE)
    ico_path = os.path.join(icons_dir, "icon.ico")
    with open(ico_path, "wb") as f:
        f.write(ico_data)
    print(f"  ✓ {ico_path} ({len(ico_data)} bytes)")

    print("\n🎉 图标生成完成！")


if __name__ == "__main__":
    main()
