#!/usr/bin/env python3
"""Drive the real TUI through a pty and measure what it writes.

Exists because the flicker bug was invisible to unit tests: the rendering logic
was correct, but `draw` was called unconditionally on every poll tick and
`Terminal::resize` was called on every draw, which resets ratatui's buffers and
forces a full repaint. The only symptom was bytes on the wire.

    9.6 KB/s while idle  ->  flicker
    0 B/s while idle     ->  correct

Usage:  python3 scripts/tui-smoke.py [path-to-octane]
Exits non-zero if the TUI repaints while nothing is happening.
"""

import fcntl
import os
import pty
import re
import select
import struct
import sys
import termios
import time

# Anything above this while idle means something is repainting on a timer.
IDLE_BUDGET_BYTES = 500


def main() -> int:
    binary = sys.argv[1] if len(sys.argv) > 1 else "target/debug/octane"
    if not os.path.exists(binary):
        print(f"not found: {binary}  (cargo build -p octane-cli)")
        return 2

    pid, fd = pty.fork()
    if pid == 0:
        os.environ["TERM"] = "xterm-256color"
        os.environ["COLORTERM"] = "truecolor"
        os.environ["LANG"] = "en_US.UTF-8"
        # The sweep would otherwise dominate the startup byte count and make the
        # idle measurement below meaningless.
        os.environ["OCTANE_NO_ANIMATION"] = "1"
        os.execv(binary, [binary])

    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))

    captured = bytearray()
    alive = True

    def pump(seconds: float) -> int:
        nonlocal alive
        total = 0
        end = time.time() + seconds
        while time.time() < end:
            ready, _, _ = select.select([fd], [], [], 0.05)
            if not ready:
                continue
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                alive = False
                return total
            if not chunk:
                alive = False
                return total
            captured.extend(chunk)
            total += len(chunk)
            # A real terminal answers the Device Status Report that inline
            # viewport mode uses to find the cursor. Without this the TUI exits
            # with "cursor position could not be read".
            if b"\x1b[6n" in chunk:
                os.write(fd, b"\x1b[20;1R")
        return total

    def send(data: bytes) -> None:
        if alive:
            try:
                os.write(fd, data)
            except OSError:
                pass

    failures = []

    pump(3.0)  # startup and first paint
    idle = pump(4.0)
    print(f"idle output          {idle:>7} bytes / 4s")
    if idle > IDLE_BUDGET_BYTES:
        failures.append(f"repainting while idle ({idle} bytes, budget {IDLE_BUDGET_BYTES})")

    send(b"!echo tui-smoke-ok\r")
    pump(4.0)

    send(b"\x1b[Z")  # shift+tab
    pump(1.0)

    # Completion: `@` should fuzzy-match a real file, and Tab should accept it.
    send(b"@toolchai")
    pump(1.5)
    send(b"\t")
    pump(0.8)

    # Newline fallbacks, neither of which needs terminal support.
    send(b"\x1b\r")  # alt+enter
    pump(0.5)
    send(b"\x15")  # ctrl+u, clear
    pump(0.5)

    idle_after = pump(4.0)
    print(f"idle after activity  {idle_after:>7} bytes / 4s")
    if idle_after > IDLE_BUDGET_BYTES:
        failures.append(f"repainting after work settled ({idle_after} bytes)")

    send(b"\x03")
    pump(1.0)

    raw = captured.decode("utf-8", "replace")
    plain = re.sub(r"\x1b\[[0-9;?]*[a-zA-Z]", "", raw)

    # Colour lives in the escape sequences, so it has to be checked before they
    # are stripped.
    if "\x1b[?1049h" not in raw:
        failures.append("did not enter the alternate screen")

    if "38;2;149;214;0" in raw:
        print("ok    acid green #95D600")
    else:
        failures.append("brand colour missing from output")

    for label, needle in {
        "wordmark": "\u2588",
        "@ completion": "rust-toolchain.toml",
        "claw mark": "\u2571\u2571\u2571",
        "hints": "shift+tab",
        "shell output": "tui-smoke-ok",
        "mode cycled": "accept-edits",
        "status line": "ctx 0%",
    }.items():
        if needle in plain:
            print(f"ok    {label}")
        else:
            failures.append(f"missing {label}: {needle!r}")

    for failure in failures:
        print(f"FAIL  {failure}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
