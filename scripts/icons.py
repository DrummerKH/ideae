#!/usr/bin/env python3
"""Makes assets/icon/ideae.icns (macOS) and ideae.ico (Windows) from
assets/icon/ideae.svg; Linux uses the SVG as it is. Run it after changing
the SVG and commit the results. Needs rsvg-convert, or sips on macOS."""

import os, shutil, struct, subprocess, sys, tempfile, zlib

DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "assets", "icon")
# macOS draws the tile at 824 of 1024 points; the SVG's tile is 56 of 64.
MAC_SCALE = (824 / 1024) / (56 / 64)
# PNG chunks only; the 16 and 32 pixel ones want another encoding, and
# macOS scales the retina ones (ic11, ic12) down instead.
ICNS = [("ic11", 32), ("ic12", 64), ("ic07", 128), ("ic13", 256), ("ic08", 256), ("ic14", 512), ("ic09", 512),
        ("ic10", 1024)]
ICO = [16, 20, 24, 32, 40, 48, 64, 256]


def render(svg, size, tmp):
    out = os.path.join(tmp, f"{size}.png")
    if shutil.which("rsvg-convert"):
        subprocess.run(["rsvg-convert", "-w", str(size), "-h", str(size), "-o", out, svg], check=True)
    elif shutil.which("sips"):
        # sips draws small sizes badly: draw large, then scale down.
        big = os.path.join(tmp, "big.png")
        quiet = {"check": True, "stdout": subprocess.DEVNULL}
        subprocess.run(["sips", "-s", "format", "png", "-z", "1024", "1024", svg, "--out", big], **quiet)
        subprocess.run(["sips", "-z", str(size), str(size), big, "--out", out], **quiet)
    else:
        sys.exit("icons.py: rsvg-convert or sips is needed")
    with open(out, "rb") as f:
        return f.read()


def pixels(png):
    """A PNG as (width, height, rows of RGBA bytes)."""
    pos, data, head = 8, b"", None
    while pos < len(png):
        length, kind = struct.unpack(">I4s", png[pos:pos + 8])
        body = png[pos + 8:pos + 8 + length]
        if kind == b"IHDR":
            head = struct.unpack(">IIBBBBB", body)
        elif kind == b"IDAT":
            data += body
        pos += 12 + length
    width, height, depth, color, _, _, interlace = head
    channels = {2: 3, 6: 4}.get(color)
    if depth != 8 or not channels or interlace:
        sys.exit("icons.py: the renderer made a PNG this script cannot read")
    raw, stride, rows, prev = zlib.decompress(data), width * channels, [], bytearray(width * channels)
    for y in range(height):
        start = y * (stride + 1)
        kind, row = raw[start], bytearray(raw[start + 1:start + 1 + stride])
        for i in range(stride):
            left = row[i - channels] if i >= channels else 0
            up = prev[i]
            corner = prev[i - channels] if i >= channels else 0
            if kind == 1:
                row[i] = (row[i] + left) & 255
            elif kind == 2:
                row[i] = (row[i] + up) & 255
            elif kind == 3:
                row[i] = (row[i] + (left + up) // 2) & 255
            elif kind == 4:
                p = left + up - corner
                pa, pb, pc = abs(p - left), abs(p - up), abs(p - corner)
                row[i] = (row[i] + (left if pa <= pb and pa <= pc else up if pb <= pc else corner)) & 255
        prev = row
        if channels == 3:
            row = bytearray(b"".join(bytes(row[i:i + 3]) + b"\xff" for i in range(0, stride, 3)))
        rows.append(row)
    return width, height, rows


def bitmap(png):
    """The 32-bit bitmap an .ico holds for small sizes: BGRA rows from the
    bottom up, then a one-bit mask."""
    width, height, rows = pixels(png)
    out = struct.pack("<IiiHHIIiiII", 40, width, height * 2, 1, 32, 0, 0, 0, 0, 0, 0)
    for row in reversed(rows):
        out += b"".join(bytes((row[i + 2], row[i + 1], row[i], row[i + 3])) for i in range(0, width * 4, 4))
    mask_stride = (width + 31) // 32 * 4
    for row in reversed(rows):
        bits = bytearray(mask_stride)
        for x in range(width):
            if row[x * 4 + 3] == 0:
                bits[x // 8] |= 0x80 >> (x % 8)
        out += bytes(bits)
    return out


def main():
    svg = os.path.join(DIR, "ideae.svg")
    with open(svg) as f:
        text = f.read()
    with tempfile.TemporaryDirectory() as tmp:
        head, rest = text.split(">", 1)
        body = rest.rsplit("</svg>", 1)[0]
        mac_svg = os.path.join(tmp, "mac.svg")
        with open(mac_svg, "w") as f:
            f.write(f'{head}><g transform="translate(32 32) scale({MAC_SCALE:.4f}) translate(-32 -32)">{body}</g></svg>')

        chunks = b""
        for kind, size in ICNS:
            png = render(mac_svg, size, tmp)
            chunks += kind.encode() + struct.pack(">I", len(png) + 8) + png
        with open(os.path.join(DIR, "ideae.icns"), "wb") as f:
            f.write(b"icns" + struct.pack(">I", len(chunks) + 8) + chunks)

        images = [(size, render(svg, size, tmp)) for size in ICO]
        images = [(size, png if size >= 256 else bitmap(png)) for size, png in images]
        offset = 6 + 16 * len(images)
        entries, blob = b"", b""
        for size, data in images:
            entries += struct.pack("<BBBBHHII", size % 256, size % 256, 0, 0, 1, 32, len(data), offset + len(blob))
            blob += data
        with open(os.path.join(DIR, "ideae.ico"), "wb") as f:
            f.write(struct.pack("<HHH", 0, 1, len(images)) + entries + blob)


main()
