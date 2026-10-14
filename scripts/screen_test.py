#!/usr/bin/env python3
"""Screen test: boots NovaOS headless, takes real screenshots through the QEMU
monitor, and checks every pixel against what the text console should show.
(The shared machinery is in screenlib.py.)

The expected picture is built independently of the kernel: the text on screen is
worked out by replaying the serial log through a small terminal emulator, and
the glyph shapes come from the font crate's own source. Then it types "echo hi"
with the emulated PS/2 keyboard and checks the screen again, which exercises
keyboard -> shell -> screen end to end.

usage: screen_test.py ISO DISK [OUTDIR]
"""
import os
import shutil
import subprocess
import sys
import tempfile
import time

from screenlib import Monitor, compare, fail, screenshot, wait_for


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
