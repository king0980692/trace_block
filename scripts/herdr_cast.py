#!/usr/bin/env python3
"""Record a herdr pane to an asciicast v2 file (for demo GIFs / videos).

Polls `herdr pane read PANE --source visible --format ansi` and writes one frame per change, until
STOP_FILE exists (or --max seconds pass). Render the cast with asciinema's `agg` (GIF) and
optionally ffmpeg (MP4):

    scripts/herdr_cast.py w1:p3 demo.cast --stop /tmp/stop-recording &
    ... drive the pane (herdr pane run / send-text / send-keys) ...
    touch /tmp/stop-recording; wait
    agg --font-size 14 demo.cast demo.gif

Must run inside herdr (HERDR_ENV=1) so the `herdr` CLI talks to the current session.
"""

import argparse
import json
import os
import subprocess
import time


def herdr(*args: str) -> str:
    return subprocess.run(["herdr", *args], capture_output=True, text=True, check=True).stdout


def pane_size(pane: str) -> tuple[int, int]:
    layout = json.loads(herdr("pane", "layout", "--pane", pane))["result"]["layout"]
    rect = next(p["rect"] for p in layout["panes"] if p["pane_id"] == pane)
    rows = json.loads(herdr("pane", "get", pane))["result"]["pane"]["scroll"]["viewport_rows"]
    return rect["width"], rows


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("pane")
    ap.add_argument("out")
    ap.add_argument("--stop", required=True, help="stop when this file exists")
    ap.add_argument("--interval", type=float, default=0.12, help="poll interval in seconds")
    ap.add_argument("--max", type=float, default=600, help="hard stop after N seconds")
    ap.add_argument("--title", default="trace_block")
    args = ap.parse_args()

    width, height = pane_size(args.pane)
    start = time.time()
    last = None
    frames = 0
    with open(args.out, "w") as f:
        header = {"version": 2, "width": width, "height": height, "timestamp": int(start),
                  "title": args.title, "env": {"TERM": "xterm-256color", "SHELL": "/bin/bash"}}
        f.write(json.dumps(header) + "\n")
        while not os.path.exists(args.stop) and time.time() - start < args.max:
            snap = herdr("pane", "read", args.pane, "--source", "visible", "--format", "ansi")
            if snap != last:
                rows = snap.rstrip("\n").split("\n")[:height]
                # home + clear, then the full screen: each event is one complete frame
                data = "\x1b[?25l\x1b[H\x1b[2J" + "\r\n".join(rows) + "\x1b[0m"
                f.write(json.dumps([round(time.time() - start, 3), "o", data]) + "\n")
                f.flush()
                last = snap
                frames += 1
            time.sleep(args.interval)
    print(f"{args.out}: {width}x{height}, {frames} frames, {time.time() - start:.1f}s")


if __name__ == "__main__":
    main()
