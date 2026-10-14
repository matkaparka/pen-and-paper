# SPDX-License-Identifier: GPL-3.0-or-later
"""读 PNG 的像素，给验收脚本用。只支持 8 位 RGB / RGBA、非隔行，够读我们自己的截图和导出图。

用法（当模块）：
    from pngpx import load
    w, h, px = load('shot.png')     # px(x, y) -> (r, g, b)
"""
import struct
import zlib


def load(path):
    data = open(path, 'rb').read()
    assert data[:8] == b'\x89PNG\r\n\x1a\n', '不是 PNG'
    pos, idat, w, h, channels = 8, b'', 0, 0, 0
    while pos < len(data):
        (length,), kind = struct.unpack('>I', data[pos:pos + 4]), data[pos + 4:pos + 8]
        body = data[pos + 8:pos + 8 + length]
        if kind == b'IHDR':
            w, h, depth, color, _, _, interlace = struct.unpack('>IIBBBBB', body)
            assert depth == 8 and color in (2, 6) and interlace == 0, '只支持 8 位 RGB/RGBA、非隔行'
            channels = 3 if color == 2 else 4
        elif kind == b'IDAT':
            idat += body
        pos += 12 + length
    raw = zlib.decompress(idat)
    stride = w * channels
    rows, prev = [], bytearray(stride)
    for y in range(h):
        f = raw[y * (stride + 1)]
        line = bytearray(raw[y * (stride + 1) + 1:(y + 1) * (stride + 1)])
        if f == 1:
            for i in range(channels, stride):
                line[i] = (line[i] + line[i - channels]) & 255
        elif f == 2:
            for i in range(stride):
                line[i] = (line[i] + prev[i]) & 255
        elif f == 3:
            for i in range(stride):
                left = line[i - channels] if i >= channels else 0
                line[i] = (line[i] + ((left + prev[i]) >> 1)) & 255
        elif f == 4:
            for i in range(stride):
                a = line[i - channels] if i >= channels else 0
                b = prev[i]
                c = prev[i - channels] if i >= channels else 0
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                line[i] = (line[i] + (a if pa <= pb and pa <= pc else b if pb <= pc else c)) & 255
        rows.append(line)
        prev = line

    def px(x, y):
        o = x * channels
        return rows[y][o], rows[y][o + 1], rows[y][o + 2]

    return w, h, px
