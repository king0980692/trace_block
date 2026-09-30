#!/usr/bin/env python3
"""Drive trace_block (or any TUI) in a pseudo-terminal and print what a user would see.

Runs COMMAND in a pty backed by a pyte terminal emulator, sends key sequences on a schedule and
prints screen snapshots. Useful to check the interactive browser without a real terminal.

    uv run --with pyte scripts/tui_drive.py \
        "trace_block browse --no-follow tests/data/pi-synthetic.jsonl" \
        '[[1.5, "g", "top"], [0, "jjl", "detail of the third cell"], [0, "?", "help"]]'

Each script step is [delay_seconds, keys_to_send, snapshot_label]; an empty label skips the
snapshot. COLS / ROWS environment variables set the terminal size (default 100x40).
"""

import fcntl
import json
import os
import pty
import select
import struct
import sys
import termios
import time

import pyte


def main() -> None:
    cols, rows = int(os.environ.get("COLS", 100)), int(os.environ.get("ROWS", 40))
    screen = pyte.Screen(cols, rows)
    stream = pyte.ByteStream(screen)
    cmd, script = sys.argv[1], json.loads(sys.argv[2])

    pid, fd = pty.fork()
    if pid == 0:
        os.execvp("bash", ["bash", "-c", cmd])
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

    def pump(seconds: float) -> None:
        end = time.time() + seconds
        while time.time() < end:
            ready, _, _ = select.select([fd], [], [], 0.05)
            if ready:
                try:
                    stream.feed(os.read(fd, 65536))
                except OSError:
                    return

    for delay, keys, label in script:
        pump(delay)
        if keys:
            os.write(fd, keys.encode())
        pump(0.4)
        if label:
            print(f"\n===== {label} " + "=" * 50)
            for line in screen.display:
                print(line.rstrip())

    os.write(fd, b"q")
    pump(0.5)


if __name__ == "__main__":
    main()
