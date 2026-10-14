#!/usr/bin/env python3
"""Screen test: boots NovaOS headless, takes real screenshots through the QEMU
monitor, and checks every pixel against what the text console should show.

The expected picture is built here from scratch, independently of the kernel:
  * the text on screen is worked out by replaying the serial log through a small
    terminal emulator (the screen shows the same bytes the serial port gets);
  * glyph shapes come from the font crate's own source file, not from kernel code;
  * the layout (cell size, margin, cursor) is written out again below.
A bug in the kernel's drawing code therefore cannot also hide in this test.

Then it types "echo hi" with the emulated PS/2 keyboard and checks the screen
again, which exercises keyboard -> shell -> screen end to end.

usage: screen_test.py ISO DISK [OUTDIR]
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


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        sys.exit(2)
    iso, disk = sys.argv[1], sys.argv[2]
    outdir = sys.argv[3] if len(sys.argv) > 3 else "build/screens"
    os.makedirs(outdir, exist_ok=True)
    work = tempfile.mkdtemp()
    qemu = None
    try:
        shutil.copy(disk, os.path.join(work, "disk.img"))
        serial = os.path.join(work, "serial.log")
        sock = os.path.join(work, "mon.sock")
        qemu = subprocess.Popen(
            ["qemu-system-x86_64", "-cdrom", iso, "-m", "512M", "-display", "none", "-no-reboot",
             "-drive", "file=%s,if=none,id=disk0,format=raw" % os.path.join(work, "disk.img"),
             "-device", "virtio-blk-pci,drive=disk0,disable-modern=on",
             "-serial", "file:" + serial, "-monitor", "unix:%s,server,nowait" % sock],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        mon = Monitor(sock)

        print("boot:")
        wait_for(serial, b"nova:/$ ", 180)
        boot_ppm = os.path.join(work, "boot.ppm")
        screenshot(mon, boot_ppm)
        compare("boot", boot_ppm, serial, outdir)

        print("typing 'echo hi' on the emulated keyboard:")
        before = len(open(serial, "rb").read())
        for key in ["e", "c", "h", "o", "spc", "h", "i", "ret"]:
            mon.command("sendkey " + key)
            time.sleep(0.15)
        data = wait_for(serial, b"\r\nhi\r\n", 60)
        wait_for(serial, b"hi\r\nnova:/$ ", 30)
        typed = open(serial, "rb").read()[before:]
        if b"echo hi" not in typed:
            fail("the keystrokes did not reach the shell; it printed %r" % typed[:80])
        typed_ppm = os.path.join(work, "typed.ppm")
        screenshot(mon, typed_ppm)
        term = compare("typed", typed_ppm, serial, outdir)
        text = ["".join(chr(c) if c else " " for c in row).rstrip() for row in term.cells]
        if not any(line.endswith("nova:/$ echo hi") for line in text) or "hi" not in text:
            fail("the expected lines are not on the screen; screen text ends with %r" % text[-6:])
        print("  the screen shows 'nova:/$ echo hi' followed by 'hi'")
        print("SCREEN TEST PASSED")
    finally:
        if qemu:
            qemu.kill()
            qemu.wait()
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
