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
        # This test explicitly verifies the true-colour tier. Do not let the
        # invoking shell's accessibility preference silently switch tiers.
        os.environ.pop("NO_COLOR", None)
        # Deliberately NOT disabling motion here. This script is the only
        # thing that measures whether a transient effect settles, and a test
        # that switches off the feature it polices proves nothing. The sweep it
        # was written for no longer exists.
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
    # ctrl+u kills to the start of the line (readline), so this clears the
    # single-line draft the steps above typed. It stopped being a whole-buffer
    # clear when the editing chords landed; the comment said otherwise for a
    # while, which is how a smoke step quietly stops testing what it names.
    send(b"\x15")  # ctrl+u, kill to line start
    pump(0.5)

    # Motion must SETTLE, not merely be absent. A state change should produce
    # bytes and then stop; an effect that never stops passes an idle-only check
    # if the idle window happens to start after it, and an effect that never
    # ran passes it always.
    send(b"\x1b[Z")  # shift+tab: a state change with a visible consequence
    during = pump(0.4)
    after = pump(2.5)
    print(f"motion during change {during:>7} bytes / 0.4s")
    print(f"motion after settle  {after:>7} bytes / 2.5s")
    if during == 0:
        failures.append("a state change produced no output at all")
    if after > IDLE_BUDGET_BYTES:
        failures.append(f"motion did not settle ({after} bytes after 2.5s)")

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

    # The industrial redesign uses acid as both foreground type and a full
    # signal-strip background; either escape proves the exact brand value made
    # it to the terminal.
    if any(sequence in raw for sequence in ("38;2;184;245;0", "48;2;184;245;0")):
        print("ok    acid green #B8F500")
    else:
        failures.append("brand colour missing from output")

    for label, needle in {
        "brand mark": "\u2588\u2588\u2588\u2588\u2588\u2588 \u2588\u2588\u2588\u2588\u2588 \u2588\u2588\u2588\u2588\u2588\u2588\u2588\u2588",
        "@ completion": "rust-toolchain.toml",
        "hints": "SHIFT+TAB",
        "shell output": "tui-smoke-ok",
        "mode cycled": "ACCEPT-EDITS",
        "status line": "CTX/100% LEFT",
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
