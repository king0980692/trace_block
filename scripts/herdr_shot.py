#!/usr/bin/env python3
"""Screenshot a herdr pane: its current (colored) screen → PNG, via a one-frame asciicast + agg.

    scripts/herdr_shot.py <pane-id> out.png [--font-size 14]

Needs herdr (run inside it), agg (https://github.com/asciinema/agg) and ffmpeg on PATH (or AGG=…).
Used to make the illustrations of the project site (site/img/).
"""

import argparse
import json
import os
import subprocess
import tempfile


def herdr(*args: str) -> str:
    return subprocess.run(["herdr", *args], capture_output=True, text=True, check=True).stdout


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("pane")
    ap.add_argument("out")
    ap.add_argument("--font-size", default="14")
    args = ap.parse_args()

    layout = json.loads(herdr("pane", "layout", "--pane", args.pane))["result"]["layout"]
    width = next(p["rect"]["width"] for p in layout["panes"] if p["pane_id"] == args.pane)
    height = json.loads(herdr("pane", "get", args.pane))["result"]["pane"]["scroll"]["viewport_rows"]
    snap = herdr("pane", "read", args.pane, "--source", "visible", "--format", "ansi")
    rows = snap.rstrip("\n").split("\n")[:height]
    frame = "\x1b[?25l\x1b[H\x1b[2J" + "\r\n".join(rows) + "\x1b[0m"

    with tempfile.TemporaryDirectory() as d:
        cast, gif = os.path.join(d, "s.cast"), os.path.join(d, "s.gif")
        with open(cast, "w") as f:
            f.write(json.dumps({"version": 2, "width": width, "height": height}) + "\n")
            f.write(json.dumps([0.0, "o", frame]) + "\n")
            f.write(json.dumps([0.5, "o", ""]) + "\n")
        subprocess.run(
            [os.environ.get("AGG", "agg"), "--font-size", args.font_size,
             "--text-font-family", "DejaVu Sans Mono,WenQuanYi Micro Hei Mono",
             "--emoji-font-family", "Noto Color Emoji", "--last-frame-duration", "1", cast, gif],
            check=True, capture_output=True,
        )
        subprocess.run(["ffmpeg", "-v", "error", "-y", "-i", gif, "-frames:v", "1", args.out], check=True)
    print(f"{args.out}: {width}x{height} cells")


if __name__ == "__main__":
    main()
