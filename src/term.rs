//! Live block-by-block stderr renderer for pi `--mode json` event streams.

use crate::util::{args_summary, human_bytes, human_dur, one_line, result_text, str_width, trunc, wrap};
use serde_json::Value;
use std::collections::HashMap;
use std::io::Write;
use std::time::Instant;

const PREVIEW_LINES: usize = 8;

#[derive(Clone, Copy, PartialEq)]
enum StreamKind {
    Thinking,
    Text,
}

/// The block that currently owns the unterminated last line on the terminal.
enum Live {
    Stream {
        kind: StreamKind,
        text: String,
        committed: usize,
    },
    Tool {
        id: String,
    },
}

struct ToolRun {
    name: String,
    args: String,
    started: Instant,
    partial_bytes: usize,
}

#[derive(Default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub total: u64,
}

impl Usage {
    pub fn add(&mut self, u: &Value) {
        let g = |k: &str| u.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
        self.input += g("input");
        self.output += g("output");
        self.cache_read += g("cacheRead");
        self.total += g("totalTokens");
    }
}

pub struct Term {
    tty: bool,
    out: Box<dyn Write>,
    live: Option<Live>,
    tools: HashMap<String, ToolRun>,
    started: Instant,
    turn_started: Instant,
    turn: usize,
    /// provider/model of the last assistant message
    model: Option<(String, String, String)>,
    /// current turn streamed answer text
    turn_had_text: bool,
    endpoints: HashMap<String, String>,
    /// tools used in the current turn, in call order
    turn_tools: Vec<String>,
    /// per-tool (calls, failures), in first-use order
    tool_stats: Vec<(String, usize, usize)>,
    tool_calls: usize,
    tool_errors: usize,
    retries: usize,
    usage: Usage,
    settled: bool,
    /// tool result preview lines (0 = hide results)
    pub preview: usize,
    pub show_thinking: bool,
    /// count of emitted chunks, to tell whether a tool block is still contiguous
    seq: u64,
    /// last emitted output ended in a blank line (or nothing printed yet)
    at_gap: bool,
    tool_seq: HashMap<String, u64>,
}

impl Term {
    pub fn new(tty: bool, out: Box<dyn Write>) -> Self {
        Term {
            tty,
            out,
            live: None,
            tools: HashMap::new(),
            started: Instant::now(),
            turn_started: Instant::now(),
            turn: 0,
            model: None,
            turn_had_text: false,
            endpoints: crate::cells::load_endpoints(),
            turn_tools: Vec::new(),
            tool_stats: Vec::new(),
            tool_calls: 0,
            tool_errors: 0,
            retries: 0,
            usage: Usage::default(),
            settled: false,
            preview: PREVIEW_LINES,
            show_thinking: true,
            seq: 0,
            at_gap: true,
            tool_seq: HashMap::new(),
        }
    }

    fn width(&self) -> usize {
        if let Some((w, _)) = terminal_size::terminal_size_of(std::io::stderr()) {
            return (w.0 as usize).max(20);
        }
        std::env::var("COLUMNS")
            .ok()
            .and_then(|c| c.parse().ok())
            .unwrap_or(100)
    }

    fn paint(&self, code: &str, s: &str) -> String {
        if self.tty && !s.is_empty() {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    fn badge(&self, name: &str) -> String {
        crate::util::badge(name, self.tty)
    }

    fn tool_list(&self, stats: &[(String, usize, usize)]) -> String {
        crate::util::tool_list(stats, self.tty)
    }

    fn emit(&mut self, s: &str) {
        self.seq += 1;
        if !s.is_empty() {
            self.at_gap = s.ends_with("\n\n");
        }
        let _ = self.out.write_all(s.as_bytes());
        let _ = self.out.flush();
    }

    /// Terminate whatever live block owns the cursor so foreign output can follow.
    fn seal(&mut self) {
        match self.live.take() {
            Some(Live::Stream { kind, text, committed }) if !self.tty => {
                // plain mode buffers the partial last line; flush it now
                let lines = self.layout(kind, &text, self.width() - 4);
                let mut s = String::new();
                for l in &lines[committed..] {
                    if !l.is_empty() {
                        s += &self.stream_line(kind, l);
                        s += "\n";
                    }
                }
                self.emit(&s);
            }
            Some(_) if self.tty => self.emit("\n"),
            _ => {}
        }
    }

    /// Blank line between blocks (never doubled).
    fn gap(&mut self) {
        self.seal();
        if !self.at_gap {
            self.emit("\n");
            self.at_gap = true;
        }
    }

    pub fn warn(&mut self, msg: &str) {
        let s = self.paint("33", &format!("⚠ {msg}"));
        self.line(&s);
    }

    /// Print complete lines (each gets a newline), after sealing any live block.
    fn line(&mut self, s: &str) {
        self.seal();
        let mut s = s.to_string();
        s.push('\n');
        self.emit(&s);
    }

    /// Display lines (without the block prefix) for a stream's full text.
    fn layout(&self, kind: StreamKind, text: &str, width: usize) -> Vec<String> {
        if kind == StreamKind::Text && self.tty {
            crate::md::render(text, width)
        } else {
            wrap(text, width)
        }
    }

    fn stream_line(&self, kind: StreamKind, l: &str) -> String {
        match kind {
            StreamKind::Thinking => format!("{}{}", self.paint("2", "  ┊ "), self.paint("2;3", l)),
            StreamKind::Text => format!("  {l}"),
        }
    }

    fn open_stream(&mut self, kind: StreamKind, cont: bool) {
        let label = match (kind, cont) {
            (StreamKind::Thinking, false) => self.paint("2;35", "💭 thinking"),
            (StreamKind::Thinking, true) => self.paint("2;35", "💭 thinking (cont.)"),
            (StreamKind::Text, false) => self.paint("1;32", "● assistant"),
            (StreamKind::Text, true) => self.paint("1;32", "● assistant (cont.)"),
        };
        self.gap();
        self.line(&label);
        self.live = Some(Live::Stream {
            kind,
            text: String::new(),
            committed: 0,
        });
    }

    fn push_delta(&mut self, kind: StreamKind, delta: &str) {
        let is_ours = matches!(&self.live, Some(Live::Stream { kind: k, .. }) if *k == kind);
        if !is_ours {
            self.open_stream(kind, true);
        }
        let width = self.width() - 4;
        let tty = self.tty;
        let text = {
            let Some(Live::Stream { text, .. }) = &mut self.live else {
                return;
            };
            text.push_str(delta);
            text.clone()
        };
        let lines = self.layout(kind, &text, width);
        let from = {
            let Some(Live::Stream { committed, .. }) = &mut self.live else {
                return;
            };
            let from = (*committed).min(lines.len() - 1);
            *committed = lines.len() - 1;
            from
        };
        let mut s = String::new();
        if tty {
            // rewrite only the unterminated tail line this block owns, then any new lines
            s += "\r\x1b[K";
            for (i, l) in lines[from..].iter().enumerate() {
                if i > 0 {
                    s += "\n";
                }
                s += &self.stream_line(kind, l);
            }
        } else {
            for l in &lines[from..lines.len() - 1] {
                s += &self.stream_line(kind, l);
                s += "\n";
            }
        }
        self.emit(&s);
    }

    fn close_stream(&mut self, kind: StreamKind) {
        if matches!(&self.live, Some(Live::Stream { kind: k, .. }) if *k == kind) {
            self.seal();
        }
    }

    fn tool_status(&self, id: &str) -> String {
        let Some(t) = self.tools.get(id) else {
            return String::new();
        };
        let mut s = format!("│ ⏳ running {}", human_dur(t.started.elapsed().as_secs_f64()));
        if t.partial_bytes > 0 {
            s += &format!(" · {}", human_bytes(t.partial_bytes));
        }
        self.paint("33", &s)
    }

    /// Periodic refresh (elapsed time on a running tool).
    pub fn tick(&mut self) {
        if let Some(Live::Tool { id }) = &self.live {
            let s = format!("\r\x1b[K{}", self.tool_status(&id.clone()));
            self.emit(&s);
        }
    }

    /// Emit pre-formatted output (e.g. an inline image) after sealing any live block.
    pub fn raw(&mut self, s: &str) {
        self.seal();
        self.emit(s);
    }

    pub fn malformed(&mut self, lineno: usize, err: &str) {
        let s = self.paint("33", &format!("⚠ line {lineno}: malformed JSON passed through ({err})"));
        self.line(&s);
    }

    pub fn event(&mut self, ev: &Value) {
        let ty = ev.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ty {
            "session" => {
                let g = |k: &str| ev.get(k).and_then(|v| v.as_str()).unwrap_or("?").to_string();
                let head = format!("━━ pi session {} ", g("id"));
                let w = self.width();
                let bar = "━".repeat(w.saturating_sub(str_width(&head)).min(200));
                let s = self.paint("1;36", &format!("{head}{bar}"));
                self.line(&s);
                let s = self.paint("36", &format!("   cwd {}  ·  {}", g("cwd"), g("timestamp")));
                self.line(&s);
            }
            "turn_start" => {
                self.turn += 1;
                self.turn_started = Instant::now();
                // two blank lines between turns (one after the very first header)
                self.gap();
                if self.turn > 1 {
                    self.emit("\n");
                }
                let w = self.width();
                let label = format!(" TURN {} ", self.turn);
                let clock = format!(" +{} ", human_dur(self.started.elapsed().as_secs_f64()));
                let mo = self.model.as_ref().map(|m| m.1.clone()).filter(|m| !m.is_empty());
                let model_w = mo.as_ref().map(|m| str_width(m) + 3).unwrap_or(0);
                let fill = w
                    .saturating_sub(str_width(&label) + model_w + str_width(&clock) + 1)
                    .min(200);
                let model_part = mo
                    .map(|m| format!(" {}", crate::util::model_badge(&m, self.tty)))
                    .unwrap_or_default();
                let s = if self.tty {
                    format!(
                        "\x1b[1;30;46m{label}\x1b[0m\x1b[36m {}\x1b[0m{model_part}\x1b[2m{clock}\x1b[0m",
                        "━".repeat(fill)
                    )
                } else {
                    format!("=={label}{}{model_part}{clock}", "=".repeat(fill.saturating_sub(2)))
                };
                self.line(&s);
            }
            "turn_end" => {
                let m = &ev["message"];
                let stop = m.get("stopReason").and_then(|v| v.as_str()).unwrap_or("?");
                let u = &m["usage"];
                let final_turn = !matches!(stop, "toolUse" | "error" | "aborted" | "?") && self.turn_had_text;
                self.turn_had_text = false;
                if final_turn {
                    // the answer has already streamed above; mark it now that the turn is known to be last
                    self.gap();
                    let w = self.width();
                    let label = format!(" ★ FINAL ANSWER ↑ turn {} ", self.turn);
                    let s = if self.tty {
                        format!(
                            "\x1b[1;30;42m{label}\x1b[0m\x1b[32m {}\x1b[0m",
                            "━".repeat(w.saturating_sub(str_width(&label) + 1).min(200))
                        )
                    } else {
                        format!("=={label}==")
                    };
                    self.line(&s);
                }
                let mut s = format!("╰─ turn {} · {stop}", self.turn);
                if let (Some(i), Some(o)) = (u["input"].as_u64(), u["output"].as_u64())
                    && i + o > 0
                {
                    s += &format!(" · in {i} / out {o} tok");
                }
                s += &format!(" · {}", human_dur(self.turn_started.elapsed().as_secs_f64()));
                self.gap();
                let mut s = self.paint("2;36", &s);
                if !self.turn_tools.is_empty() {
                    let mut stats: Vec<(String, usize, usize)> = Vec::new();
                    for t in &self.turn_tools {
                        match stats.iter_mut().find(|x| &x.0 == t) {
                            Some(x) => x.1 += 1,
                            None => stats.push((t.clone(), 1, 0)),
                        }
                    }
                    s += &format!("{} {}", self.paint("2;36", " · tools"), self.tool_list(&stats));
                }
                self.turn_tools.clear();
                self.line(&s);
            }
            "message_start" => {
                let m = &ev["message"];
                if m["role"] == "assistant" {
                    let g = |k: &str| m[k].as_str().unwrap_or("").to_string();
                    let info = (g("provider"), g("model"), g("api"));
                    if !info.1.is_empty() && self.model.is_none() {
                        let verb = "model";
                        let mut details = info.2.clone();
                        if let Some(url) = self.endpoints.get(&info.0) {
                            details += &format!(" · baseUrl {url} (from ~/.pi/agent/models.json)");
                        }
                        let id = format!("{}/{}", info.0, info.1);
                        let s = format!(
                            "   {} {} {}",
                            self.paint("1;35", verb),
                            crate::util::model_badge(&id, self.tty),
                            self.paint("35", &details)
                        );
                        self.line(&s);
                        self.model = Some(info);
                    }
                }
                if m["role"] == "system" {
                    let tools = crate::util::offered_tools(m);
                    let badges = tools.iter().map(|t| self.badge(t)).collect::<Vec<_>>().join(" ");
                    let s = format!("{} {badges}", self.paint("2", "   tools available:"));
                    self.line(&s);
                    // one line per tool schema pi disclosed to the model
                    let w = self.width();
                    if let Some(defs) = m["toolsAdded"].as_array() {
                        for d in defs {
                            let (name, params, desc) = crate::cells::tool_signature(d);
                            let used = 5 + str_width(&name) + 2;
                            let sig = trunc(&format!("({params})"), w.saturating_sub(used + 2));
                            let room = w.saturating_sub(used + str_width(&sig) + 4);
                            let desc = if room > 8 {
                                format!(" {}", self.paint("2", &trunc(&format!("— {desc}"), room)))
                            } else {
                                String::new()
                            };
                            let s = format!("     {} {}{desc}", self.badge(&name), self.paint("2", &sig));
                            self.line(&s);
                        }
                    }
                }
            }
            "message_end" => self.message_end(&ev["message"]),
            "message_update" => self.message_update(&ev["assistantMessageEvent"]),
            "tool_execution_start" => self.tool_start(ev),
            "tool_execution_update" => {
                let id = ev["toolCallId"].as_str().unwrap_or("").to_string();
                let n = result_text(&ev["partialResult"]).len();
                if let Some(t) = self.tools.get_mut(&id) {
                    t.partial_bytes = n;
                }
                self.tick();
            }
            "tool_execution_end" => self.tool_end(ev),
            "auto_retry_start" => {
                self.retries += 1;
                self.gap();
                let s = format!(
                    "⚠ retry {}/{} in {} — {}",
                    ev["attempt"],
                    ev["maxAttempts"],
                    human_dur(ev["delayMs"].as_f64().unwrap_or(0.0) / 1000.0),
                    ev["errorMessage"].as_str().unwrap_or("")
                );
                let w = self.width();
                let s = wrap(&s, w - 2).join("\n  ");
                let s = self.paint("1;33", &s);
                self.line(&s);
            }
            "auto_retry_end" => {
                let ok = ev["success"].as_bool().unwrap_or(false);
                let s = if ok {
                    self.paint("33", &format!("⚠ retry {} succeeded", ev["attempt"]))
                } else {
                    self.paint("1;31", &format!("✗ retry {} failed", ev["attempt"]))
                };
                self.line(&s);
            }
            "agent_settled" => {
                self.settled = true;
                self.summary();
            }
            // noisy lifecycle events with nothing worth showing
            "agent_start" | "agent_end" | "queue_update" | "bash_execution_update" | "entry_appended" => {}
            other => {
                let s = self.paint(
                    "2",
                    &format!("· {}", if other.is_empty() { "(untyped event)" } else { other }),
                );
                self.line(&s);
            }
        }
    }

    fn message_update(&mut self, ame: &Value) {
        let sub = ame["type"].as_str().unwrap_or("");
        match sub {
            "thinking_start" | "thinking_delta" | "thinking_end" if !self.show_thinking => {}
            "thinking_start" => self.open_stream(StreamKind::Thinking, false),
            "thinking_delta" => self.push_delta(StreamKind::Thinking, ame["delta"].as_str().unwrap_or("")),
            "thinking_end" => self.close_stream(StreamKind::Thinking),
            "text_start" => {
                self.turn_had_text = true;
                self.open_stream(StreamKind::Text, false)
            }
            "text_delta" => self.push_delta(StreamKind::Text, ame["delta"].as_str().unwrap_or("")),
            "text_end" => self.close_stream(StreamKind::Text),
            "toolcall_start" => {
                let s = format!(
                    "{} {}",
                    self.paint("2", "→ calling"),
                    self.badge(ame["toolName"].as_str().unwrap_or("?"))
                );
                self.line(&s);
            }
            _ => {}
        }
    }

    fn message_end(&mut self, m: &Value) {
        match m["role"].as_str().unwrap_or("") {
            "user" => {
                let text = &crate::util::content_text(&m["content"]);
                let w = self.width();
                let body = wrap(text, w - 4).join("\n  ");
                self.gap();
                let s = format!("{}\n  {}", self.paint("1;34", "▸ user"), body);
                self.line(&s);
            }
            "assistant" => {
                self.usage.add(&m["usage"]);
                let summary = crate::cells::response_summary(m);
                let w = self.width();
                let s = wrap(&format!("⇄ {summary}"), w - 4).join("\n     ");
                let s = self.paint(
                    if m["stopReason"] == "error" { "31" } else { "2;36" },
                    &format!("   {s}"),
                );
                self.line(&s);
                if m["stopReason"] == "error" {
                    let msg = format!("✗ model error: {}", m["errorMessage"].as_str().unwrap_or("unknown"));
                    let w = self.width();
                    let s = self.paint("31", &wrap(&msg, w - 2).join("\n  "));
                    self.line(&s);
                }
            }
            _ => {}
        }
    }

    fn tool_start(&mut self, ev: &Value) {
        let id = ev["toolCallId"].as_str().unwrap_or("").to_string();
        let name = ev["toolName"].as_str().unwrap_or("?").to_string();
        let args = args_summary(&ev["args"]);
        self.gap();
        self.tool_calls += 1;
        let w = self.width();
        let head_w = 2 + str_width(&name) + 2; // "┌ " + " name "
        let mut s = format!("{}{}", self.paint("1;34", "┌ "), self.badge(&name));
        let arg_lines = wrap(&args, w.saturating_sub(head_w + 2).max(20));
        let pad = " ".repeat(head_w + 1);
        for (i, l) in arg_lines.iter().enumerate() {
            if i == 0 {
                s += &format!(" {}", self.paint("1", l));
            } else {
                s += &format!(
                    "\n{}{}",
                    self.paint("34", &pad.replacen(' ', "│", 1)),
                    self.paint("1", l)
                );
            }
        }
        self.line(&s);
        self.turn_tools.push(name.clone());
        match self.tool_stats.iter_mut().find(|t| t.0 == name) {
            Some(t) => t.1 += 1,
            None => self.tool_stats.push((name.clone(), 1, 0)),
        }
        self.tools.insert(
            id.clone(),
            ToolRun {
                name,
                args,
                started: Instant::now(),
                partial_bytes: 0,
            },
        );
        self.tool_seq.insert(id.clone(), self.seq);
        if self.tty {
            let st = self.tool_status(&id);
            self.emit(&st);
            self.live = Some(Live::Tool { id });
        }
    }

    fn tool_end(&mut self, ev: &Value) {
        let id = ev["toolCallId"].as_str().unwrap_or("").to_string();
        let is_err = ev["isError"].as_bool().unwrap_or(false);
        let run = self.tools.remove(&id);
        let owner = matches!(&self.live, Some(Live::Tool { id: l }) if *l == id);
        let contiguous = owner || self.tool_seq.remove(&id) == Some(self.seq);
        if owner {
            self.emit("\r\x1b[K");
            self.live = None;
        }
        let name = run
            .as_ref()
            .map(|r| r.name.clone())
            .unwrap_or_else(|| ev["toolName"].as_str().unwrap_or("?").to_string());
        let text = crate::util::pretty_if_json(&result_text(&ev["result"]));
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let w = self.width();
        let bar = self.paint("34", "│ ");
        let mut s = String::new();
        if !contiguous {
            // another block was printed in between; restate which call this is
            if let Some(r) = &run {
                s += &self.paint(
                    "34",
                    &format!(
                        "┆ {} {}\n",
                        r.name,
                        trunc(&one_line(&r.args), w.saturating_sub(6 + r.name.len()))
                    ),
                );
            }
        }
        for l in lines.iter().take(self.preview) {
            let body = trunc(&l.replace('\t', "    "), w - 3);
            s += &format!(
                "{bar}{}\n",
                if is_err {
                    self.paint("31", &body)
                } else {
                    self.paint("2", &body)
                }
            );
        }
        if self.preview > 0 && lines.len() > self.preview {
            s += &format!(
                "{bar}{}\n",
                self.paint("2;3", &format!("… +{} more lines", lines.len() - self.preview))
            );
        }
        let dur = run
            .map(|r| human_dur(r.started.elapsed().as_secs_f64()))
            .unwrap_or_else(|| "?".into());
        let meta = format!("{dur} · {}", crate::util::output_meta(&text));
        if is_err {
            self.tool_errors += 1;
            if let Some(t) = self.tool_stats.iter_mut().find(|t| t.0 == name) {
                t.2 += 1;
            }
            s += &format!(
                "{} {} {}",
                self.paint("1;31", "└ ✗"),
                self.badge(&name),
                self.paint("1;31", &format!("FAILED · {meta}"))
            );
        } else {
            s += &format!(
                "{} {} {}",
                self.paint("1;32", "└ ✓"),
                self.badge(&name),
                self.paint("32", &meta)
            );
        }
        self.line(&s);
    }

    fn summary(&mut self) {
        let u = &self.usage;
        let body = format!(
            "✔ settled in {} · {} turns · {} tool calls ({} failed) · {} retries\ntokens: in {} · out {} · cache-read {} · total {}",
            human_dur(self.started.elapsed().as_secs_f64()),
            self.turn,
            self.tool_calls,
            self.tool_errors,
            self.retries,
            u.input,
            u.output,
            u.cache_read,
            u.total
        );
        let w = self.width();
        self.gap();
        let rule = self.paint("1;36", &"━".repeat(w.min(200)));
        let body = body
            .lines()
            .flat_map(|l| wrap(l, w - 2))
            .collect::<Vec<_>>()
            .join("\n  ");
        let mut s = format!("{rule}\n{}", self.paint("1", &body));
        if !self.tool_stats.is_empty() {
            s += &format!(
                "\n  {} {}",
                self.paint("1", "tools used:"),
                self.tool_list(&self.tool_stats.clone())
            );
        }
        self.line(&s);
    }

    pub fn eof(&mut self) {
        if !self.settled {
            let s = self.paint(
                "1;31",
                &format!(
                    "✗ stream ended without agent_settled — run killed or unfinished ({} tool calls, {} still open)",
                    self.tool_calls,
                    self.tools.len()
                ),
            );
            self.line(&s);
        } else {
            self.seal();
        }
    }
}
