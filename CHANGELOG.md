# Changelog

## Unreleased

- Read the session logs pi (`~/.pi/agent/sessions`) and Claude Code (`~/.claude/projects`) keep on
  disk: browse, scroll view, `--trajectory` and `replay`. Claude transcripts add the prompt, recorded
  stop reasons and the system prompt (`attachment/prompt_snapshot`).
- No wall-clock timings for session logs; `end of session log` instead of "unfinished".
- Recorded images are drawn when the terminal supports graphics (`--images auto|kitty|sixel|iterm|off`,
  auto-detected): in the browser's detail view and as thumbnails in the scroll view.
- Image content blocks (pi and Claude Code; user messages and tool results) are shown as
  `[image · <media type> · <size>]` placeholders instead of being dropped or dumped as base64;
  tool summaries count images separately (`1 lines · 27 B · 1 image`).
- Docs: sending images in pipe mode, and pi's `models.json` `input` requirement for custom models.
- Docs: thinking redaction in Claude Code depends on the mode/version (`redact-thinking` beta).

## v0.1.0

First release.

- Interactive full-screen browser (default on a terminal): cells grouped by turn, `j`/`k`/`l`/`h`
  navigation, detail view with syntax-colored JSON, `/` search with `n`/`N` (smartcase, CJK input),
  `a` final answer, `t` thinking and `b` backend-cell toggles, `[`/`]` turn jumps, `?` key reference,
  live follow mode; `q` during a live run keeps draining the stream.
- Input formats: pi `--mode json` and Claude Code `--output-format stream-json` (auto-detected).
- Transparent views: model/provider, tool definitions, per-response backend metadata (response id,
  served-by model, raw stop reason, usage incl. reasoning/cache tokens, cost, diagnostics), retries,
  permission denials, hooks, rate-limit and thinking-token events — only what the stream records.
- Byte-exact stdin→stdout passthrough (suppressed when stdout is a terminal), `-o` raw save.
- `--trajectory FILE.md|FILE.jsonl` trajectory export.
- `--scroll` append-only colored view; plain output when stderr is not a TTY.
- Live Mermaid sequence diagram (`--mmd/--svg/--png`, `--inline`) rendered with mermaid-rs-renderer,
  and `trace_block view` live viewer (kitty / sixel / iTerm2 / character-cell, terminal probing).
- `trace_block browse FILE` and `trace_block replay FILE`.
