# trace_block

[![CI](https://github.com/king0980692/trace_block/actions/workflows/ci.yml/badge.svg)](https://github.com/king0980692/trace_block/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/king0980692/trace_block)](https://github.com/king0980692/trace_block/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**A transparent, live tracer for AI coding-agent JSON event streams.**

Pipe an agent's machine-readable event stream into `trace_block` and watch the run as it happens —
every prompt, thinking block, answer, tool call, tool result, retry and backend response — in a
full-screen terminal browser you can navigate cell by cell with vim keys. The raw stream passes
through untouched, so you can still save or post-process it.

Supported producers:

| Agent | Command |
|---|---|
| [pi coding agent](https://github.com/earendil-works/pi) | `pi --mode json -p "…" \| trace_block` |
| [Claude Code](https://docs.claude.com/en/docs/claude-code) | `claude -p "…" --output-format stream-json --verbose --include-partial-messages \| trace_block` |

The input format is detected from the event types; no flag is needed.

![trace_block demo: a live pi run in the browser — colored JSON detail, search, final answer, key help](docs/demo.gif)

<sub>Recorded from a real run (pi + gpt-oss-120b searching the public-domain 三国演义 text) in a
herdr pane with <code>scripts/record_demo.sh</code>. Also available as
<a href="https://github.com/king0980692/trace_block/releases/download/v0.1.0/demo.mp4">MP4</a> and as an
asciinema cast (<code>asciinema play docs/demo.cast</code>).</sub>

```
━━ pi session 01a0ec3a-d5e2-7754-9c5f-8cacdbc848ae
   cwd  /work
   model    nchc/gpt-oss-120b  openai-completions · thinking off

 TURN 2 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━  gpt-oss-120b  +1.2s
  💭 thinking
    ┊ Now grep within that file for the name.
  ⇄ LLM response · chatcmpl-1c4cb4ed-25b… · stop toolUse (tool_calls) · in 1013 / out 66 (reasoning 12)
▌ ┌  grep  ./docs/book.txt  pattern=name  context=2  limit=20
▌ │ No matches found
▌ └ ✓  grep  0.0s · 1 lines · 16 B
  ╰─ turn 2 · toolUse · in 1013 / out 66 tok · 0.6s · tools  grep  ×1

 j/k next/prev cell · l open · / search · n/N next/prev match · [ ] prev/next turn · ? all keys
 following new output                gpt-oss-120b  live · cell 9/23 · turn 2 · ● live
```

## Why

Agents in non-interactive mode print nothing useful while they work: the event stream goes to a
file or a pipe, and a 2-minute run looks frozen. Reading the raw NDJSON afterwards is painful
(hundreds of `message_update` deltas). `trace_block` turns that stream into readable blocks **while
it runs**, without hiding anything.

**Transparency is the design rule:** the view shows what is recorded in the stream — no
reconstructed values, no guessed metrics, no interpretive labels. When something is not in the
stream it says so (`stop (not in stream)`, `(empty — no thinking text recorded)`), and the one
piece of context that does not come from the stream (a provider's `baseUrl` from pi's
`models.json`) is labelled with its source. Every cell can be opened (`l`) to see the recorded JSON.

## Features

- **Interactive browser** (default on a terminal): one cell per block, grouped under `TURN` headers;
  `j`/`k` cell by cell, `l` opens the full content (complete tool args and output, full thinking,
  the system prompt, syntax-colored JSON), `/` search with `n`/`N`, `a` jumps to the final answer.
- **Live streaming**: thinking and answer text stream in as deltas arrive; running tools show a timer.
- **Everything recorded is visible**: model and provider, tool definitions sent to the model, each
  LLM response's backend metadata (response id, served-by model, raw stop reason, token usage incl.
  reasoning/cache tokens, cost, diagnostics), retries, permission denials, hooks, rate-limit events.
- **Final answer** highlighted (`★ FINAL ANSWER`, green turn header) based on the recorded stop reason.
- **Passthrough**: stdin → stdout byte-for-byte (never to a terminal); `-o FILE` saves the raw stream.
- **Trajectory export**: `--trajectory run.md` (readable Markdown of every message) or
  `run.jsonl` (the message lines, byte-exact).
- **Scroll view** (`--scroll`): append-only colored output for logs and non-TTY use.
- **Mermaid sequence diagram** of user ↔ agent ↔ tools (pi streams), rendered in-process with
  [mermaid-rs-renderer](https://github.com/1jehuang/mermaid-rs-renderer) — no browser, no Node:
  live `.mmd`/`.svg`/`.png` files, inline terminal images, or a live viewer pane (`trace_block view`)
  using kitty graphics, sixel, iTerm2 or a character-cell fallback.
- **Offline**: `trace_block browse FILE` reopens a saved run — including the session logs pi and
  Claude Code keep on disk; `trace_block replay FILE` re-streams it with realistic timing.

## Install

### Prebuilt binaries

Download the archive for your platform from the
[Releases page](https://github.com/king0980692/trace_block/releases), then:

```bash
tar xzf trace_block-v*-x86_64-unknown-linux-gnu.tar.gz
install trace_block-*/trace_block ~/.local/bin/        # or anywhere on your PATH
trace_block --version
```

Available targets: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
`x86_64-apple-darwin`, `aarch64-apple-darwin`. Each release has a `SHA256SUMS` file.

### With cargo

```bash
cargo install --git https://github.com/king0980692/trace_block --locked
```

### From source

```bash
git clone https://github.com/king0980692/trace_block
cd trace_block
cargo build --release          # → target/release/trace_block
```

Requires Rust ≥ 1.90 (edition 2024; some dependencies need 1.90). Linux and macOS (terminal handling uses POSIX termios);
Windows is not supported. Rendering SVG/PNG/inline images uses the system's installed fonts.

## Quick start

### pi

```bash
pi --mode json -p "Summarise the files in ./docs" | trace_block -o run.json
```

- Pick the model with `--model provider/id` (optionally `:high` for the thinking level) and the tools
  with `-t read,bash` / `-xt write` / `-nt` (no tools).
- `pi --mode json` writes events to stdout; trace_block renders them on stderr and saves the raw
  stream with `-o`. **Don't** add `| tee run.json` at the end: tee would print the raw JSON into the
  same terminal. Use `-o` (or `| tee run.json >/dev/null`).

### Claude Code

```bash
claude -p "Summarise the files in ./docs" \
  --output-format stream-json --verbose --include-partial-messages \
  | trace_block -o run.jsonl
```

- `--verbose` is required for `stream-json` in print mode.
- `--include-partial-messages` is recommended: without it you get whole blocks instead of live
  deltas, and per-message stop reasons are not in the stream.
- Useful flags for a clean, reproducible run: `--tools "Read Grep Glob"` (tools offered),
  `--allowedTools "…"` (tools allowed without a prompt — print mode cannot ask, so anything else is
  denied and shows up as a red `permission denied` cell), `--setting-sources ""` (ignore user/project
  settings, e.g. hooks that rewrite commands), `--strict-mcp-config` (no MCP servers),
  `--model`, `--effort low|medium|high|xhigh|max`.

### Later

```bash
trace_block browse run.json            # reopen in the browser (follows the file while it grows)
trace_block replay run.json | trace_block    # re-stream with realistic timing
cat run.json | trace_block -q --trajectory run.md >/dev/null   # export a trajectory afterwards
```

## Images

Images sent to a vision model appear in the view as one-line placeholders built from the recorded
block — `[image · image/png · 8.0 KB]` (media type and decoded size); the base64 data is never
printed, and `--trajectory FILE.md` notes it as omitted (`-o` and the JSONL trajectory keep it
byte-exact). This covers user messages and tool results (e.g. a `read`/`Read` of an image file).

Sending an image in pipe mode:

```bash
# pi: attach with @file (the model must accept images)
pi --model opencode/… --mode json -p @photo.png "What is in this picture?" | trace_block
# Claude Code: let it read the file …
claude -p "Look at ./photo.png — what is in it?" --tools Read --allowedTools Read \
  --output-format stream-json --verbose | trace_block
# … or send an image block directly (not echoed into the output stream, like text prompts)
claude -p --input-format stream-json --output-format stream-json --verbose < message.jsonl | trace_block
```

**pi and custom models:** pi only sends images to models declared as image-capable. A model in
`~/.pi/agent/models.json` without `"input": ["text", "image"]` gets the image replaced by the text
`(image omitted: model does not support images)` in the request — while the session log and the
event stream still record the image. The model then answers that it cannot see it. Declare the
input on the model entry:

```json
{ "id": "gemma-4-31B-it", "input": ["text", "image"] }
```

(Verified with a request-logging proxy; the event stream alone cannot show this, which is why the
view shows only what was recorded.)

## Session logs

Both agents also save every session to disk, and `trace_block` reads those files directly — in the
browser, the scroll view, `--trajectory` and `replay`:

| Agent | Where | Written by |
|---|---|---|
| pi | `~/.pi/agent/sessions/--<cwd>--/<timestamp>_<id>.jsonl` | interactive and `-p` runs (not `--no-session`) |
| Claude Code | `~/.claude/projects/<cwd>/<session-id>.jsonl` | interactive and `-p` runs |

```bash
trace_block browse ~/.pi/agent/sessions/--work--/2026-01-01T00-00-00-000Z_<id>.jsonl
trace_block browse ~/.claude/projects/-work/<session-id>.jsonl
trace_block browse "$(ls -t ~/.claude/projects/*/*.jsonl | head -1)"     # the newest Claude Code session
trace_block replay <session-log> | trace_block                            # re-play it with pacing
```

The format is detected automatically. A session log holds complete messages rather than a live
event stream, so the view shows what it records and nothing more:

- **pi session logs** — the system prompt and tool definitions, the prompt, each assistant message
  (one turn per LLM call: thinking, text, tool calls, backend metadata), tool results, model and
  thinking-level changes. No deltas and no tool timing are recorded, so none are shown.
- **Claude Code transcripts** — richer than `-p` streams in some ways: the **prompt** itself, the
  recorded **`stop_reason`** of every message (so the final answer is known), and the full **system
  prompt** (`attachment/prompt_snapshot`). Other entries (`attachment/*`, `queue-operation`,
  `last-prompt`, `ai-title`, `mode`, …) are shown as event cells with their raw JSON on `l`.
  Transcripts contain account metadata (e.g. `attachment/session_context`, `credential_org`): mind
  that before sharing a trajectory or a copy.
- Neither format has a completion marker; the end of the file is shown as `end of session log`.

## The interactive browser

Default when stderr is a terminal (`-i` forces it, `--scroll` disables it).

| Key | List | Detail view (after `l`) |
|---|---|---|
| `j` / `k` (↓ / ↑) | next / previous cell | scroll |
| `l` / Enter (→) | open the cell's full content | — |
| `h` / Esc (←) | — | back to the list |
| `J` / `K` | — | next / previous cell |
| `/` | search (smartcase, CJK input works) | search inside the cell |
| `n` / `N` | next / previous matching cell | next / previous matching line |
| `[` / `]` | previous / next turn | — |
| `g` / `G` | top / end (`G` follows new output) | top / bottom |
| `^d` / `^u` / space | jump 5 cells | page down / up |
| `a` | jump to the ★ final answer | — |
| `t` | hide / show thinking cells | — |
| `b` | hide / show `⇄ LLM response` cells | — |
| `f` | toggle follow mode | — |
| `?` | key reference | key reference |
| `q` | quit (a live stream keeps being consumed so the producer and `-o` finish) | back |
| `^c` | stop everything (like Ctrl+C in a normal terminal) | — |

The bottom rows show key hints (always as key + description; `?` lists the rest when the terminal
is narrow) and the position: model, source, `cell 12/40 · turn 5`, and `● live` / `✔ settled` /
`■ ended`. Opening a cell with an active search jumps to the first match inside it.

## Cells

| Cell | Content |
|---|---|
| `━━ pi session` / `━━ claude session` | session id, cwd, model; pi: provider `baseUrl` (labelled as from `~/.pi/agent/models.json`); Claude: version, permission mode |
| `· tools available` / `⚙ …` | the tools offered to the model; pi: full JSON schemas (`toolsAdded`), Claude: names from `system/init` |
| `▸ user` | the prompt (pi; Claude Code does not put the prompt in its stream) |
| `💭 thinking` | the model's thinking/reasoning as recorded |
| `● assistant` / `★ FINAL ANSWER` | answer text rendered as Markdown |
| `┌ tool` … `└ ✓/✗` | tool name badge, arguments, output preview, status, duration, size |
| `⇄ LLM response` | per LLM call: response id, served-by model, stop reason (+ raw provider reason), tokens, cost, effort, diagnostics; `l` = the recorded message JSON |
| `⚠ retry` / `✗ model error` | pi auto-retries and failed attempts |
| `✗ permission denied`, `· system/…`, `· rate_limit_event` | Claude Code system events (raw JSON on `l`) |
| summary | pi: turns, tool calls, retries, token totals; Claude: the recorded `result` (duration, turns, cost, usage, thinking tokens) |

## Output files

| Option | Writes |
|---|---|
| `-o FILE` | the raw input stream, byte-for-byte, flushed per line |
| `--trajectory FILE.md` | every recorded message as Markdown: system prompt sections + tool definitions, user, each assistant message (all recorded fields, thinking, text, tool calls), each tool result — including failed attempts |
| `--trajectory FILE.jsonl` | the message lines, byte-exact: pi → `session` + every `message_end` (session logs: every `message` entry); Claude Code → `system/init`, `assistant`, `user`, `result` |
| `--mmd` / `--svg` / `--png PATH` | the live Mermaid sequence diagram (pi streams), rewritten atomically as events arrive |

## Mermaid diagram and the live viewer

```bash
# pane A
pi --mode json -p "…" | trace_block --mmd /tmp/run.mmd
# pane B
trace_block view /tmp/run.mmd
```

`view` redraws the newest part of the diagram in place whenever the file changes. It asks the
terminal what it supports (works over ssh and inside multiplexers that answer the query): kitty
graphics (kitty, Ghostty, WezTerm), else sixel (e.g. Windows Terminal ≥ 1.22), else a character-cell
sequence diagram. Force one with `--proto kitty|sixel|iterm|text`; `trace_block view --probe` prints
what the terminal answered. `--inline` draws the diagram into the scroll view at the end of the run.

## Scroll view

`--scroll` (or any non-terminal stderr) prints append-only blocks: session banner, turn separators,
streaming thinking/answer text, tool blocks with a live timer, retry warnings, `⇄` backend lines and a
final summary. Colors and in-place rewrites only when stderr is a TTY; plain text otherwise.

## Troubleshooting

- **The screen fills with raw JSON** — something prints stdout to the terminal (usually `| tee FILE`).
  Use `-o FILE`.
- **Claude Code: `✗ permission denied · Bash · This command requires approval`** — print mode cannot
  ask for approval. Offer only the tools you want (`--tools "Read Grep Glob"`) or allow specific
  commands (`--allowedTools "Bash(ls *)"`). If a user hook rewrites commands (e.g. to a wrapper), the
  allow rules no longer match: run with `--setting-sources ""`.
- **Claude Code: `💭 thinking (empty — no thinking text recorded)`** — the request asked the API to
  redact thinking (the `redact-thinking-…` beta), so the blocks carry only a signature. Whether that
  beta is sent depends on the Claude Code mode and version: in a capture with 2.1.285 the interactive
  CLI sent it (thinking empty) and `claude -p` did not (thinking text present). The model did think
  either way: see the `system/thinking_tokens · estimated_tokens …` cells and `thinking_tokens` in the
  result summary.
- **pi: thinking appears although `thinking off`** — `thinking off` means pi does not request reasoning;
  models that always reason (e.g. gpt-oss) still return it, and pi records it.
- **No image in `view`** — your terminal (or multiplexer) does not pass kitty/sixel graphics; use
  `--proto text`.

## Development

See [DEVELOPMENT.md](DEVELOPMENT.md) for the architecture, the event schemas, how to test (unit,
CLI, terminal-emulator and multiplexer tests) and how releases are made.

## Credits

- Diagram rendering: [mermaid-rs-renderer](https://github.com/1jehuang/mermaid-rs-renderer) (MIT)
- SVG rasterisation: [resvg](https://github.com/linebender/resvg); sixel encoding: [icy_sixel](https://github.com/mkrueger/icy_sixel)

## License

[MIT](LICENSE)
