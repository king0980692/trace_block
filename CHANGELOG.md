# Changelog

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
