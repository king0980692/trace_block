#!/usr/bin/env bash
# Record the README demo from a herdr pane: docs/demo.cast → docs/demo.gif (+ demo.mp4).
#
#   scripts/record_demo.sh <pane-id> [workdir]
#
# Needs: herdr (run inside it), agg (https://github.com/asciinema/agg), ffmpeg, a provider for pi.
# The pane should be a plain shell, ideally ~110x32. The workdir must contain the corpus the prompt
# searches (the demo uses the public-domain 三国演义 text in ./三國/).
set -euo pipefail
P=${1:?pane id, e.g. w1:p3}
WORK=${2:-/tmp/tbdemo}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=$ROOT/docs
STOP=$(mktemp -u)
PROMPT='曹腾的养子是谁？他的孙子又是谁？搜索 ./三國/ 来回答。'

t() { herdr pane send-text "$P" "$1" >/dev/null; sleep "${2:-1.2}"; }
k() { herdr pane send-keys "$P" "$1" >/dev/null; sleep "${2:-1.2}"; }

herdr pane run "$P" "cd $WORK && clear" >/dev/null; sleep 1
"$ROOT/scripts/herdr_cast.py" "$P" "$OUT/demo.cast" --stop "$STOP" --interval 0.1 &
sleep 1.5
t "pi --model nchc/gpt-oss-120b -t grep,find,ls --mode json -p \"$PROMPT\" | trace_block -o run.json" 2.5
k enter 1
herdr pane wait-output "$P" --source visible --match "✔ settled" --timeout 120000 >/dev/null; sleep 2.5
t g 2; t j 1; t j 1.6                           # session → tools → ⚙ toolsAdded
t l 2.2; t j 0.5; t j 0.5; t j 0.5; t j 1.5      # colored JSON detail
t h 1.5
t j 0.8; t j 0.8; t j 0.8; t j 1.2               # walk cells
t l 2.5; t h 1.5                                 # open one
t / 0.8; t "曹嵩" 1.2; k enter 2.2; t n 1.8; t n 1.8   # search
t a 3                                            # final answer
t b 1.8; t b 1.2                                 # backend cells off/on
t "?" 3.5; t j 1.2                               # key reference
t q 1.5
touch "$STOP"; wait

agg --font-size 14 --idle-time-limit 2.5 --last-frame-duration 3 \
    --text-font-family "DejaVu Sans Mono,WenQuanYi Micro Hei Mono" --emoji-font-family "Noto Color Emoji" \
    "$OUT/demo.cast" "$OUT/demo.gif"
ffmpeg -v error -y -i "$OUT/demo.gif" -movflags faststart -pix_fmt yuv420p \
    -vf "scale=trunc(iw/2)*2:trunc(ih/2)*2" -c:v libx264 -crf 22 "$ROOT/target/demo.mp4"
echo "wrote $OUT/demo.cast, $OUT/demo.gif, target/demo.mp4"
