#!/usr/bin/env python3
"""Tests the mouse, keyboard routing and the screen mapping against the real
emulated devices.

It boots NovaOS headless, types "gfxtest input" or "gfxtest draw" into the shell
on the emulated PS/2 keyboard, then drives QEMU's monitor:

  input: mouse_move / mouse_button / sendkey. The program prints every event it
         receives, and the test checks the exact coordinates, clamping at all
         four screen edges, buttons, that every key arrives as an event, and
         that none of them leaks to the shell.
  draw:  the program paints a known picture over the whole screen through the
         framebuffer it was given; the test compares all pixels with its own
         independent model.

Both finish by quitting the program and checking that the screen and the
keyboard really went back to the shell (pixel-exact, then typing works again).

usage: gfx_test.py ISO DISK [OUTDIR]
"""
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

from screenlib import Monitor, compare, fail, read_ppm, screenshot, wait_for, write_png

KEYS = {" ": "spc", "\n": "ret"}


def type_text(mon, text):
    for ch in text:
        mon.command("sendkey " + KEYS.get(ch, ch))
        time.sleep(0.12)


class Machine:
    def __init__(self, iso, disk, work):
        shutil.copy(disk, os.path.join(work, "disk.img"))
        self.serial = os.path.join(work, "serial.log")
        sock = os.path.join(work, "mon.sock")
        self.qemu = subprocess.Popen(
            ["qemu-system-x86_64", "-cdrom", iso, "-m", "512M", "-display", "none", "-no-reboot",
             "-drive", "file=%s,if=none,id=disk0,format=raw" % os.path.join(work, "disk.img"),
             "-device", "virtio-blk-pci,drive=disk0,disable-modern=on",
             "-serial", "file:" + self.serial, "-monitor", "unix:%s,server,nowait" % sock],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.mon = Monitor(sock)
        self.work = work

    def log(self):
        return open(self.serial, "rb").read()

    def stop(self):
        self.qemu.kill()
        self.qemu.wait()

    def events(self):
        """Every 'EV kind a b c' line the program has printed so far."""
        return [tuple(int(v) for v in m.groups())
                for m in re.finditer(rb"EV (\d+) (-?\d+) (-?\d+) (-?\d+)\r?\n", self.log())]

    def wait_event(self, count_before, seconds=15):
        """Wait until at least one new event line has arrived (and things went quiet)."""
        deadline = time.time() + seconds
        while time.time() < deadline:
            if len(self.events()) > count_before:
                time.sleep(0.3)
                return self.events()
            time.sleep(0.1)
        fail("no input event arrived within %d s" % seconds)

    def screenshot_compare(self, label, outdir):
        ppm = os.path.join(self.work, label + ".ppm")
        screenshot(self.mon, ppm)
        compare(label, ppm, self.serial, outdir)


def expect(what, got, wanted):
    if got != wanted:
        fail("%s: got %r, wanted %r" % (what, got, wanted))
    print("  ok: %s -> %r" % (what, got))


def boot_to_shell(iso, disk, work):
    m = Machine(iso, disk, work)
    wait_for(m.serial, b"nova:/$ ", 180)
    return m


def input_test(iso, disk, outdir):
    print("input test:")
    work = tempfile.mkdtemp()
    m = boot_to_shell(iso, disk, work)
    try:
        type_text(m.mon, "gfxtest input\n")
        wait_for(m.serial, b"input: ready", 60)
        ready = re.search(rb"input: ready (\d+)x(\d+)", m.log())
        w, h = int(ready.group(1)), int(ready.group(2))
        cx, cy = (w - 1) // 2, (h - 1) // 2
        max_x, max_y = w - 1, h - 1
        print("  screen is %dx%d" % (w, h))

        events = m.wait_event(0)
        expect("first event is the pointer's starting position (centre)", events[0], (1, cx, cy, 0))

        def move(dx, dy, label):
            before = len(m.events())
            m.mon.command("mouse_move %d %d" % (dx, dy))
            return m.wait_event(before)

        x, y = cx, cy
        events = move(10, 5, "right and down")
        x, y = x + 10, y + 5
        expect("move +10,+5", events[-1], (1, x, y, 0))
        events = move(-20, -30, "left and up")
        x, y = x - 20, y - 30
        expect("move -20,-30", events[-1], (1, x, y, 0))

        # Go to every edge, in steps QEMU can send as single packets.
        def go(dx, dy, steps):
            last = None
            for _ in range(steps):
                before = len(m.events())
                m.mon.command("mouse_move %d %d" % (dx, dy))
                time.sleep(0.15)
            time.sleep(0.4)
            return m.events()[-1]

        last = go(100, 0, 14)  # far past the right edge
        expect("clamped at the right edge", (last[1], last[2]), (max_x, y))
        last = go(0, 100, 10)  # past the bottom
        expect("clamped at the bottom edge", (last[1], last[2]), (max_x, max_y))
        last = go(-100, 0, 14)  # past the left edge
        expect("clamped at the left edge", (last[1], last[2]), (0, max_y))
        last = go(0, -100, 10)  # past the top
        expect("clamped at the top edge", (last[1], last[2]), (0, 0))
        x, y = 0, 0

        # Moving further into a wall produces no event at all.
        count = len(m.events())
        m.mon.command("mouse_move -50 -50")
        time.sleep(0.6)
        expect("pushing into the corner makes no new event", len(m.events()), count)

        # Away from the corner so buttons report a known position.
        events = move(40, 30, "off the corner")
        x, y = 40, 30
        expect("move to (40,30)", events[-1], (1, x, y, 0))

        def button(state, label):
            before = len(m.events())
            m.mon.command("mouse_button %d" % state)
            return m.wait_event(before)[-1]

        # QEMU's monitor takes the same bit values as the PS/2 packet itself
        # (its own documentation lists them in a different order, but the code uses
        # these): bit 0 left, bit 1 right, bit 2 middle. The kernel passes the
        # hardware bits through unchanged, so what goes in comes out.
        expect("left button down", button(1, "left"), (2, x, y, 1))
        expect("buttons up", button(0, "up"), (2, x, y, 0))
        expect("right button down", button(2, "right"), (2, x, y, 2))
        expect("buttons up again", button(0, "up"), (2, x, y, 0))
        expect("middle button down", button(4, "middle"), (2, x, y, 4))
        expect("buttons up once more", button(0, "up"), (2, x, y, 0))
        expect("two buttons at once", button(3, "left+right"), (2, x, y, 3))
        expect("one released, one still held", button(1, "left only"), (2, x, y, 1))
        expect("released", button(0, "up"), (2, x, y, 0))

        # Dragging: moves while a button is held report it.
        button(1, "left")
        events = move(3, 4, "drag")
        expect("move with the left button held", events[-1], (1, x + 3, y + 4, 1))
        button(0, "up")

        # Keys arrive as events with their ASCII codes.
        def key(name):
            before = len(m.events())
            m.mon.command("sendkey " + name)
            return m.wait_event(before)[-1]

        expect("key a", key("a"), (3, ord("a"), 0, 0))
        expect("key shift-a", key("shift-a"), (3, ord("A"), 0, 0))
        expect("key 7", key("7"), (3, ord("7"), 0, 0))
        expect("key space", key("spc"), (3, 32, 0, 0))
        expect("key enter", key("ret"), (3, 10, 0, 0))
        expect("key backspace", key("backspace"), (3, 8, 0, 0))
        expect("key escape", key("esc"), (3, 27, 0, 0))

        # None of that may have reached the shell: between the program's first and
        # last line, the only output is the program's own.
        log = m.log()
        between = log[log.index(b"input: ready"):]
        between = re.sub(rb"input: ready[^\r\n]*\r?\n", b"", between)
        between = re.sub(rb"EV [^\r\n]*\r?\n", b"", between)
        expect("nothing the shell echoed while the program owned the keyboard", between, b"")

        # Quit: the keyboard and screen must go back to the shell.
        m.mon.command("sendkey q")
        wait_for(m.serial, b"input: done", 30)
        wait_for(m.serial, b"input: done\r\nnova:/$ ", 30)
        print("  the program exited and the shell is back")
        m.screenshot_compare("input-after-exit", outdir)
        type_text(m.mon, "echo back\n")
        wait_for(m.serial, b"back\r\nnova:/$ ", 30)
        m.screenshot_compare("input-typing-again", outdir)
        print("  keyboard input goes to the shell again")
    finally:
        m.stop()
        shutil.rmtree(work, ignore_errors=True)


def expected_draw_frame(w, h, rgb_order=(0, 1, 2)):
    """What `gfxtest draw` paints, built here independently of that program."""
    palette = [bytes((220, 30, 30)), bytes((30, 220, 30)), bytes((30, 30, 220)), bytes((240, 240, 240))]
    white = bytes((255, 255, 255))
    rows = []
    for y in range(h):
        if y < 3 or y >= h - 3:
            rows.append(white * w)
            continue
        parts = [white * 3]
        k = 0
        while k * 64 < w:
            x0 = max(k * 64, 3)
            x1 = min((k + 1) * 64, w - 3)
            if x1 > x0:
                parts.append(palette[(k + (y >> 6)) % 4] * (x1 - x0))
            k += 1
        parts.append(white * 3)
        rows.append(b"".join(parts))
    return b"".join(rows)


def draw_test(iso, disk, outdir):
    print("draw test:")
    work = tempfile.mkdtemp()
    m = boot_to_shell(iso, disk, work)
    try:
        type_text(m.mon, "gfxtest draw\n")
        wait_for(m.serial, b"draw: done", 60)
        done = re.search(rb"draw: done (\d+)x(\d+)", m.log())
        want_w, want_h = int(done.group(1)), int(done.group(2))
        ppm = os.path.join(work, "draw.ppm")
        screenshot(m.mon, ppm)
        w, h, actual = read_ppm(ppm)
        expect("the screenshot has the size the program reported", (w, h), (want_w, want_h))
        expected = expected_draw_frame(w, h)
        write_png(os.path.join(outdir, "draw.png"), w, h, actual)
        if actual != expected:
            bad = next(i for i in range(w * h) if actual[i * 3:i * 3 + 3] != expected[i * 3:i * 3 + 3])
            write_png(os.path.join(outdir, "draw-expected.png"), w, h, expected)
            fail("draw: the picture differs, first at (%d,%d): saw %r, expected %r"
                 % (bad % w, bad // w, tuple(actual[bad * 3:bad * 3 + 3]), tuple(expected[bad * 3:bad * 3 + 3])))
        print("  ok: all %d pixels of the painted screen match (%dx%d)" % (w * h, w, h))

        m.mon.command("sendkey q")
        wait_for(m.serial, b"nova:/$ ", 30, quiet=1.5)
        wait_for(m.serial, b"draw: done %dx%d\r\nnova:/$ " % (w, h), 30)
        m.screenshot_compare("draw-after-exit", outdir)
        print("  the text console repainted itself exactly after the program quit")
    finally:
        m.stop()
        shutil.rmtree(work, ignore_errors=True)


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        sys.exit(2)
    iso, disk = sys.argv[1], sys.argv[2]
    outdir = sys.argv[3] if len(sys.argv) > 3 else "build/gfx-screens"
    os.makedirs(outdir, exist_ok=True)
    input_test(iso, disk, outdir)
    draw_test(iso, disk, outdir)
    print("GFX TEST PASSED")


if __name__ == "__main__":
    main()
