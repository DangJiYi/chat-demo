"""从 assets/icon.png 生成所有平台需要的图标。

只用标准库（zlib + struct），不依赖 Pillow——CI 与本地都不必额外装包。

产出：
    rust/assets/app.ico                         Windows exe 图标 + 托盘图标
    android/app/src/main/res/mipmap-*/ic_launcher.png   安卓各密度启动图标

用法（在 chat/ 目录下）：
    python tools/make-icons.py

为什么把生成物提交进仓库而不是构建时生成：
    build.rs 只负责把已存在的 .ico 嵌进 exe。图标是素材、不是编译产物，
    生成过程留在版本库里，换图时跑一次脚本、看一眼 diff 就够了。
"""

import os
import struct
import sys
import zlib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC = os.path.join(ROOT, "assets", "icon.png")
ICO_OUT = os.path.join(ROOT, "rust", "assets", "app.ico")
ANDROID_RES = os.path.join(ROOT, "android", "app", "src", "main", "res")

# Windows 资源管理器/任务栏/托盘会从这里挑最合适的一档。
# 16/24/32 是托盘与列表视图，48/64 是中等图标，256 是大图标视图。
ICO_SIZES = [16, 24, 32, 48, 64, 128, 256]

# 安卓各密度下启动图标边长（48dp）
ANDROID_SIZES = {
    "mipmap-mdpi": 48,
    "mipmap-hdpi": 72,
    "mipmap-xhdpi": 96,
    "mipmap-xxhdpi": 144,
    "mipmap-xxxhdpi": 192,
}


# ---------------------------------------------------------------- PNG 解码

def read_png(path):
    """读 8 位 RGBA、非隔行的 PNG，返回 (宽, 高, bytearray RGBA)。"""
    with open(path, "rb") as f:
        data = f.read()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise SystemExit("不是 PNG: " + path)

    pos = 8
    width = height = None
    idat = bytearray()
    while pos < len(data):
        (length,) = struct.unpack(">I", data[pos : pos + 4])
        ctype = data[pos + 4 : pos + 8]
        body = data[pos + 8 : pos + 8 + length]
        pos += 12 + length
        if ctype == b"IHDR":
            width, height, depth, color, _comp, _filt, interlace = struct.unpack(
                ">IIBBBBB", body
            )
            if depth != 8 or color != 6 or interlace != 0:
                raise SystemExit(
                    "只支持 8 位 RGBA 非隔行 PNG，实际 depth=%d color=%d interlace=%d"
                    % (depth, color, interlace)
                )
        elif ctype == b"IDAT":
            idat += body
        elif ctype == b"IEND":
            break

    raw = zlib.decompress(bytes(idat))
    stride = width * 4
    out = bytearray(width * height * 4)
    prev = bytearray(stride)
    pos = 0
    for y in range(height):
        ftype = raw[pos]
        pos += 1
        line = bytearray(raw[pos : pos + stride])
        pos += stride
        # 逐像素反滤波（PNG 规范 §9.2）
        if ftype == 1:  # Sub
            for i in range(4, stride):
                line[i] = (line[i] + line[i - 4]) & 0xFF
        elif ftype == 2:  # Up
            for i in range(stride):
                line[i] = (line[i] + prev[i]) & 0xFF
        elif ftype == 3:  # Average
            for i in range(stride):
                left = line[i - 4] if i >= 4 else 0
                line[i] = (line[i] + ((left + prev[i]) >> 1)) & 0xFF
        elif ftype == 4:  # Paeth
            for i in range(stride):
                a = line[i - 4] if i >= 4 else 0
                b = prev[i]
                c = prev[i - 4] if i >= 4 else 0
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                if pa <= pb and pa <= pc:
                    pr = a
                elif pb <= pc:
                    pr = b
                else:
                    pr = c
                line[i] = (line[i] + pr) & 0xFF
        elif ftype != 0:
            raise SystemExit("未知 PNG 滤波类型 %d（第 %d 行）" % (ftype, y))
        out[y * stride : (y + 1) * stride] = line
        prev = line
    return width, height, out


# ---------------------------------------------------------------- 缩放

def resize(w, h, px, nw, nh):
    """面积平均（box filter）缩小。缩小时比最近邻干净得多。

    只用于缩小；目标比源大时按最近邻放大（本脚本用不到）。
    """
    if nw == w and nh == h:
        return bytearray(px)
    if nw > w or nh > h:
        out = bytearray(nw * nh * 4)
        for y in range(nh):
            sy = min(h - 1, y * h // nh)
            for x in range(nw):
                sx = min(w - 1, x * w // nw)
                si = (sy * w + sx) * 4
                di = (y * nw + x) * 4
                out[di : di + 4] = px[si : si + 4]
        return out

    out = bytearray(nw * nh * 4)
    x_edges = [x * w // nw for x in range(nw + 1)]
    y_edges = [y * h // nh for y in range(nh + 1)]
    for y in range(nh):
        y0, y1 = y_edges[y], y_edges[y + 1]
        for x in range(nw):
            x0, x1 = x_edges[x], x_edges[x + 1]
            r = g = b = a = 0
            n = 0
            for sy in range(y0, y1):
                base = sy * w * 4
                for sx in range(x0, x1):
                    si = base + sx * 4
                    r += px[si]
                    g += px[si + 1]
                    b += px[si + 2]
                    a += px[si + 3]
                    n += 1
            di = (y * nw + x) * 4
            if n:
                out[di] = r // n
                out[di + 1] = g // n
                out[di + 2] = b // n
                out[di + 3] = a // n
    return out


# ---------------------------------------------------------------- PNG 编码

def write_png(path, w, h, px):
    raw = bytearray()
    stride = w * 4
    for y in range(h):
        raw.append(0)  # filter: None
        raw += px[y * stride : (y + 1) * stride]

    def chunk(ctype, body):
        return (
            struct.pack(">I", len(body))
            + ctype
            + body
            + struct.pack(">I", zlib.crc32(ctype + body) & 0xFFFFFFFF)
        )

    out = b"\x89PNG\r\n\x1a\n"
    out += chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
    out += chunk(b"IDAT", zlib.compress(bytes(raw), 9))
    out += chunk(b"IEND", b"")
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(out)
    return len(out)


# ---------------------------------------------------------------- ICO 编码

def ico_dib(w, h, px):
    """把 RGBA 像素转成 ICO 内嵌的 BMP 形式（32bpp BGRA + AND 掩码）。

    用 BMP 而不是 PNG：PNG 只在 Vista 以后的 256 档可靠，
    小尺寸走 BMP 兼容性最好（托盘图标经不起兼容性意外）。
    注意 BMP 在 ICO 里是**自下而上**存的，且 biHeight 要写两倍。
    """
    header = struct.pack(
        "<IiiHHIIiiII",
        40,          # biSize
        w,           # biWidth
        h * 2,       # biHeight = XOR + AND
        1,           # biPlanes
        32,          # biBitCount
        0,           # biCompression = BI_RGB
        w * h * 4,   # biSizeImage
        0, 0, 0, 0,
    )
    # XOR 位图：BGRA，自下而上
    xor = bytearray()
    for y in range(h - 1, -1, -1):
        row = y * w * 4
        for x in range(w):
            i = row + x * 4
            r, g, b, a = px[i], px[i + 1], px[i + 2], px[i + 3]
            xor += bytes((b, g, r, a))
    # AND 掩码：全透明处置 1，每行按 4 字节对齐
    mask_row = ((w + 31) // 32) * 4
    mask = bytearray(mask_row * h)
    for y in range(h - 1, -1, -1):
        brow = (h - 1 - y) * mask_row
        row = y * w * 4
        for x in range(w):
            if px[row + x * 4 + 3] == 0:
                mask[brow + x // 8] |= 0x80 >> (x % 8)
    return header + bytes(xor) + bytes(mask)


def write_ico(path, src_w, src_h, src_px):
    entries = []
    for size in ICO_SIZES:
        px = resize(src_w, src_h, src_px, size, size)
        entries.append((size, ico_dib(size, size, px)))

    # ICO 目录 + 数据
    count = len(entries)
    header = struct.pack("<HHH", 0, 1, count)
    offset = 6 + 16 * count
    directory = bytearray()
    blobs = bytearray()
    for size, blob in entries:
        directory += struct.pack(
            "<BBBBHHII",
            0 if size >= 256 else size,   # width（0 表示 256）
            0 if size >= 256 else size,   # height
            0,                            # 调色板色数
            0,                            # reserved
            1,                            # planes
            32,                           # bitcount
            len(blob),
            offset,
        )
        blobs += blob
        offset += len(blob)

    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(header + bytes(directory) + bytes(blobs))
    return 6 + 16 * count + len(blobs)


# ---------------------------------------------------------------- main

def main():
    if not os.path.isfile(SRC):
        raise SystemExit("找不到源图: " + SRC)

    w, h, px = read_png(SRC)
    corner = px[3]
    transparent = sum(1 for i in range(3, len(px), 4) if px[i] < 255)
    print("源图 %dx%d，左上角 alpha=%d，非全透明像素 %d / %d"
          % (w, h, corner, transparent, w * h))

    size = write_ico(ICO_OUT, w, h, px)
    print("写入 %s（%d 档，%.1f KB）"
          % (os.path.relpath(ICO_OUT, ROOT), len(ICO_SIZES), size / 1024))

    for folder, side in ANDROID_SIZES.items():
        out = os.path.join(ANDROID_RES, folder, "ic_launcher.png")
        scaled = resize(w, h, px, side, side)
        n = write_png(out, side, side, scaled)
        print("写入 %-28s %3dpx  %.1f KB"
              % (os.path.relpath(out, ROOT), side, n / 1024))
    return 0


if __name__ == "__main__":
    sys.exit(main())
