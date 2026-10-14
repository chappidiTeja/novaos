#!/usr/bin/env python3
"""End-to-end test of the graphical desktop, through the real emulated devices.

It boots NovaOS headless, types "desktop" into the shell, and then uses the QEMU
monitor (mouse_move, mouse_button, sendkey, screendump) to do what a user does:
open the Start menu, start programs, drag a window, click in one, close one, use
the terminal window, and finally quit the desktop. After every step it compares
the WHOLE screenshot, pixel for pixel, with a picture this script builds on its
own from the desktop's documented layout (see user/wm/src/lib.rs). Nothing here
is taken from the program's own code, so a bug in the drawing cannot also be in
the expectation. The only region ignored is the taskbar clock, which changes
every second (it is checked separately: it must show a plausible time that
moves forward).

usage: desktop_test.py ISO DISK [OUTDIR]
       desktop_test.py --boot DESKTOP_ISO DISK [OUTDIR]   (the image that boots to the desktop)
"""
import os
import re
import shutil
import sys
import tempfile
import time

from gfx_test import Machine, boot_to_shell, type_text
from screenlib import FONT, fail, read_ppm, screenshot, wait_for, write_png

# ---- layout, written out again from the documentation -------------------------------------

BORDER, TITLE_H, CLOSE = 2, 28, 24
TASKBAR_H = 40
START = (4, 4, 84, 32)  # x, y offset from the taskbar top, w, h
TASK_X0, TASK_W, TASK_STEP, TASK_H = 96, 150, 154, 32
MENU_X, MENU_W, ITEM_H, MENU_EDGE_W = 4, 220, 40, 2
CASCADE0, CASCADE_STEP = (80, 60), 32
MENU_ITEMS = ["Terminal", "Graphics demo", "About NovaOS", "Exit desktop"]

C = dict(
    wallpaper=(30, 60, 114), logo=(60, 100, 160), taskbar=(24, 24, 32), line=(70, 70, 90),
    start=(45, 95, 185), start_hot=(70, 125, 220), btn=(50, 52, 66), btn_focus=(70, 110, 190),
    text=(240, 240, 240), border_focus=(70, 130, 230), border_idle=(80, 86, 100),
    title_focus=(50, 100, 200), title_idle=(62, 66, 78), close=(200, 60, 60), close_hot=(230, 90, 90),
    menu=(36, 40, 52), menu_hot=(50, 110, 200), menu_edge=(70, 76, 92),
)

# The mouse pointer: 'X' outline, '.' fill, ' ' transparent.
CURSOR = [
    "X           ", "XX          ", "X.X         ", "X..X        ", "X...X       ", "X....X      ",
    "X.....X     ", "X......X    ", "X.......X   ", "X........X  ", "X.........X ", "X......XXXXX",
    "X...X..X    ", "X..X X..X   ", "X.X  X..X   ", "XX    X..X  ", "X     X..X  ", "       X..X ",
    "       XXX  ",
]


class Frame:
    """An RGB picture as a list of rows, with the few drawing operations we need."""

    def __init__(self, w, h):
        self.w, self.h = w, h
        self.rows = [bytearray(w * 3) for _ in range(h)]

    def fill(self, x, y, w, h, color):
        x0, y0, x1, y1 = max(x, 0), max(y, 0), min(x + w, self.w), min(y + h, self.h)
        if x0 >= x1 or y0 >= y1:
            return
        strip = bytes(color) * (x1 - x0)
        for row in range(y0, y1):
            self.rows[row][x0 * 3:x1 * 3] = strip

    def put(self, x, y, color):
        if 0 <= x < self.w and 0 <= y < self.h:
            self.rows[y][x * 3:x * 3 + 3] = bytes(color)

    def char(self, x, y, ch, scale, color):
        glyph = FONT[ch & 0x7F]
        for gy in range(8):
            for gx in range(8):
                if glyph[gy] >> gx & 1:
                    self.fill(x + gx * scale, y + gy * scale, scale, scale, color)

    def text(self, x, y, s, scale, color):
        for i, byte in enumerate(s.encode()):
            ch = byte if 0x20 <= byte < 0x7F else ord("?")
            self.char(x + i * 8 * scale, y, ch, scale, color)

    def cursor(self, x, y):
        for r, line in enumerate(CURSOR):
            for c, shade in enumerate(line):
                if shade == "X":
                    self.put(x + c, y + r, (0, 0, 0))
                elif shade == ".":
                    self.put(x + c, y + r, (255, 255, 255))

    def blit(self, x, y, src):
        for r in range(src.h):
            if 0 <= y + r < self.h:
                x0, x1 = max(x, 0), min(x + src.w, self.w)
                if x0 < x1:
                    self.rows[y + r][x0 * 3:x1 * 3] = src.rows[r][(x0 - x) * 3:(x1 - x) * 3]

    def bytes(self):
        return b"".join(bytes(r) for r in self.rows)


# ---- what the programs draw inside their windows ---------------------------------------------

def about_pixels():
    f = Frame(360, 150)
    f.fill(0, 0, 360, 150, (24, 28, 40))
    f.text(16, 14, "NovaOS", 3, (120, 170, 255))
    f.text(16, 56, "An operating system", 2, (230, 230, 230))
    f.text(16, 80, "written from scratch", 2, (230, 230, 230))
    f.text(16, 104, "in Rust.", 2, (230, 230, 230))
    return f


def demo_pixels(pointer=(160, 100), clicked=False):
    f = Frame(320, 200)
    for y in range(200):
        row = bytearray()
        for x in range(320):
            row += bytes((x * 255 // 319, y * 255 // 199, 128))
        f.rows[y] = row
    f.text(16, 16, "NovaOS", 3, (255, 255, 255))
    f.text(16, 56, "Move the mouse here", 1, (255, 255, 255))
    color = (255, 80, 80) if clicked else (255, 230, 60)
    f.fill(pointer[0] - 12, pointer[1] - 12, 24, 24, color)
    return f


def terminal_pixels(output, cols=50, rows=30):
    """The terminal window's contents for the text the shell has printed."""
    cells = [[0] * cols for _ in range(rows)]
    col = row = 0

    def newline():
        nonlocal col, row
        col = 0
        if row + 1 < rows:
            row += 1
        else:
            cells.pop(0)
            cells.append([0] * cols)

    for b in output:
        if b == 10:
            newline()
        elif b == 13:
            col = 0
        elif b == 8:
            col = max(col - 1, 0)
        elif 0x20 <= b <= 0x7E:
            if col >= cols:
                newline()
            cells[row][col] = b
            col += 1
    f = Frame(cols * 16, rows * 16)
    bg, fg = (16, 24, 40), (220, 220, 220)
    f.fill(0, 0, f.w, f.h, bg)
    for r in range(rows):
        for c in range(cols):
            if cells[r][c]:
                f.char(c * 16, r * 16, cells[r][c], 2, fg)
    cc = min(col, cols - 1)
    f.fill(cc * 16, row * 16 + 14, 16, 2, fg)
    return f


# ---- the desktop model ---------------------------------------------------------------------------

class Window:
    def __init__(self, wid, title, pixels, pos):
        self.id, self.title, self.pixels = wid, title, pixels
        self.x, self.y = pos
        self.cw, self.ch = pixels.w, pixels.h

    def outer(self):
        return (self.x, self.y, self.cw + 2 * BORDER, self.ch + TITLE_H + 2 * BORDER)

    def title_bar(self):
        return (self.x + BORDER, self.y + BORDER, self.cw, TITLE_H)

    def client(self):
        return (self.x + BORDER, self.y + BORDER + TITLE_H, self.cw, self.ch)

    def close_button(self):
        tx, ty, tw, _ = self.title_bar()
        return (tx + tw - CLOSE - 2, ty + 2, CLOSE, CLOSE)


def inside(rect, x, y):
    return rect[0] <= x < rect[0] + rect[2] and rect[1] <= y < rect[1] + rect[3]


class Desktop:
    def __init__(self, w, h):
        self.w, self.h = w, h
        self.windows = []  # back to front
        self.focus = None
        self.menu = False
        self.pointer = ((w - 1) // 2, (h - 1) // 2)
        self.next_pos = CASCADE0
        self.next_id = 1

    def top(self):
        return self.h - TASKBAR_H

    def start_rect(self):
        return (START[0], self.top() + START[1], START[2], START[3])

    def menu_rect(self):
        return (MENU_X, self.top() - (len(MENU_ITEMS) * ITEM_H + 2 * MENU_EDGE_W), MENU_W,
                len(MENU_ITEMS) * ITEM_H + 2 * MENU_EDGE_W)

    def item_rect(self, i):
        m = self.menu_rect()
        return (m[0] + MENU_EDGE_W, m[1] + MENU_EDGE_W + i * ITEM_H, MENU_W - 2 * MENU_EDGE_W, ITEM_H)

    def task_rect(self, i):
        return (TASK_X0 + i * TASK_STEP, self.top() + 4, TASK_W, TASK_H)

    def add_window(self, title, pixels):
        x, y = self.next_pos
        if x + pixels.w + 2 * BORDER > self.w or y + pixels.h + TITLE_H + 2 * BORDER > self.h - TASKBAR_H:
            x, y = CASCADE0
        self.next_pos = (x + CASCADE_STEP, y + CASCADE_STEP)
        win = Window(self.next_id, title, pixels, (x, y))
        self.next_id += 1
        self.windows.append(win)
        self.focus = win.id
        return win

    def remove(self, win):
        self.windows.remove(win)
        self.focus = self.windows[-1].id if self.windows else None

    def raise_window(self, win):
        self.windows.remove(win)
        self.windows.append(win)
        self.focus = win.id

    def hit(self, x, y):
        if self.menu:
            for i in range(len(MENU_ITEMS)):
                if inside(self.item_rect(i), x, y):
                    return ("item", i)
            if inside(self.menu_rect(), x, y):
                return ("menu", None)
        if y >= self.top():
            if inside(self.start_rect(), x, y):
                return ("start", None)
            for i, win in enumerate(sorted(self.windows, key=lambda w: w.id)):
                if inside(self.task_rect(i), x, y):
                    return ("task", win)
            return ("taskbar", None)
        for win in reversed(self.windows):
            if inside(win.outer(), x, y):
                if inside(win.close_button(), x, y):
                    return ("close", win)
                if inside(win.title_bar(), x, y):
                    return ("title", win)
                if inside(win.client(), x, y):
                    return ("client", win)
                return ("border", win)
        return ("nothing", None)

    def render(self):
        w, h = self.w, self.h
        f = Frame(w, h)
        f.fill(0, 0, w, h, C["wallpaper"])
        f.text((w - 6 * 8 * 6) // 2, self.top() // 2 - 24, "NovaOS", 6, C["logo"])
        pointer_hit = self.hit(*self.pointer)
        for win in self.windows:
            focused = self.focus == win.id
            ox, oy, ow, oh = win.outer()
            f.fill(ox, oy, ow, oh, C["border_focus"] if focused else C["border_idle"])
            tx, ty, tw, th = win.title_bar()
            f.fill(tx, ty, tw, th, C["title_focus"] if focused else C["title_idle"])
            room = max(win.cw - 8 - CLOSE - 6, 0) // 16
            f.text(tx + 8, ty + 6, win.title[:room], 2, C["text"])
            cx, cy, cw, ch = win.close_button()
            hot = pointer_hit == ("close", win)
            f.fill(cx, cy, cw, ch, C["close_hot"] if hot else C["close"])
            f.char(cx + 4, cy + 4, ord("X"), 2, C["text"])
            clx, cly, _, _ = win.client()
            f.blit(clx, cly, win.pixels)
        top = self.top()
        f.fill(0, top, w, TASKBAR_H, C["taskbar"])
        f.fill(0, top, w, 1, C["line"])
        sx, sy, sw, sh = self.start_rect()
        hot = self.menu or pointer_hit == ("start", None)
        f.fill(sx, sy, sw, sh, C["start_hot"] if hot else C["start"])
        f.text(sx + 2, sy + 8, "Start", 2, C["text"])
        for i, win in enumerate(sorted(self.windows, key=lambda w: w.id)):
            bx, by, bw, bh = self.task_rect(i)
            f.fill(bx, by, bw, bh, C["btn_focus"] if self.focus == win.id else C["btn"])
            f.text(bx + 8, by + 8, win.title[:8], 2, C["text"])
        if self.menu:
            mx, my, mw, mh = self.menu_rect()
            f.fill(mx, my, mw, mh, C["menu_edge"])
            for i, label in enumerate(MENU_ITEMS):
                rx, ry, rw, rh = self.item_rect(i)
                f.fill(rx, ry, rw, rh, C["menu_hot"] if pointer_hit == ("item", i) else C["menu"])
                f.text(rx + 16, ry + 12, label, 2, C["text"])
        f.cursor(*self.pointer)
        return f

    def clock_rect(self):
        return (self.w - 88, self.top() + 12, 80, 16)


# ---- driving the machine ------------------------------------------------------------------------------

class Session:
    def __init__(self, machine, outdir):
        self.m = machine
        self.outdir = outdir
        self.desk = None
        self.shots = 0
        self.last_clock = None

    def pointer_to(self, x, y):
        """Move the real (emulated) mouse to (x, y) with relative moves, as a user would."""
        px, py = self.desk.pointer
        while (px, py) != (x, y):
            dx = max(-100, min(100, x - px))
            dy = max(-100, min(100, y - py))
            self.m.mon.command("mouse_move %d %d" % (dx, dy))
            px, py = px + dx, py + dy
            time.sleep(0.12)
        self.desk.pointer = (x, y)

    def button(self, state):
        self.m.mon.command("mouse_button %d" % state)
        time.sleep(0.25)

    def click(self, x, y):
        self.pointer_to(x, y)
        self.button(1)
        self.button(0)

    def check(self, label, retries=8):
        """The screen must equal the model (apart from the clock). Retries absorb
        the moment between an action and the desktop finishing its redraw."""
        expected_frame = self.desk.render()
        for attempt in range(retries):
            time.sleep(0.6)
            ppm = os.path.join(self.m.work, "d%d.ppm" % self.shots)
            screenshot(self.m.mon, ppm)
            w, h, actual = read_ppm(ppm)
            if (w, h) != (self.desk.w, self.desk.h):
                fail("%s: screenshot is %dx%d, expected %dx%d" % (label, w, h, self.desk.w, self.desk.h))
            expected = bytearray(expected_frame.bytes())
            cx, cy, cw, ch = self.desk.clock_rect()
            for y in range(cy, cy + ch):
                expected[(y * w + cx) * 3:(y * w + cx + cw) * 3] = actual[(y * w + cx) * 3:(y * w + cx + cw) * 3]
            if bytes(expected) == actual:
                self.shots += 1
                write_png(os.path.join(self.outdir, "desktop-%02d-%s.png" % (self.shots, label.replace(" ", "-"))), w, h, actual)
                self.last_clock = self.read_clock(actual, w)
                print("  ok: %s (all %d pixels, clock %s)" % (label, w * h, self.last_clock))
                return
        # Failure: say where.
        bad = next(i for i in range(w * h) if bytes(expected[i * 3:i * 3 + 3]) != actual[i * 3:i * 3 + 3])
        write_png(os.path.join(self.outdir, "FAIL-%s.png" % label.replace(" ", "-")), w, h, actual)
        write_png(os.path.join(self.outdir, "FAIL-%s-expected.png" % label.replace(" ", "-")), w, h, bytes(expected))
        count = sum(1 for i in range(w * h) if bytes(expected[i * 3:i * 3 + 3]) != actual[i * 3:i * 3 + 3])
        fail("%s: %d pixels differ, first at (%d,%d): saw %r, expected %r"
             % (label, count, bad % w, bad // w, tuple(actual[bad * 3:bad * 3 + 3]), tuple(expected[bad * 3:bad * 3 + 3])))

    def read_clock(self, actual, w):
        """Decode the clock's five characters by matching them against the font."""
        cx, cy, _, _ = self.desk.clock_rect()
        text = ""
        for i in range(5):
            bits = []
            for gy in range(8):
                value = 0
                for gx in range(8):
                    px = actual[((cy + gy * 2) * w + cx + i * 16 + gx * 2) * 3:((cy + gy * 2) * w + cx + i * 16 + gx * 2) * 3 + 3]
                    if tuple(px) == C["text"]:
                        value |= 1 << gx
                bits.append(value)
            match = [chr(c) for c in range(0x20, 0x7F) if FONT[c] == bits]
            text += match[0] if match else "?"
        if not re.fullmatch(r"\d\d:[0-5]\d", text):
            fail("the clock shows %r, which is not a time" % text)
        return text


def clock_seconds(text):
    return int(text[:2]) * 60 + int(text[3:])


def desktop_test(iso, disk, outdir):
    print("desktop test:")
    work = tempfile.mkdtemp()
    m = boot_to_shell(iso, disk, work)
    try:
        type_text(m.mon, "desktop\n")
        wait_for(m.serial, b"desktop: ready", 60)
        ready = re.search(rb"desktop: ready (\d+)x(\d+)", m.log())
        w, h = int(ready.group(1)), int(ready.group(2))
        s = Session(m, outdir)
        s.desk = Desktop(w, h)

        s.check("empty desktop")
        first_clock = s.last_clock

        # The Start menu.
        sx, sy, _, _ = s.desk.start_rect()
        s.pointer_to(sx + 20, sy + 10)
        s.check("pointer over Start")
        s.button(1)
        s.button(0)
        s.desk.menu = True
        s.check("menu open")
        item = s.desk.item_rect(0)
        s.pointer_to(item[0] + 30, item[1] + 20)
        s.check("menu hover on Terminal")

        # About NovaOS.
        item = s.desk.item_rect(2)
        s.pointer_to(item[0] + 30, item[1] + 20)
        s.button(1)
        s.button(0)
        s.desk.menu = False
        about = s.desk.add_window("About NovaOS", about_pixels())
        s.check("about window opened")

        # Graphics demo, in front of it.
        s.pointer_to(sx + 20, sy + 10)
        s.button(1)
        s.button(0)
        s.desk.menu = True
        item = s.desk.item_rect(1)
        s.pointer_to(item[0] + 30, item[1] + 20)
        s.button(1)
        s.button(0)
        s.desk.menu = False
        demo = s.desk.add_window("Graphics demo", demo_pixels())
        s.check("demo window opened in front")

        # Drag the demo window by its title bar.
        tb = demo.title_bar()
        grab = (tb[0] + 150, tb[1] + 10)
        s.pointer_to(*grab)
        s.button(1)
        s.pointer_to(grab[0] + 90, grab[1] + 60)
        s.button(0)
        demo.x += 90
        demo.y += 60
        s.check("demo window dragged")

        # The demo's square: hovering does nothing; a press makes it jump to the pointer and
        # turn red; while the button is held it follows; it stays where the button is released.
        cl = demo.client()
        s.pointer_to(cl[0] + 200, cl[1] + 120)
        s.check("hovering leaves the square alone")
        s.button(1)
        demo.pixels = demo_pixels(pointer=(200, 120), clicked=True)
        s.check("square jumps to the press and turns red")
        s.pointer_to(cl[0] + 80, cl[1] + 60)
        demo.pixels = demo_pixels(pointer=(80, 60), clicked=True)
        s.check("square follows the pointer while the button is held")
        s.button(0)
        demo.pixels = demo_pixels(pointer=(80, 60), clicked=False)
        s.check("square normal again where it was dropped")

        # Click on the other window's title bar: it comes to the front.
        tb = about.title_bar()
        s.click(tb[0] + 150, tb[1] + 10)
        s.desk.raise_window(about)
        s.check("about window raised")

        # Taskbar button: back to the demo.
        s.click(*[v + o for v, o in zip(s.desk.task_rect(1)[:2], (20, 10))])
        s.desk.raise_window(demo)
        s.check("taskbar button raises the demo")

        # Close the demo with its close button.
        cb = demo.close_button()
        s.click(cb[0] + 12, cb[1] + 12)
        s.desk.remove(demo)
        s.check("demo closed")

        # Terminal.
        s.pointer_to(sx + 20, sy + 10)
        s.button(1)
        s.button(0)
        s.desk.menu = True
        item = s.desk.item_rect(0)
        s.pointer_to(item[0] + 30, item[1] + 20)
        s.button(1)
        s.button(0)
        s.desk.menu = False
        banner = b"NovaOS shell. Type 'help' for commands.\nnova:/$ "
        term = s.desk.add_window("Terminal", terminal_pixels(banner))
        # The window the program opened must be where the model puts it; the pointer is on the
        # Terminal entry, which is now closed, so nothing hovers.
        s.check("terminal opened")

        type_text(m.mon, "echo hi\n")
        shown = banner + b"echo hi\nhi\nnova:/$ "
        term.pixels = terminal_pixels(shown)
        s.check("typed into the terminal")

        # Close the terminal (this ends the shell inside it) and the about window.
        cb = term.close_button()
        s.click(cb[0] + 12, cb[1] + 12)
        s.desk.remove(term)
        s.check("terminal closed")
        cb = about.close_button()
        s.click(cb[0] + 12, cb[1] + 12)
        s.desk.remove(about)
        s.check("all windows closed")

        # The clock moved forward while all this happened.
        elapsed = clock_seconds(s.last_clock) - clock_seconds(first_clock)
        if not 5 <= elapsed <= 600:
            fail("the clock went from %s to %s" % (first_clock, s.last_clock))
        print("  ok: the clock advanced %d s over the test" % elapsed)

        # Quit through the menu.
        s.pointer_to(sx + 20, sy + 10)
        s.button(1)
        s.button(0)
        s.desk.menu = True
        item = s.desk.item_rect(3)
        s.pointer_to(item[0] + 30, item[1] + 20)
        s.button(1)
        s.button(0)
        wait_for(m.serial, b"desktop: bye", 30)
        print("  the desktop said goodbye")
        # Back on the text console, drawn exactly as the kernel's console should be.
        wait_for(m.serial, b"desktop: bye\r\nnova:/$ ", 30)
        m.screenshot_compare("desktop-after-exit", outdir)
        type_text(m.mon, "echo back\n")
        wait_for(m.serial, b"back\r\nnova:/$ ", 30)
        m.screenshot_compare("desktop-typing-again", outdir)
        print("  the console and the keyboard are back")
    finally:
        m.stop()
        shutil.rmtree(work, ignore_errors=True)


def boot_into_desktop_test(iso, disk, outdir):
    """The desktop boot image must reach the desktop by itself: no shell, no typing."""
    print("boot straight into the desktop:")
    work = tempfile.mkdtemp()
    m = Machine(iso, disk, work)
    try:
        wait_for(m.serial, b"desktop: ready", 180)
        log = m.log()
        if b"started /bin/desktop" not in log:
            fail("the kernel did not report starting /bin/desktop")
        if b"NovaOS shell" in log or b"nova:/$" in log:
            fail("a shell was started although the desktop was asked for")
        ready = re.search(rb"desktop: ready (\d+)x(\d+)", log)
        s = Session(m, outdir)
        s.desk = Desktop(int(ready.group(1)), int(ready.group(2)))
        s.check("desktop straight from boot")
        # And it works: open the About window from the menu with the mouse.
        sx, sy, _, _ = s.desk.start_rect()
        s.click(sx + 20, sy + 10)
        s.desk.menu = True
        item = s.desk.item_rect(2)
        s.click(item[0] + 30, item[1] + 20)
        s.desk.menu = False
        s.desk.add_window("About NovaOS", about_pixels())
        s.check("about window from the boot desktop")
    finally:
        m.stop()
        shutil.rmtree(work, ignore_errors=True)


def main():
    args = sys.argv[1:]
    boot_mode = bool(args) and args[0] == "--boot"
    if boot_mode:
        args = args[1:]
    if len(args) < 2:
        print(__doc__)
        sys.exit(2)
    iso, disk = args[0], args[1]
    outdir = args[2] if len(args) > 2 else "build/desktop-screens"
    os.makedirs(outdir, exist_ok=True)
    if boot_mode:
        boot_into_desktop_test(iso, disk, outdir)
    else:
        desktop_test(iso, disk, outdir)
    print("DESKTOP TEST PASSED")


if __name__ == "__main__":
    main()
