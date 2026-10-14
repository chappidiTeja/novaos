"""Shared machinery for the screen tests (screen_test.py, gfx_test.py).

The expected picture is built independently of the kernel:
  * the text on screen is worked out by replaying the serial log through a small
    terminal emulator (the screen shows the same bytes the serial port gets);
  * glyph shapes come from the font crate's own source file, not from kernel code;
  * the layout (cell size, margin, cursor) is written out again here.
A bug in the kernel's drawing code therefore cannot also hide in the tests.
"""
import glob
import os
import re
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time
import zlib

SCALE, GLYPH, PAD, TAB = 2, 8, 8, 8
CELL = GLYPH * SCALE
BG = (16, 24, 40)
FG = (220, 220, 220)
MAX_CELLS = 65536


def fail(message):
    print("SCREEN TEST FAILED: " + message)
    sys.exit(1)


def load_font():
    """BASIC_LEGACY from the font8x8 crate: 128 glyphs of 8 row bytes, bit 0 = leftmost."""
    paths = glob.glob(os.path.expanduser("~/.cargo/registry/src/*/font8x8-*/src/legacy.rs"))
    if not paths:
        fail("cannot find the font8x8 crate source to read glyph shapes from")
    text = open(paths[0]).read()
    start = text.index("pub const BASIC_LEGACY")
    end = text.index("\n];", start)
    glyphs = []
    # Entries are either eight explicit bytes or the name of an all-blank constant.
    for entry in re.finditer(r"NOTHING_TO_DISPLAY|\[((?:0x[0-9A-Fa-f]{2},?\s*){8})\]", text[start:end]):
        if entry.group(1) is None:
            glyphs.append([0] * 8)
        else:
            glyphs.append([int(v, 16) for v in re.findall(r"0x([0-9A-Fa-f]{2})", entry.group(1))])
    if len(glyphs) != 128 or any(len(g) != 8 for g in glyphs):
        fail("unexpected font table: %d glyphs" % len(glyphs))
    return glyphs


FONT = load_font()


def glyph_rows(ch):
    """Sixteen pixel rows (as RGB byte strings of 16 pixels) for character `ch`."""
    code = 0x20 if ch == 0 else ch
    rows = []
    for gy in range(GLYPH):
        bits = FONT[code & 0x7F][gy]
        row = b"".join(
            bytes(FG if bits >> gx & 1 else BG) * SCALE for gx in range(GLYPH)
        )
        rows.extend([row] * SCALE)
    return rows


_glyph_cache = {}


def cached_glyph(ch):
    if ch not in _glyph_cache:
        _glyph_cache[ch] = glyph_rows(ch)
    return _glyph_cache[ch]


class Terminal:
    """What the kernel's console should be showing after receiving some bytes."""

    def __init__(self, width, height):
        self.cols = (width - 2 * PAD) // CELL
        self.rows = min((height - 2 * PAD) // CELL, MAX_CELLS // self.cols)
        self.col = self.row = 0
        self.cells = [[0] * self.cols for _ in range(self.rows)]

    def newline(self):
        self.col = 0
        if self.row + 1 < self.rows:
            self.row += 1
        else:
            self.cells.pop(0)
            self.cells.append([0] * self.cols)

    def printable(self, b):
        if self.col >= self.cols:
            self.newline()
        self.cells[self.row][self.col] = b
        self.col += 1

    def feed(self, data):
        for b in data:
            if b == 0x0A:
                self.newline()
            elif b == 0x0D:
                self.col = 0
            elif b == 0x08:
                self.col = max(self.col - 1, 0)
            elif b == 0x09:
                target = (self.col // TAB + 1) * TAB
                while self.col < min(target, self.cols):
                    self.printable(0x20)
            elif 0x20 <= b <= 0x7E:
                self.printable(b)

    def cursor_cell(self):
        return min(self.col, self.cols - 1), self.row

    def expected_frame(self, width, height):
        """The whole screen as RGB bytes."""
        bg = bytes(BG)
        left = bg * PAD
        used = PAD + self.cols * CELL
        right = bg * (width - used)
        blank_row = bg * width
        cursor_col, cursor_row = self.cursor_cell()
        rows_out = [blank_row] * PAD
        for r in range(self.rows):
            pieces = [[] for _ in range(CELL)]
            for c in range(self.cols):
                g = cached_glyph(self.cells[r][c])
                if (c, r) == (cursor_col, cursor_row):
                    g = list(g)
                    for k in range(CELL - SCALE, CELL):
                        g[k] = bytes(FG) * CELL
                for k in range(CELL):
                    pieces[k].append(g[k])
            for k in range(CELL):
                rows_out.append(left + b"".join(pieces[k]) + right)
        while len(rows_out) < height:
            rows_out.append(blank_row)
        return b"".join(rows_out[:height])


def read_ppm(path):
    data = open(path, "rb").read()
    m = re.match(rb"P6\s+(\d+)\s+(\d+)\s+(\d+)\s", data)
    if not m or int(m.group(3)) != 255:
        fail("screenshot is not an 8-bit PPM")
    w, h = int(m.group(1)), int(m.group(2))
    pixels = data[m.end():]
    if len(pixels) != w * h * 3:
        fail("screenshot has %d pixel bytes, expected %d" % (len(pixels), w * h * 3))
    return w, h, pixels


def write_png(path, w, h, rgb):
    def chunk(tag, body):
        return struct.pack(">I", len(body)) + tag + body + struct.pack(">I", zlib.crc32(tag + body) & 0xFFFFFFFF)

    raw = b"".join(b"\x00" + rgb[y * w * 3:(y + 1) * w * 3] for y in range(h))
    png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(raw, 6)) + chunk(b"IEND", b"")
    open(path, "wb").write(png)


class Monitor:
    def __init__(self, path):
        self.sock = socket.socket(socket.AF_UNIX)
        for _ in range(100):
            try:
                self.sock.connect(path)
                break
            except OSError:
                time.sleep(0.1)
        else:
            fail("cannot connect to the QEMU monitor")
        self.sock.settimeout(10)
        self.read_until_prompt()

    def read_until_prompt(self):
        data = b""
        while not data.endswith(b"(qemu) "):
            chunk = self.sock.recv(4096)
            if not chunk:
                break
            data += chunk
        return data

    def command(self, text):
        self.sock.sendall(text.encode() + b"\n")
        return self.read_until_prompt()


def screenshot(mon, path):
    if os.path.exists(path):
        os.remove(path)
    mon.command("screendump " + path)
    last = -1
    for _ in range(100):
        time.sleep(0.2)
        if os.path.exists(path):
            size = os.path.getsize(path)
            if size > 0 and size == last:
                break
            last = size
    return read_ppm(path)


def kernel_output(serial_path):
    """Bytes the kernel sent to the console: everything from its first banner on
    (the bootloader's own serial messages before it never reach our screen)."""
    data = open(serial_path, "rb").read()
    start = data.find(b"Hello from NovaOS!")
    if start < 0:
        fail("the kernel banner never appeared on the serial port")
    return data[start:]


def compare(label, ppm_path, serial_path, outdir):
    w, h, actual = read_ppm(ppm_path)
    data = kernel_output(serial_path)
    term = Terminal(w, h)
    term.feed(data)

    reported = re.search(rb"\[ok\] screen: (\d+)x(\d+) pixels, (\d+)x(\d+) text", data)
    if not reported:
        fail("%s: the kernel never reported its screen size" % label)
    rw, rh, rc, rr = (int(x) for x in reported.groups())
    if (rw, rh) != (w, h) or (rc, rr) != (term.cols, term.rows):
        fail("%s: kernel says %dx%d (%dx%d text) but the screenshot is %dx%d (%dx%d expected)"
             % (label, rw, rh, rc, rr, w, h, term.cols, term.rows))

    expected = term.expected_frame(w, h)
    png_path = os.path.join(outdir, label + ".png")
    write_png(png_path, w, h, actual)
    if actual == expected:
        scrolled = data.count(b"\n") >= term.rows
        print("  %s: all %d pixels match (%dx%d, %dx%d text, %s) -> %s"
              % (label, w * h, w, h, term.cols, term.rows,
                 "scrolled" if scrolled else "not scrolled", png_path))
        return term

    bad = [i for i in range(w * h) if actual[i * 3:i * 3 + 3] != expected[i * 3:i * 3 + 3]]
    first = bad[0]
    x, y = first % w, first // w
    cx, cy = (x - PAD) // CELL, (y - PAD) // CELL
    write_png(os.path.join(outdir, label + "-expected.png"), w, h, expected)
    fail("%s: %d of %d pixels differ; first at (%d,%d) = cell (col %d, row %d); saw %s, expected %s; see %s"
         % (label, len(bad), w * h, x, y, cx, cy, tuple(actual[first * 3:first * 3 + 3]),
            tuple(expected[first * 3:first * 3 + 3]), png_path))


def wait_for(path, needle, seconds, quiet=1.0):
    """Wait until `needle` is in the file and nothing new has been written for `quiet` seconds."""
    deadline = time.time() + seconds
    while time.time() < deadline:
        if os.path.exists(path):
            data = open(path, "rb").read()
            if needle in data:
                time.sleep(quiet)
                if open(path, "rb").read() == data:
                    return data
        time.sleep(0.2)
    fail("timed out waiting for %r on the serial port" % needle)


