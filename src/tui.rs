//! Interactive full-screen trace browser: j/k move cell by cell, l opens a cell's full content,
//! h goes back. Works live (while the stream arrives) and on saved traces.

use crate::cells::{Cell, Kind, Model, Status};
use crate::util::{badge, human_bytes, human_dur, str_width, tool_list, trunc, wrap};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

pub enum Input {
    Line(Vec<u8>),
    Eof,
    Key(Key),
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Key {
    Char(char),
    Up,
    Down,
    Left,
    Right,
    Enter,
    Esc,
    CtrlC,
    CtrlD,
    CtrlU,
    Backspace,
}

/// Raw-mode /dev/tty + alternate screen; restored on drop (also on panic unwind).
struct Tty {
    file: std::fs::File,
    old: libc::termios,
    active: bool,
}

impl Tty {
    fn open() -> Result<Tty> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .context("opening /dev/tty (interactive mode needs a terminal)")?;
        let fd = file.as_raw_fd();
        let mut old: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut old) } != 0 {
            anyhow::bail!("tcgetattr failed on /dev/tty");
        }
        let mut raw = old;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
        raw.c_iflag &= !(libc::IXON | libc::ICRNL);
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 0;
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) };
        let mut t = Tty {
            file,
            old,
            active: true,
        };
        t.write("\x1b[?1049h\x1b[?25l\x1b[2J");
        Ok(t)
    }

    fn write(&mut self, s: &str) {
        let _ = self.file.write_all(s.as_bytes());
        let _ = self.file.flush();
    }

    fn size(&self) -> (usize, usize) {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(self.file.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0 {
            (ws.ws_col as usize, ws.ws_row as usize)
        } else {
            (100, 30)
        }
    }

    fn restore(&mut self) {
        if self.active {
            self.write("\x1b[?25h\x1b[?1049l");
            unsafe { libc::tcsetattr(self.file.as_raw_fd(), libc::TCSANOW, &self.old) };
            self.active = false;
        }
    }
}

impl Drop for Tty {
    fn drop(&mut self) {
        self.restore();
    }
}

fn parse_keys(buf: &[u8], out: &mut Vec<Key>) {
    let mut i = 0;
    while i < buf.len() {
        let b = buf[i];
        if b == 0x1b {
            if buf.get(i + 1) == Some(&b'[') || buf.get(i + 1) == Some(&b'O') {
                let k = match buf.get(i + 2) {
                    Some(b'A') => Some(Key::Up),
                    Some(b'B') => Some(Key::Down),
                    Some(b'C') => Some(Key::Right),
                    Some(b'D') => Some(Key::Left),
                    _ => None,
                };
                if let Some(k) = k {
                    out.push(k);
                    i += 3;
                    continue;
                }
                // skip unknown CSI sequence
                i += 2;
                while i < buf.len() && !(0x40..=0x7e).contains(&buf[i]) {
                    i += 1;
                }
                i += 1;
                continue;
            }
            out.push(Key::Esc);
            i += 1;
            continue;
        }
        if b >= 0x80 {
            // multi-byte UTF-8 (e.g. CJK search terms)
            let len = if b >= 0xf0 {
                4
            } else if b >= 0xe0 {
                3
            } else if b >= 0xc0 {
                2
            } else {
                1
            };
            if let Some(ch) = buf
                .get(i..i + len)
                .and_then(|s| std::str::from_utf8(s).ok())
                .and_then(|s| s.chars().next())
            {
                out.push(Key::Char(ch));
            }
            i += len;
            continue;
        }
        out.push(match b {
            b'\r' | b'\n' => Key::Enter,
            0x7f | 0x08 => Key::Backspace,
            3 => Key::CtrlC,
            4 => Key::CtrlD,
            21 => Key::CtrlU,
            c if c.is_ascii_graphic() || c == b' ' => Key::Char(c as char),
            _ => {
                i += 1;
                continue;
            }
        });
        i += 1;
    }
}

fn spawn_keys(file: std::fs::File, tx: Sender<Input>, stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let mut file = file;
        let fd = file.as_raw_fd();
        let mut buf = [0u8; 64];
        while !stop.load(Ordering::SeqCst) {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut pfd, 1, 100) } <= 0 {
                continue;
            }
            let n = match file.read(&mut buf) {
                Ok(n) if n > 0 => n,
                _ => continue,
            };
            let mut keys = Vec::new();
            parse_keys(&buf[..n], &mut keys);
            for k in keys {
                if tx.send(Input::Key(k)).is_err() {
                    return;
                }
            }
        }
    });
}

/// Truncate an ANSI-styled line to `width` display columns.
fn fit(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut w = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            out.push(c);
            // copy the escape sequence verbatim
            while let Some(&n) = chars.peek() {
                out.push(n);
                chars.next();
                if n.is_ascii_alphabetic() || n == '\\' {
                    break;
                }
            }
            continue;
        }
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    out + "\x1b[0m"
}

fn paint(code: &str, s: &str) -> String {
    format!("\x1b[{code}m{s}\x1b[0m")
}

const THINK_LINES: usize = 4;
const ANSWER_LINES: usize = 40;
const TOOL_PREVIEW: usize = 4;

fn more(n: usize) -> String {
    paint("2;3", &format!("… +{n} more lines  (l to open)"))
}

/// Compact list rendering of one cell (content width `cw`).
fn cell_lines(c: &Cell, m: &Model, cw: usize) -> Vec<String> {
    let mut v = Vec::new();
    match c.kind {
        Kind::Session => {
            v.extend(
                wrap(&format!("━━ {}", c.title), cw)
                    .into_iter()
                    .map(|l| paint("1;36", &l)),
            );
            for l in c.body.lines() {
                if let Some(rest) = l.strip_prefix("model ") {
                    // model id as a magenta badge, the details beside it
                    let (id, details) = rest.split_once(" · ").unwrap_or((rest, ""));
                    let head = format!("   {} {}", paint("1;35", "model  "), crate::util::model_badge(id, true));
                    let head_w = 3 + 7 + 1 + str_width(id) + 2;
                    if head_w + 1 + str_width(details) <= cw {
                        v.push(format!("{head} {}", paint("35", details)));
                    } else {
                        v.push(head);
                        v.extend(
                            wrap(details, cw - 11)
                                .into_iter()
                                .map(|d| format!("           {}", paint("35", &d))),
                        );
                    }
                } else if let Some(rest) = l.strip_prefix("backend ") {
                    v.extend(wrap(rest, cw - 11).into_iter().enumerate().map(|(i, d)| {
                        format!(
                            "   {}{}",
                            paint("1;35", if i == 0 { "backend " } else { "        " }),
                            paint("35", &d)
                        )
                    }));
                } else {
                    v.extend(wrap(l, cw - 3).into_iter().map(|l| paint("36", &format!("   {l}"))));
                }
            }
        }
        Kind::System => {
            v.extend(badge_lines(
                paint("2", "· tools available:"),
                18,
                &m.tools_available,
                cw,
            ));
            v.push(paint("2;3", &format!("· {}  (l to read the prompt)", c.title)));
        }
        Kind::Tools => {
            v.push(format!(
                "{} {}",
                paint("1;34", "⚙"),
                paint("1;34", &format!("{}  (l: JSON)", c.title))
            ));
            let parsed: Value = serde_json::from_str(&c.body).unwrap_or(Value::Null);
            if parsed.is_object() {
                // Claude Code system/init: tool names only (no schemas in the stream)
                v.extend(badge_lines(paint("2", "   tools:"), 9, &m.tools_available, cw));
                if let Some(mcp) = parsed["mcp_servers"].as_array().filter(|a| !a.is_empty()) {
                    let names: Vec<String> = mcp
                        .iter()
                        .map(|s| {
                            format!(
                                "{} ({})",
                                s["name"].as_str().unwrap_or("?"),
                                s["status"].as_str().unwrap_or("?")
                            )
                        })
                        .collect();
                    v.extend(
                        wrap(&format!("   mcp_servers: {}", names.join(", ")), cw)
                            .into_iter()
                            .map(|l| paint("2", &l)),
                    );
                }
            }
            let defs: Vec<Value> = parsed.as_array().cloned().unwrap_or_default();
            for d in defs.iter().filter(|d| d["parameters"].is_object()) {
                let (name, params, desc) = crate::cells::tool_signature(d);
                let head = format!("   {} ", badge(&name, true));
                let head_w = 3 + str_width(&name) + 3;
                let sig = format!("({params})");
                // signature on the badge line, description dimmed after it, clipped to the width
                let avail = cw.saturating_sub(head_w);
                let sig_t = trunc(&sig, avail);
                let desc_room = avail.saturating_sub(str_width(&sig_t) + 3);
                let desc_t = if desc_room > 8 {
                    format!(" {}", paint("2", &trunc(&format!("— {desc}"), desc_room)))
                } else {
                    String::new()
                };
                v.push(format!("{head}{}{desc_t}", paint("1", &sig_t)));
            }
        }
        Kind::User => {
            v.push(paint("1;34", "▸ user"));
            v.extend(wrap(&c.body, cw - 2).into_iter().map(|l| format!("  {l}")));
        }
        Kind::Thinking => {
            v.push(paint("2;35", "💭 thinking"));
            if c.body.trim().is_empty() {
                v.push(format!(
                    "{}{}",
                    paint("2", "  ┊ "),
                    paint("2;3", "(empty — no thinking text in the stream)")
                ));
            }
            let lines = if c.body.trim().is_empty() {
                Vec::new()
            } else {
                wrap(c.body.trim(), cw - 4)
            };
            for l in lines.iter().take(THINK_LINES) {
                v.push(format!("{}{}", paint("2", "  ┊ "), paint("2;3", l)));
            }
            if lines.len() > THINK_LINES {
                v.push(format!("    {}", more(lines.len() - THINK_LINES)));
            }
        }
        Kind::Answer if c.final_answer => {
            let model = m
                .model
                .as_ref()
                .map(|mi| format!(" · {}", mi.model))
                .unwrap_or_default();
            v.push(format!(
                "\x1b[1;30;42m ★ FINAL ANSWER \x1b[0m{}",
                paint("32", &format!(" turn {}{model}", c.turn))
            ));
            // the final answer is shown in full, behind a green bar
            for l in crate::md::render(c.body.trim_end(), cw - 2) {
                v.push(format!("{}{l}", paint("1;32", "┃ ")));
            }
        }
        Kind::Answer => {
            v.push(paint("1;32", "● assistant"));
            let lines = crate::md::render(c.body.trim_end(), cw - 2);
            for l in lines.iter().take(ANSWER_LINES) {
                v.push(format!("  {l}"));
            }
            if lines.len() > ANSWER_LINES {
                v.push(format!("  {}", more(lines.len() - ANSWER_LINES)));
            }
        }
        Kind::Tool => {
            let name = c.tool.clone().unwrap_or_default();
            let bw = str_width(&name) + 2;
            let args = wrap(&c.title, cw.saturating_sub(bw + 3).max(10));
            for (i, l) in args.iter().take(3).enumerate() {
                if i == 0 {
                    v.push(format!(
                        "{}{} {}",
                        paint("1;34", "┌ "),
                        badge(&name, true),
                        paint("1", l)
                    ));
                } else {
                    v.push(format!("{}{}{}", paint("34", "│ "), " ".repeat(bw + 1), paint("1", l)));
                }
            }
            if args.len() > 3 {
                v.push(format!("{}{}", paint("34", "│ "), more(args.len() - 3)));
            }
            match c.status {
                Status::Running => {
                    let mut s = format!("│ ⏳ running {}", human_dur(c.started.elapsed().as_secs_f64()));
                    if c.partial_bytes > 0 {
                        s += &format!(" · {}", human_bytes(c.partial_bytes));
                    }
                    v.push(paint("33", &s));
                }
                st => {
                    let err = st == Status::Err;
                    let lines: Vec<&str> = c.body.lines().filter(|l| !l.trim().is_empty()).collect();
                    for l in lines.iter().take(TOOL_PREVIEW) {
                        let body = trunc(&l.replace('\t', "    "), cw - 2);
                        v.push(format!(
                            "{}{}",
                            paint("34", "│ "),
                            paint(if err { "31" } else { "2" }, &body)
                        ));
                    }
                    if lines.len() > TOOL_PREVIEW {
                        v.push(format!("{}{}", paint("34", "│ "), more(lines.len() - TOOL_PREVIEW)));
                    }
                    let meta = format!(
                        "{} · {} lines · {}",
                        human_dur(c.secs.unwrap_or(0.0)),
                        c.body.lines().count(),
                        human_bytes(c.body.len())
                    );
                    v.push(if err {
                        format!(
                            "{} {} {}",
                            paint("1;31", "└ ✗"),
                            badge(&name, true),
                            paint("1;31", &format!("FAILED · {meta}"))
                        )
                    } else {
                        format!("{} {} {}", paint("1;32", "└ ✓"), badge(&name, true), paint("32", &meta))
                    });
                }
            }
        }
        Kind::Retry => {
            let st = match c.status {
                Status::Ok => " → recovered",
                Status::Err => " → FAILED",
                _ => " …",
            };
            let text = format!("⚠ {} — {}{st}", c.title, c.body);
            v.extend(
                wrap(&text, cw - 2)
                    .iter()
                    .enumerate()
                    .map(|(i, l)| paint("1;33", &if i == 0 { l.clone() } else { format!("  {l}") })),
            );
        }
        Kind::ModelError => {
            let text = format!("✗ model error: {}", c.title);
            v.extend(
                wrap(&text, cw - 2)
                    .iter()
                    .enumerate()
                    .map(|(i, l)| paint("31", &if i == 0 { l.clone() } else { format!("  {l}") })),
            );
        }
        Kind::Summary if c.title.starts_with("result ") => {
            v.push(paint("1;36", &"━".repeat(cw.min(200))));
            v.push(paint("1", &trunc(&format!("✔ {}", c.title), cw)));
            let u = &m.usage;
            v.push(paint(
                "1",
                &trunc(
                    &format!(
                        "  usage (from result): in {} · out {} · cache-read {}",
                        u.input, u.output, u.cache_read
                    ),
                    cw,
                ),
            ));
            if !m.tool_stats.is_empty() {
                v.push(format!(
                    "  {} {}",
                    paint("1", "tools used:"),
                    tool_list(&m.tool_stats, true)
                ));
            }
            if let Some(t) = m.final_turn {
                v.push(format!(
                    "  {} {}",
                    paint("1;32", &format!("★ final answer: turn {t}")),
                    paint("2", "(press a)")
                ));
            }
        }
        Kind::Summary => {
            let calls: usize = m.tool_stats.iter().map(|t| t.1).sum();
            let fails: usize = m.tool_stats.iter().map(|t| t.2).sum();
            v.push(paint("1;36", &"━".repeat(cw.min(200))));
            v.push(paint(
                "1",
                &trunc(
                    &format!(
                        "✔ {} · {} turns · {calls} tool calls ({fails} failed) · {} retries",
                        c.title,
                        m.turn(),
                        m.retries
                    ),
                    cw,
                ),
            ));
            let u = &m.usage;
            v.push(paint(
                "1",
                &trunc(
                    &format!(
                        "  tokens: in {} · out {} · cache-read {} · total {}",
                        u.input, u.output, u.cache_read, u.total
                    ),
                    cw,
                ),
            ));
            if !m.tool_stats.is_empty() {
                v.push(format!(
                    "  {} {}",
                    paint("1", "tools used:"),
                    tool_list(&m.tool_stats, true)
                ));
            }
            if let Some(t) = m.final_turn {
                v.push(format!(
                    "  {} {}",
                    paint("1;32", &format!("★ final answer: turn {t}")),
                    paint("2", "(press a)")
                ));
            }
        }
        Kind::Response => {
            let color = if c.status == Status::Err { "31" } else { "2;36" };
            for (i, l) in wrap(&format!("⇄ LLM response · {}", c.title), cw - 2)
                .iter()
                .enumerate()
            {
                v.push(paint(color, &if i == 0 { l.clone() } else { format!("  {l}") }));
            }
        }
        Kind::Unfinished => v.push(paint("1;31", &trunc(&format!("✗ {}", c.title), cw))),
        Kind::Malformed => v.push(paint("33", &trunc(&format!("⚠ {}", c.title), cw))),
        Kind::Other if c.status == Status::Err => {
            v.extend(
                wrap(&format!("✗ {}", c.title), cw - 2)
                    .iter()
                    .enumerate()
                    .map(|(i, l)| paint("1;31", &if i == 0 { l.clone() } else { format!("  {l}") })),
            );
        }
        Kind::Other => v.push(paint("2", &trunc(&format!("· {}", c.title), cw))),
    }
    v
}

/// `label` followed by tool badges, wrapped to `cw` columns (continuation lines indented).
fn badge_lines(label: String, label_w: usize, tools: &[String], cw: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = label;
    let mut w = label_w;
    for t in tools {
        let bw = str_width(t) + 3; // " name " + separating space
        if w + bw > cw && w > 2 {
            lines.push(std::mem::take(&mut cur));
            cur = "  ".into();
            w = 2;
        }
        cur += " ";
        cur += &badge(t, true);
        w += bw;
    }
    lines.push(cur);
    lines
}

/// Full content of one cell for the detail view.
fn detail_lines(c: &Cell, m: &Model, cw: usize) -> Vec<String> {
    let mut v = Vec::new();
    let head = match c.kind {
        Kind::Answer if c.final_answer => "\x1b[1;30;42m ★ FINAL ANSWER \x1b[0m".to_string(),
        Kind::Response => paint("1;36", "⇄ LLM response · assistant message as recorded"),
        Kind::Tools => paint("1;34", "⚙ toolsAdded as recorded in the system message"),
        Kind::Tool => format!(
            "{} {}",
            badge(c.tool.as_deref().unwrap_or("?"), true),
            paint("1", "tool call")
        ),
        k => paint("1;36", &format!("{k:?}")),
    };
    v.push(format!(
        "{head}  {}",
        paint("2", &format!("turn {} · input line {}", c.turn, c.lineno))
    ));
    v.push(paint("2", &"─".repeat(cw.min(200))));
    let plain = |v: &mut Vec<String>, text: &str| {
        for raw in text.split('\n') {
            v.extend(wrap(raw, cw));
        }
    };
    // JSON gets syntax colors; anything else is shown as plain wrapped text
    let json_or_plain = |v: &mut Vec<String>, text: &str| {
        if crate::jsonhl::is_json(text) {
            v.extend(crate::jsonhl::highlight(text, cw));
        } else {
            for raw in text.split('\n') {
                v.extend(wrap(raw, cw));
            }
        }
    };
    match c.kind {
        Kind::Tool => {
            v.push(paint("1;34", "args"));
            json_or_plain(&mut v, &c.args_full);
            v.push(String::new());
            let status = match c.status {
                Status::Running => paint(
                    "33",
                    &format!("running {}", human_dur(c.started.elapsed().as_secs_f64())),
                ),
                Status::Err => paint("1;31", &format!("FAILED · {}", human_dur(c.secs.unwrap_or(0.0)))),
                _ => paint("32", &format!("ok · {}", human_dur(c.secs.unwrap_or(0.0)))),
            };
            v.push(format!(
                "{}  {status} · {} lines · {}",
                paint("1;34", "output"),
                c.body.lines().count(),
                human_bytes(c.body.len())
            ));
            json_or_plain(&mut v, &c.body);
        }
        Kind::Answer => v.extend(crate::md::render(c.body.trim_end(), cw)),
        Kind::Thinking => {
            for raw in c.body.trim().split('\n') {
                v.extend(wrap(raw, cw).into_iter().map(|l| paint("3", &l)));
            }
        }
        Kind::System => {
            v.extend(badge_lines(paint("1", "tools available:"), 16, &m.tools_available, cw));
            v.push(String::new());
            plain(&mut v, &c.body);
        }
        Kind::Summary if c.title.starts_with("result ") => json_or_plain(&mut v, &c.body),
        Kind::Summary => v.extend(cell_lines(c, m, cw)),
        Kind::Tools | Kind::Response => json_or_plain(&mut v, &c.body),
        _ => {
            if !c.title.is_empty() {
                plain(&mut v, &c.title);
            }
            if !c.body.is_empty() {
                v.push(String::new());
                json_or_plain(&mut v, &c.body);
            }
        }
    }
    v
}

struct Ui {
    sel: usize,
    follow: bool,
    top: usize,
    detail: Option<usize>,
    dscroll: usize,
    hide_thinking: bool,
    hide_backend: bool,
    cache: HashMap<usize, (u64, usize, Vec<String>)>,
    /// search prompt being typed after `/`
    input: Option<String>,
    /// committed search term
    query: Option<String>,
    /// one-shot status message (text, is_error)
    msg: Option<(String, bool)>,
    /// current match line in the detail view
    dmatch: Option<usize>,
    reveal: Reveal,
    /// `?` overlay listing every key
    help: bool,
}

const LIST_KEYS: &[(&str, &str)] = &[
    ("j/k", "next/prev cell"),
    ("l", "open"),
    ("/", "search"),
    ("n/N", "next/prev match"),
    ("[ ]", "prev/next turn"),
    ("g/G", "top/end"),
    ("a", "final answer"),
    ("t", "thinking on/off"),
    ("b", "backend on/off"),
    ("f", "follow on/off"),
    ("q", "quit"),
];
const DETAIL_KEYS: &[(&str, &str)] = &[
    ("h", "back"),
    ("j/k", "scroll"),
    ("^d/^u", "page"),
    ("/", "search"),
    ("n/N", "next/prev match"),
    ("J/K", "next/prev cell"),
    ("g/G", "top/end"),
];
const INPUT_KEYS: &[(&str, &str)] = &[("Enter", "search"), ("Esc", "cancel"), ("Backspace", "delete")];

/// One row of `key description` pairs that fit in `w`; ends with `? all keys` when some are left out.
fn key_help_row(keys: &[(&str, &str)], w: usize, offer_more: bool) -> String {
    let item = |k: &str, d: &str| format!("\x1b[1;36m{k}\x1b[0m {d}");
    let item_w = |k: &str, d: &str| str_width(k) + 1 + str_width(d);
    let sep = "\x1b[2m · \x1b[0m";
    let more = ("?", "all keys");
    let mut out = String::from(" ");
    let mut used = 1;
    for (i, (k, d)) in keys.iter().enumerate() {
        let need = item_w(k, d) + if i > 0 { 3 } else { 0 };
        let rest_after = i + 1 < keys.len() && offer_more;
        let reserve = if rest_after { 3 + item_w(more.0, more.1) } else { 0 };
        if used + need + reserve > w {
            if i > 0 {
                out += sep;
            }
            out += &item(more.0, more.1);
            return out;
        }
        if i > 0 {
            out += sep;
        }
        out += &item(k, d);
        used += need;
    }
    if offer_more && used + 3 + item_w(more.0, more.1) <= w {
        out += sep;
        out += &item(more.0, more.1);
    }
    out
}

/// Full key reference shown by `?`.
fn help_screen(cw: usize) -> Vec<String> {
    const ALSO: &[(&str, &str)] = &[
        ("Enter / →", "open (same as l)"),
        ("Esc / ←", "back (same as h)"),
        ("^d ^u space", "jump 5 cells in the list"),
        ("^c", "stop everything (pp too)"),
        ("?", "this help"),
    ];
    // (text, plain width)
    let section = |v: &mut Vec<(String, usize)>, title: &str, keys: &[(&str, &str)]| {
        v.push((paint("1", title), str_width(title)));
        for (k, d) in keys {
            v.push((
                format!("  \x1b[1;36m{k:<11}\x1b[0m {d}"),
                2 + 11.max(str_width(k)) + 1 + str_width(d),
            ));
        }
        v.push((String::new(), 0));
    };
    let mut left = Vec::new();
    section(&mut left, "list", LIST_KEYS);
    let mut right = Vec::new();
    section(&mut right, "detail view (after l)", DETAIL_KEYS);
    section(&mut right, "search prompt (after /)", INPUT_KEYS);
    section(&mut right, "also", ALSO);
    let mut v = vec![paint("1;36", "trace_block keys"), String::new()];
    let col = left.iter().map(|x| x.1).max().unwrap_or(30) + 4;
    let right_w = right.iter().map(|x| x.1).max().unwrap_or(30);
    if cw >= col + right_w {
        for i in 0..left.len().max(right.len()) {
            let (l, lw) = left.get(i).cloned().unwrap_or_default();
            let r = right.get(i).map(|x| x.0.clone()).unwrap_or_default();
            v.push(format!("{l}{}{r}", " ".repeat(col.saturating_sub(lw))));
        }
    } else {
        v.extend(left.into_iter().chain(right).map(|x| x.0));
    }
    v.into_iter().map(|l| trunc_ansi(&l, cw)).collect()
}

fn trunc_ansi(s: &str, w: usize) -> String {
    fit(s, w)
}

/// What the next list redraw should scroll into view.
#[derive(Clone, Copy, PartialEq)]
enum Reveal {
    None,
    Cell,
    Match,
}

/// Char-index ranges of `q` in `hay`; smartcase (case-insensitive unless `q` has uppercase).
fn find_all(hay: &str, q: &str) -> Vec<(usize, usize)> {
    if q.is_empty() {
        return Vec::new();
    }
    let sensitive = q.chars().any(|c| c.is_uppercase());
    let norm = |c: char| {
        if sensitive {
            c
        } else {
            c.to_lowercase().next().unwrap_or(c)
        }
    };
    let h: Vec<char> = hay.chars().map(norm).collect();
    let n: Vec<char> = q.chars().map(norm).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + n.len() <= h.len() {
        if h[i..i + n.len()] == n[..] {
            out.push((i, i + n.len()));
            i += n.len();
        } else {
            i += 1;
        }
    }
    out
}

/// Reverse-video the matches of `q` inside an ANSI-styled line.
fn highlight_matches(line: &str, q: &str) -> String {
    let plain = strip_ansi(line);
    let hits = find_all(&plain, q);
    if hits.is_empty() {
        return line.to_string();
    }
    let mut out = String::new();
    let mut idx = 0usize;
    let mut chars = line.chars().peekable();
    let inside = |i: usize| hits.iter().any(|&(a, b)| i >= a && i < b);
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            out.push(c);
            while let Some(&n) = chars.peek() {
                out.push(n);
                chars.next();
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
            if inside(idx) {
                out += "\x1b[7m"; // styles inside the match may reset; re-apply
            }
            continue;
        }
        if hits.iter().any(|&(a, _)| a == idx) {
            out += "\x1b[7m";
        }
        out.push(c);
        idx += 1;
        if hits.iter().any(|&(_, b)| b == idx) {
            out += "\x1b[27m";
        }
    }
    out
}

/// Everything searchable in a cell.
fn haystack(c: &Cell) -> String {
    format!(
        "{}\n{}\n{}\n{}",
        c.tool.as_deref().unwrap_or(""),
        c.title,
        c.args_full,
        c.body
    )
}

/// Next/previous visible cell matching `q`, wrapping around. Returns (index, wrapped).
fn search_cells(ui: &Ui, m: &Model, from: usize, q: &str, fwd: bool) -> Option<(usize, bool)> {
    let n = m.cells.len();
    for k in 1..=n {
        let (i, wrapped) = if fwd {
            ((from + k) % n, from + k >= n)
        } else {
            ((from + n - k) % n, k > from)
        };
        let c = &m.cells[i];
        if visible(ui, c) && !find_all(&haystack(c), q).is_empty() {
            return Some((i, wrapped));
        }
    }
    None
}

/// (position among matches, total matches) of cell `sel`.
fn match_rank(ui: &Ui, m: &Model, sel: usize, q: &str) -> (usize, usize) {
    let hits: Vec<usize> = (0..m.cells.len())
        .filter(|&i| visible(ui, &m.cells[i]) && !find_all(&haystack(&m.cells[i]), q).is_empty())
        .collect();
    (
        hits.iter().position(|&i| i == sel).map(|p| p + 1).unwrap_or(0),
        hits.len(),
    )
}

/// Jump to the next/previous matching line of the open detail view.
fn detail_search(ui: &mut Ui, m: &Model, cw: usize, fwd: bool) {
    let (Some(di), Some(q)) = (ui.detail, ui.query.clone()) else {
        return;
    };
    let Some(c) = m.cells.get(di) else { return };
    let lines: Vec<String> = detail_lines(c, m, cw).iter().map(|l| strip_ansi(l)).collect();
    let hits: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| !find_all(l, &q).is_empty())
        .map(|(i, _)| i)
        .collect();
    if hits.is_empty() {
        ui.msg = Some((format!("not found in this cell: {q}"), true));
        return;
    }
    let cur = ui.dmatch;
    let next = if fwd {
        hits.iter()
            .copied()
            .find(|&h| cur.map_or(h >= ui.dscroll, |c| h > c))
            .map(|h| (h, false))
            .unwrap_or((hits[0], cur.is_some()))
    } else {
        hits.iter()
            .rev()
            .copied()
            .find(|&h| cur.map_or(h < ui.dscroll, |c| h < c))
            .map(|h| (h, false))
            .unwrap_or((*hits.last().unwrap(), true))
    };
    ui.dmatch = Some(next.0);
    ui.dscroll = next.0.saturating_sub(2);
    let pos = hits.iter().position(|&h| h == next.0).unwrap_or(0) + 1;
    let wrap_note = if next.1 {
        if fwd {
            " · wrapped to top"
        } else {
            " · wrapped to bottom"
        }
    } else {
        ""
    };
    ui.msg = Some((format!("/{q} · line match {pos}/{}{wrap_note}", hits.len()), false));
}

impl Ui {
    fn cached(&mut self, i: usize, c: &Cell, m: &Model, cw: usize) -> Vec<String> {
        let cacheable =
            matches!(c.kind, Kind::Thinking | Kind::Answer | Kind::User | Kind::Tool) && c.status != Status::Running;
        if cacheable
            && let Some((ver, w, lines)) = self.cache.get(&i)
            && *ver == c.ver
            && *w == cw
        {
            return lines.clone();
        }
        let lines = cell_lines(c, m, cw);
        if cacheable {
            self.cache.insert(i, (c.ver, cw, lines.clone()));
        }
        lines
    }
}

fn turn_header(m: &Model, t: usize, cw: usize) -> String {
    let info = m.turns.get(t - 1);
    let at = info.map(|x| x.at).unwrap_or(0.0);
    let is_final = m.final_turn == Some(t);
    let label = if is_final {
        format!(" TURN {t} · FINAL ")
    } else {
        format!(" TURN {t} ")
    };
    let model = info.map(|x| x.model.as_str()).filter(|s| !s.is_empty());
    let clock = format!(" +{} ", human_dur(at));
    let model_w = model.map(|mo| str_width(mo) + 3).unwrap_or(0);
    let fill = cw
        .saturating_sub(str_width(&label) + model_w + str_width(&clock) + 1)
        .min(200);
    let (bg, fg) = if is_final { ("42", "32") } else { ("46", "36") };
    let model_part = model
        .map(|mo| format!(" {}", crate::util::model_badge(mo, true)))
        .unwrap_or_default();
    format!(
        "\x1b[1;30;{bg}m{label}\x1b[0m\x1b[{fg}m {}\x1b[0m{model_part}\x1b[2m{clock}\x1b[0m",
        "━".repeat(fill)
    )
}

fn turn_footer(m: &Model, t: usize) -> Option<String> {
    let info = m.turns.get(t - 1)?;
    let stop = info.stop.as_ref()?;
    let mut s = format!("╰─ turn {t} · {stop}");
    if let Some((i, o)) = info.usage {
        s += &format!(" · in {i} / out {o} tok");
    }
    if let Some(sec) = info.secs {
        s += &format!(" · {}", human_dur(sec));
    }
    let mut out = paint("2;36", &s);
    if !info.tools.is_empty() {
        let mut stats: Vec<(String, usize, usize)> = Vec::new();
        for n in &info.tools {
            match stats.iter_mut().find(|x| &x.0 == n) {
                Some(x) => x.1 += 1,
                None => stats.push((n.clone(), 1, 0)),
            }
        }
        out += &format!("{} {}", paint("2;36", " · tools"), tool_list(&stats, true));
    }
    Some(out)
}

fn draw(tty: &mut Tty, ui: &mut Ui, m: &Model, source: &str) {
    let (w, h) = tty.size();
    let body_h = h.saturating_sub(2).max(1);
    let cw = w.saturating_sub(3).max(20);
    let mut rows: Vec<String> = Vec::with_capacity(body_h);
    let status_right;
    let status_left;

    if let Some(di) = ui.detail {
        let di = di.min(m.cells.len().saturating_sub(1));
        let lines = m.cells.get(di).map(|c| detail_lines(c, m, cw)).unwrap_or_default();
        let max_scroll = lines.len().saturating_sub(body_h);
        ui.dscroll = ui.dscroll.min(max_scroll);
        for l in lines.iter().skip(ui.dscroll).take(body_h) {
            rows.push(format!(" {l}"));
        }
        status_left = String::new();
        status_right = format!(
            " cell {}/{} · lines {}-{}/{} ",
            di + 1,
            m.cells.len(),
            ui.dscroll + 1,
            (ui.dscroll + body_h).min(lines.len()),
            lines.len()
        );
    } else {
        if ui.follow && !m.cells.is_empty() {
            ui.sel = m.cells.len() - 1;
        }
        ui.sel = ui.sel.min(m.cells.len().saturating_sub(1));
        ui.sel = visible_near(ui, m, ui.sel);
        // compose all lines: (owning cell, text)
        let mut all: Vec<(Option<usize>, String)> = Vec::new();
        let mut cur_turn = 0usize;
        for i in 0..m.cells.len() {
            let c = &m.cells[i];
            if !visible(ui, c) {
                continue;
            }
            if c.turn != cur_turn {
                if cur_turn > 0
                    && let Some(f) = turn_footer(m, cur_turn)
                {
                    all.push((None, String::new()));
                    all.push((None, f));
                }
                if c.turn > 0 {
                    all.push((None, String::new()));
                    all.push((None, String::new()));
                    all.push((None, turn_header(m, c.turn, cw)));
                }
                cur_turn = c.turn;
            }
            if !all.is_empty() && all.last().is_some_and(|(o, _)| o.is_some()) {
                all.push((None, String::new()));
            }
            let lines = ui.cached(i, c, m, cw);
            all.extend(lines.into_iter().map(|l| (Some(i), l)));
        }
        if cur_turn > 0
            && let Some(f) = turn_footer(m, cur_turn)
        {
            all.push((None, String::new()));
            all.push((None, f));
        }
        // scroll so the selected cell (plus its turn header) is visible
        let a = all.iter().position(|(o, _)| *o == Some(ui.sel)).unwrap_or(0);
        let b = all
            .iter()
            .rposition(|(o, _)| *o == Some(ui.sel))
            .map(|x| x + 1)
            .unwrap_or(a);
        let mut a_ctx = a;
        while a_ctx > 0 && a - a_ctx < 4 && all[a_ctx - 1].0.is_none() {
            a_ctx -= 1;
        }
        let total = all.len();
        let reveal_cell = |top: &mut usize| {
            if b - a > body_h || a_ctx < *top {
                *top = a_ctx.min(a);
            } else if b > *top + body_h {
                *top = b - body_h;
            }
        };
        if ui.follow {
            ui.top = total.saturating_sub(body_h);
        } else {
            match std::mem::replace(&mut ui.reveal, Reveal::None) {
                Reveal::Match => {
                    // put the first matching line of the selected cell in view
                    let hit = ui
                        .query
                        .as_ref()
                        .and_then(|q| (a..b).find(|&i| !find_all(&strip_ansi(&all[i].1), q).is_empty()));
                    match hit {
                        Some(i) => ui.top = i.saturating_sub(body_h / 3).max(a_ctx.min(i)),
                        None => reveal_cell(&mut ui.top),
                    }
                    if hit.is_none()
                        && ui.msg.as_ref().is_some_and(|m| !m.1)
                        && let Some((t, _)) = &mut ui.msg
                    {
                        *t += " · match not in preview (l to open)";
                    }
                }
                Reveal::Cell => reveal_cell(&mut ui.top),
                // new output arriving: keep the view unless the selection left it entirely
                Reveal::None => {
                    if b <= ui.top || a >= ui.top + body_h {
                        ui.top = a_ctx.min(a);
                    }
                }
            }
        }
        ui.top = ui.top.min(total.saturating_sub(body_h));
        for (owner, l) in all.iter().skip(ui.top).take(body_h) {
            let gutter = if *owner == Some(ui.sel) {
                "\x1b[1;36m▌\x1b[0m "
            } else {
                "  "
            };
            rows.push(format!("{gutter}{l}"));
        }
        let state = if m.settled {
            paint("1;32", "✔ settled")
        } else if m.eof {
            paint("1;31", "■ ended")
        } else {
            paint("1;33", "● live")
        };
        status_left = if ui.follow {
            " following new output ".into()
        } else {
            String::new()
        };
        let turn = m.cells.get(ui.sel).map(|c| c.turn).unwrap_or(0);
        let model = m
            .model
            .as_ref()
            .map(|mi| format!("\x1b[0;1;97;45m {} \x1b[0;7m ", mi.model))
            .unwrap_or_default();
        let pos = format!(
            "cell {}/{} · turn {turn} · {state}\x1b[7m ",
            (ui.sel + 1).min(m.cells.len()),
            m.cells.len()
        );
        let full = format!(" {model}{source} · {pos}");
        status_right = if str_width(&strip_ansi(&full)) > w {
            format!(" {model}{pos}")
        } else {
            full
        };
    }

    let mut status_left = status_left;
    let mut status_right = status_right;
    if let Some(q) = ui.query.as_ref().filter(|_| ui.input.is_none()) {
        for r in rows.iter_mut() {
            *r = highlight_matches(r, q);
        }
    }
    if ui.help {
        rows = help_screen(cw);
    }
    let keys: &[(&str, &str)] = if ui.input.is_some() {
        INPUT_KEYS
    } else if ui.help {
        &[("any key", "close this help")]
    } else if ui.detail.is_some() {
        DETAIL_KEYS
    } else {
        LIST_KEYS
    };
    if let Some(buf) = &ui.input {
        status_left = format!(" /{buf}\u{2581} ");
    } else if let Some((text, err)) = &ui.msg {
        let color = if *err { "\x1b[0;1;31m" } else { "\x1b[0;1;33m" };
        status_right = format!("{color} {text} \x1b[0;7m {}", status_right.trim_start());
    }
    let mut frame = String::from("\x1b[H");
    for r in 0..body_h {
        frame += &fit(rows.get(r).map(String::as_str).unwrap_or(""), w);
        frame += "\x1b[K\r\n";
    }
    // key help row: whole "key description" pairs only, never bare keys
    frame += &key_help_row(keys, w, ui.input.is_none() && !ui.help);
    frame += "\x1b[K\r\n";
    let right_plain_w = str_width(&strip_ansi(&status_right));
    if str_width(&status_left) + right_plain_w > w {
        status_left.clear();
    }
    let lw = str_width(&status_left);
    let pad = w.saturating_sub(lw + right_plain_w);
    frame += &fit(&format!("\x1b[7m{status_left}{}{status_right}", " ".repeat(pad)), w);
    frame += "\x1b[K";
    tty.write(&frame);
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut esc = false;
    for c in s.chars() {
        if c == '\x1b' {
            esc = true;
        } else if esc {
            if c.is_ascii_alphabetic() {
                esc = false;
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// First cell index of the turn containing `sel`, or of the previous turn if already there.
fn turn_start(m: &Model, sel: usize, back: bool) -> usize {
    let t = m.cells.get(sel).map(|c| c.turn).unwrap_or(0);
    if back {
        let first_of = |t: usize| m.cells.iter().position(|c| c.turn == t);
        match first_of(t) {
            Some(f) if f < sel => f,
            _ => (0..t).rev().find_map(first_of).unwrap_or(0),
        }
    } else {
        m.cells.iter().position(|c| c.turn > t).unwrap_or(sel)
    }
}

/// Run the TUI. `on_line` handles passthrough/saving and parses a raw line;
/// `on_event` feeds side outputs (diagram). Returns when the user quits.
pub fn run(
    rx: Receiver<Input>,
    tx: Sender<Input>,
    source: &str,
    // live pipe: keep consuming after the user quits so the producer and -o stay intact
    drain_on_quit: bool,
    hide_thinking: bool,
    mut on_line: impl FnMut(&[u8]) -> Option<Result<Value, String>>,
    mut on_event: impl FnMut(&Value, bool),
) -> Result<()> {
    let mut tty = Tty::open()?;
    let stop = Arc::new(AtomicBool::new(false));
    spawn_keys(tty.file.try_clone()?, tx, stop.clone());
    let mut m = Model::new();
    let mut ui = Ui {
        sel: 0,
        follow: true,
        top: 0,
        detail: None,
        dscroll: 0,
        hide_thinking,
        hide_backend: false,
        cache: HashMap::new(),
        input: None,
        query: None,
        msg: None,
        dmatch: None,
        reveal: Reveal::Cell,
        help: false,
    };
    let mut lineno = 0usize;
    let mut dirty = true;
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    let mut quit = false;

    while !quit {
        let msg = rx.recv_timeout(Duration::from_millis(50));
        let mut batch = Vec::new();
        match msg {
            Ok(x) => batch.push(x),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                if !m.eof {
                    m.finish();
                    on_event(&Value::Null, true);
                    dirty = true;
                }
            }
        }
        while let Ok(x) = rx.try_recv() {
            batch.push(x);
            if batch.len() > 2000 {
                break;
            }
        }
        let mut key_pressed = false;
        for x in batch {
            match x {
                Input::Line(raw) => {
                    lineno += 1;
                    match on_line(&raw) {
                        Some(Ok(ev)) => {
                            m.event(&ev, lineno);
                            on_event(&ev, false);
                        }
                        Some(Err(e)) => m.malformed(lineno, &e),
                        None => {}
                    }
                    dirty = true;
                }
                Input::Eof => {
                    if !m.eof {
                        m.finish();
                        on_event(&Value::Null, true);
                    }
                    dirty = true;
                }
                Input::Key(k) => {
                    key_pressed = true;
                    let typing = ui.input.is_some();
                    if handle_key(k, &mut ui, &m, tty.size()) {
                        quit = true;
                    }
                    if k == Key::CtrlC && !typing {
                        // behave like Ctrl+C in a normal terminal: stop the whole pipeline
                        drop(tty);
                        stop.store(true, Ordering::SeqCst);
                        unsafe { libc::kill(0, libc::SIGINT) };
                        std::process::exit(130);
                    }
                }
            }
        }
        let tick = m.running() && last_draw.elapsed() >= Duration::from_millis(500);
        if key_pressed || tick || (dirty && last_draw.elapsed() >= Duration::from_millis(40)) {
            draw(&mut tty, &mut ui, &m, source);
            last_draw = Instant::now();
            dirty = false;
        }
    }

    stop.store(true, Ordering::SeqCst);
    tty.restore();
    if drain_on_quit && !m.eof {
        // keep draining so the producer isn't killed by EPIPE and -o/--mmd stay complete
        eprintln!("trace_block: browser closed — still consuming the stream (Ctrl+C to stop)…");
        for x in rx.iter() {
            match x {
                Input::Line(raw) => {
                    lineno += 1;
                    if let Some(Ok(ev)) = on_line(&raw) {
                        m.event(&ev, lineno);
                        on_event(&ev, false);
                    }
                }
                Input::Eof => break,
                Input::Key(_) => {}
            }
        }
        m.finish();
        on_event(&Value::Null, true);
        let calls: usize = m.tool_stats.iter().map(|t| t.1).sum();
        eprintln!(
            "trace_block: stream {} · {} turns · {calls} tool calls · {} tokens",
            if m.settled {
                "settled"
            } else {
                "ended without agent_settled"
            },
            m.turn(),
            m.usage.total
        );
    }
    Ok(())
}

/// Returns true to quit.
fn visible(ui: &Ui, c: &Cell) -> bool {
    !(ui.hide_thinking && c.kind == Kind::Thinking) && !(ui.hide_backend && c.kind == Kind::Response)
}

/// Next (fwd) / previous visible cell after `from`.
fn step(ui: &Ui, m: &Model, from: usize, fwd: bool) -> Option<usize> {
    if fwd {
        (from + 1..m.cells.len()).find(|&i| visible(ui, &m.cells[i]))
    } else {
        (0..from).rev().find(|&i| visible(ui, &m.cells[i]))
    }
}

/// `i` if visible, else the nearest visible cell (forward first).
fn visible_near(ui: &Ui, m: &Model, i: usize) -> usize {
    match m.cells.get(i) {
        Some(c) if visible(ui, c) => i,
        Some(_) => step(ui, m, i, true).or_else(|| step(ui, m, i, false)).unwrap_or(i),
        None => i,
    }
}

/// Move the list selection to the next/previous cell matching `q`.
fn list_search(ui: &mut Ui, m: &Model, q: &str, fwd: bool, include_current: bool) {
    if m.cells.is_empty() {
        return;
    }
    let cur_matches =
        include_current && visible(ui, &m.cells[ui.sel]) && !find_all(&haystack(&m.cells[ui.sel]), q).is_empty();
    let found = if cur_matches {
        Some((ui.sel, false))
    } else {
        search_cells(ui, m, ui.sel, q, fwd)
    };
    match found {
        Some((i, wrapped)) => {
            ui.follow = false;
            ui.sel = i;
            ui.reveal = Reveal::Match;
            let (pos, total) = match_rank(ui, m, i, q);
            let wrap_note = if wrapped {
                if fwd {
                    " · wrapped to top"
                } else {
                    " · wrapped to bottom"
                }
            } else {
                ""
            };
            ui.msg = Some((format!("/{q} · cell {pos}/{total}{wrap_note}"), false));
        }
        None => ui.msg = Some((format!("not found: {q}"), true)),
    }
}

/// `[`: first visible cell of this turn, or of the previous turn when already there.
fn prev_turn(ui: &Ui, m: &Model, sel: usize) -> usize {
    let f = visible_near(ui, m, turn_start(m, sel, true));
    if f < sel {
        return f;
    }
    // already at this turn's first visible cell (its raw first may be a hidden thinking cell)
    let turn = m.cells.get(sel).map(|c| c.turn).unwrap_or(0);
    let raw_first = m.cells.iter().position(|c| c.turn == turn).unwrap_or(0);
    visible_near(ui, m, turn_start(m, raw_first, true))
}

fn handle_key(k: Key, ui: &mut Ui, m: &Model, size: (usize, usize)) -> bool {
    let (cols, rows) = size;
    let cw = cols.saturating_sub(3).max(20);
    let n = m.cells.len();
    let last = n.saturating_sub(1);
    let page = (rows / 2).max(1);
    ui.msg = None;
    if ui.help {
        ui.help = false;
        return false;
    }
    if k == Key::Char('?') && ui.input.is_none() {
        ui.help = true;
        return false;
    }
    // typing a search term after `/`
    if let Some(buf) = &mut ui.input {
        match k {
            Key::Char(c) => buf.push(c),
            Key::Backspace => {
                if buf.pop().is_none() {
                    ui.input = None;
                }
            }
            Key::Esc | Key::CtrlC => ui.input = None,
            Key::Enter => {
                let q = ui.input.take().unwrap_or_default();
                if !q.is_empty() {
                    ui.query = Some(q.clone());
                    ui.dmatch = None;
                    if ui.detail.is_some() {
                        detail_search(ui, m, cw, true);
                    } else {
                        list_search(ui, m, &q, true, true);
                    }
                }
            }
            _ => {}
        }
        return false;
    }
    if k == Key::Char('/') {
        ui.input = Some(String::new());
        return false;
    }
    if let Some(di) = ui.detail {
        match k {
            Key::Char('n') => detail_search(ui, m, cw, true),
            Key::Char('N') => detail_search(ui, m, cw, false),
            Key::Char('h') | Key::Left | Key::Esc | Key::Char('q') => ui.detail = None,
            Key::Char('j') | Key::Down => ui.dscroll += 1,
            Key::Char('k') | Key::Up => ui.dscroll = ui.dscroll.saturating_sub(1),
            Key::CtrlD | Key::Char(' ') => ui.dscroll += page,
            Key::CtrlU => ui.dscroll = ui.dscroll.saturating_sub(page),
            Key::Char('g') => ui.dscroll = 0,
            Key::Char('G') => ui.dscroll = usize::MAX / 2,
            Key::Char('J') => {
                let ni = (di + 1).min(last);
                ui.detail = Some(ni);
                ui.sel = ni;
                ui.dscroll = 0;
                ui.dmatch = None;
                ui.follow = false;
            }
            Key::Char('K') => {
                let ni = di.saturating_sub(1);
                ui.detail = Some(ni);
                ui.sel = ni;
                ui.dscroll = 0;
                ui.dmatch = None;
                ui.follow = false;
            }
            _ => {}
        }
        if ui.detail.is_none() {
            ui.reveal = Reveal::Cell;
        }
        return false;
    }
    ui.reveal = Reveal::Cell;
    match k {
        Key::Char('q') => return true,
        Key::Char('n') | Key::Char('N') => match ui.query.clone() {
            Some(q) => list_search(ui, m, &q, k == Key::Char('n'), false),
            None => ui.msg = Some(("no search term — press / first".into(), true)),
        },
        Key::Char('j') | Key::Down => match step(ui, m, ui.sel, true) {
            Some(i) => ui.sel = i,
            None => ui.follow = true,
        },
        Key::Char('k') | Key::Up => {
            ui.follow = false;
            if let Some(i) = step(ui, m, ui.sel, false) {
                ui.sel = i;
            }
        }
        Key::Char('t') => ui.hide_thinking = !ui.hide_thinking,
        Key::Char('b') => ui.hide_backend = !ui.hide_backend,
        Key::Char('l') | Key::Right | Key::Enter if n > 0 => {
            ui.follow = false;
            ui.detail = Some(ui.sel);
            ui.dscroll = 0;
            ui.dmatch = None;
            // with an active search, open at the first match inside the cell
            if ui.query.is_some() {
                detail_search(ui, m, cw, true);
            }
        }
        Key::CtrlD | Key::Char(' ') => {
            ui.follow = false;
            ui.sel = (ui.sel + 5).min(last);
        }
        Key::CtrlU => {
            ui.follow = false;
            ui.sel = ui.sel.saturating_sub(5);
        }
        Key::Char('g') => {
            ui.follow = false;
            ui.sel = 0;
        }
        Key::Char('G') => ui.follow = true,
        Key::Char('a') => {
            if let Some(i) = m.final_idx {
                ui.follow = false;
                ui.sel = i;
            }
        }
        Key::Char('f') => ui.follow = !ui.follow,
        Key::Char('[') => {
            ui.follow = false;
            ui.sel = prev_turn(ui, m, ui.sel);
        }
        Key::Char(']') => {
            ui.follow = false;
            ui.sel = visible_near(ui, m, turn_start(m, ui.sel, false));
        }
        _ => {}
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        let mut k = Vec::new();
        parse_keys(b"jk\x1b[A\x1b\r\x04q", &mut k);
        assert_eq!(
            k,
            vec![
                Key::Char('j'),
                Key::Char('k'),
                Key::Up,
                Key::Esc,
                Key::Enter,
                Key::CtrlD,
                Key::Char('q')
            ]
        );
    }

    #[test]
    fn fit_ansi() {
        assert_eq!(strip_ansi(&fit("\x1b[1mhello world\x1b[0m", 5)), "hello");
    }

    #[test]
    fn model_from_fixture_and_render() {
        let mut m = Model::new();
        let data = fixture_or_skip!("sirius-trace.jsonl");
        for (i, l) in data.lines().enumerate() {
            m.event(&serde_json::from_str(l).unwrap(), i + 1);
        }
        let tools = m.cells.iter().filter(|c| c.kind == Kind::Tool).count();
        assert_eq!(tools, 17);
        assert_eq!(m.tools_available, vec!["read", "bash", "edit", "write"]);
        let tools = m.cells.iter().find(|c| c.kind == Kind::Tools).expect("tools cell");
        assert_eq!(tools.title, "system message · toolsAdded (4)");
        assert!(tools.body.contains("\"parameters\"") && tools.body.contains("\"command\""));
        let lines = cell_lines(tools, &m, 100);
        assert_eq!(lines.len(), 5, "header + one line per tool: {lines:?}");
        assert!(m.settled);
        let mi = m.model.as_ref().unwrap();
        assert_eq!(
            mi.full(),
            "opencode/nemotron-3-ultra-free · openai-completions · thinking medium"
        );
        assert!(m.cells[0].body.contains("model opencode/nemotron-3-ultra-free"));
        assert_eq!(m.turns[0].model, "nemotron-3-ultra-free");
        let mut m2 = Model::new();
        m2.event(
            &serde_json::json!({"type":"session","id":"x","cwd":"/","timestamp":"t"}),
            1,
        );
        m2.event(&serde_json::json!({"type":"message_end","message":{"role":"assistant","provider":"nchc","model":"Gemma","api":"a","responseModel":"openai/gpt-oss-120b","content":[],"usage":{}}}), 2);
        assert!(m2.cells[0].body.contains("served by openai/gpt-oss-120b"));
        let fi = m.final_idx.expect("final answer found");
        assert_eq!(m.cells[fi].kind, Kind::Answer);
        assert!(m.cells[fi].final_answer && m.cells[fi].body.contains("Sirius"));
        assert_eq!(m.final_turn, Some(m.turn()));
        assert_eq!(m.cells.iter().filter(|c| c.final_answer).count(), 1);
        let resp: Vec<&Cell> = m.cells.iter().filter(|c| c.kind == Kind::Response).collect();
        assert!(resp.len() >= 21, "one backend cell per LLM call, got {}", resp.len());
        assert!(resp.iter().any(|c| c.title.contains("stop toolUse")));
        assert!(
            resp.iter()
                .any(|c| c.status == Status::Err && c.title.contains("stop error"))
        );
        assert!(resp[0].body.contains("\"provider\": \"opencode\""));
        // the detail is the assistant message exactly as recorded, content included
        assert!(
            resp[0].body.contains("\"content\"") && !resp[0].body.contains("ttft") && !resp[0].body.contains("request")
        );
        assert!(
            m.cells
                .iter()
                .any(|c| c.kind == Kind::Answer && c.body.contains("Sirius"))
        );
        for c in &m.cells {
            let _ = cell_lines(c, &m, 80);
            let _ = detail_lines(c, &m, 80);
        }
        // [ / ] land on turn boundaries
        let t5 = turn_start(&m, 0, false);
        assert!(m.cells[t5].turn > m.cells[0].turn);
    }
}

#[cfg(test)]
mod turn_nav {
    use super::*;

    #[test]
    fn bracket_back_lands_on_turn_starts() {
        let mut m = Model::new();
        for (i, l) in fixture_or_skip!("sirius-trace.jsonl").lines().enumerate() {
            m.event(&serde_json::from_str(l).unwrap(), i + 1);
        }
        for sel in 0..m.cells.len() {
            let f = turn_start(&m, sel, true);
            let first = f == 0 || m.cells[f - 1].turn != m.cells[f].turn;
            assert!(first, "sel {sel} -> {f} is not the first cell of its turn");
        }
    }
}

#[cfg(test)]
mod hide_thinking {
    use super::*;

    #[test]
    fn navigation_skips_hidden_thinking() {
        let mut m = Model::new();
        for (i, l) in fixture_or_skip!("sirius-trace.jsonl").lines().enumerate() {
            m.event(&serde_json::from_str(l).unwrap(), i + 1);
        }
        let ui = Ui {
            sel: 0,
            follow: false,
            top: 0,
            detail: None,
            dscroll: 0,
            hide_thinking: true,
            hide_backend: false,
            cache: HashMap::new(),
            input: None,
            query: None,
            msg: None,
            dmatch: None,
            reveal: Reveal::Cell,
            help: false,
        };
        // j walks only non-thinking cells
        let mut i = 0;
        while let Some(n) = step(&ui, &m, i, true) {
            assert_ne!(m.cells[n].kind, Kind::Thinking);
            i = n;
        }
        // [ from the end reaches turn 1 without getting stuck
        let mut sel = m.cells.len() - 1;
        let mut turns_seen = vec![m.cells[sel].turn];
        for _ in 0..50 {
            let n = prev_turn(&ui, &m, sel);
            assert_ne!(m.cells[n].kind, Kind::Thinking);
            if n == sel {
                break;
            }
            sel = n;
            turns_seen.push(m.cells[sel].turn);
        }
        assert!(turns_seen.windows(2).all(|w| w[1] <= w[0]));
        assert!(*turns_seen.last().unwrap() <= 1, "reached {:?}", turns_seen.last());
    }
}

#[cfg(test)]
mod search {
    use super::*;

    fn fixture() -> Option<Model> {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/sirius-trace.jsonl");
        let Ok(data) = std::fs::read_to_string(path) else {
            eprintln!("skipped: fixtures/sirius-trace.jsonl not present");
            return None;
        };
        let mut m = Model::new();
        for (i, l) in data.lines().enumerate() {
            m.event(&serde_json::from_str(l).unwrap(), i + 1);
        }
        Some(m)
    }

    fn ui() -> Ui {
        Ui {
            sel: 0,
            follow: false,
            top: 0,
            detail: None,
            dscroll: 0,
            hide_thinking: false,
            hide_backend: false,
            cache: HashMap::new(),
            input: None,
            query: None,
            msg: None,
            dmatch: None,
            reveal: Reveal::Cell,
            help: false,
        }
    }

    #[test]
    fn smartcase_and_cjk() {
        assert_eq!(find_all("Sirius BLACK sirius", "sirius"), vec![(0, 6), (13, 19)]);
        assert_eq!(find_all("Sirius sirius", "Sirius"), vec![(0, 6)]);
        assert_eq!(find_all("曹騰的養子是誰", "養子"), vec![(3, 5)]);
    }

    #[test]
    fn utf8_and_backspace_keys() {
        let mut k = Vec::new();
        parse_keys("/曹騰\x7f\r".as_bytes(), &mut k);
        assert_eq!(
            k,
            vec![
                Key::Char('/'),
                Key::Char('曹'),
                Key::Char('騰'),
                Key::Backspace,
                Key::Enter
            ]
        );
    }

    #[test]
    fn highlight_survives_ansi() {
        let line = "\x1b[1mhello\x1b[0m world hello";
        let out = highlight_matches(line, "hello");
        assert_eq!(strip_ansi(&out), "hello world hello");
        assert!(out.matches("\x1b[7m").count() >= 2);
    }

    #[test]
    fn typing_slash_then_lower_and_upper_n() {
        let Some(m) = fixture() else { return };
        let mut u = ui();
        let size = (100, 40);
        for k in [
            Key::Char('/'),
            Key::Char('p'),
            Key::Char('e'),
            Key::Char('t'),
            Key::Char('t'),
            Key::Char('i'),
            Key::Enter,
        ] {
            handle_key(k, &mut u, &m, size);
        }
        assert_eq!(u.query.as_deref(), Some("petti"));
        let first = u.sel;
        assert!(haystack(&m.cells[first]).to_lowercase().contains("petti"));
        handle_key(Key::Char('n'), &mut u, &m, size);
        let second = u.sel;
        assert!(second != first && haystack(&m.cells[second]).to_lowercase().contains("petti"));
        handle_key(Key::Char('N'), &mut u, &m, size);
        assert_eq!(u.sel, first);
        // not found
        for k in [
            Key::Char('/'),
            Key::Char('z'),
            Key::Char('q'),
            Key::Char('x'),
            Key::Enter,
        ] {
            handle_key(k, &mut u, &m, size);
        }
        assert!(u.msg.as_ref().is_some_and(|(t, err)| *err && t.contains("not found")));
        assert_eq!(u.sel, first);
    }

    #[test]
    fn detail_search_moves_between_lines() {
        let Some(m) = fixture() else { return };
        let mut u = ui();
        let tool = m
            .cells
            .iter()
            .position(|c| c.kind == Kind::Tool && c.body.matches("Sirius").count() > 3)
            .unwrap();
        u.sel = tool;
        u.query = Some("Sirius".into());
        handle_key(Key::Char('l'), &mut u, &m, (100, 40));
        let a = u.dmatch.expect("opened at first match");
        handle_key(Key::Char('n'), &mut u, &m, (100, 40));
        let b = u.dmatch.unwrap();
        assert!(b > a);
        handle_key(Key::Char('N'), &mut u, &m, (100, 40));
        assert_eq!(u.dmatch, Some(a));
    }
}

#[cfg(test)]
mod status_row {
    use super::*;

    #[test]
    fn never_bare_keys() {
        for w in [30, 45, 60, 80, 100, 140, 220] {
            let row = strip_ansi(&key_help_row(LIST_KEYS, w, true));
            assert!(str_width(&row) <= w, "width {w}: {row:?}");
            // every shown item is "key description"
            for item in row.trim().split(" · ") {
                assert!(item.contains(' '), "bare key at width {w}: {item:?} in {row:?}");
            }
            let all_fit = LIST_KEYS.iter().all(|(k, d)| row.contains(&format!("{k} {d}")));
            assert!(all_fit || row.trim_end().ends_with("? all keys"), "width {w}: {row:?}");
        }
    }

    #[test]
    fn question_mark_opens_and_any_key_closes_help() {
        let m = Model::new();
        let mut u = Ui {
            sel: 0,
            follow: false,
            top: 0,
            detail: None,
            dscroll: 0,
            hide_thinking: false,
            hide_backend: false,
            cache: HashMap::new(),
            input: None,
            query: None,
            msg: None,
            dmatch: None,
            reveal: Reveal::Cell,
            help: false,
        };
        handle_key(Key::Char('?'), &mut u, &m, (80, 24));
        assert!(u.help);
        let screen = help_screen(77)
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("next/prev match") && screen.contains("final answer") && screen.contains("J/K"));
        handle_key(Key::Char('j'), &mut u, &m, (80, 24));
        assert!(!u.help);
    }
}

#[cfg(test)]
mod claude_stream {
    use super::*;

    fn load(data: &str) -> Model {
        let mut m = Model::new();
        for (i, l) in data.lines().enumerate() {
            m.event(&serde_json::from_str(l).unwrap(), i + 1);
        }
        m
    }

    fn count(m: &Model, k: Kind) -> usize {
        m.cells.iter().filter(|c| c.kind == k).count()
    }

    #[test]
    fn partial_messages_stream() {
        let m = load(&fixture_or_skip!("claude-sirius.jsonl"));
        // numbers below are counted from the fixture itself
        assert_eq!(m.turns.len(), 5, "one TURN per assistant message id");
        assert_eq!(count(&m, Kind::Thinking), 5);
        assert_eq!(count(&m, Kind::Answer), 2);
        assert_eq!(count(&m, Kind::Tool), 4);
        assert_eq!(count(&m, Kind::Response), 5);
        let stops: Vec<Option<String>> = m.turns.iter().map(|t| t.stop.clone()).collect();
        assert_eq!(
            stops[..4].iter().filter(|s| s.as_deref() == Some("tool_use")).count(),
            4
        );
        assert_eq!(stops[4].as_deref(), Some("end_turn"));
        assert_eq!(
            m.tool_stats.iter().map(|t| t.2).sum::<usize>(),
            1,
            "one tool_result with is_error"
        );
        assert!(
            m.cells
                .iter()
                .filter(|c| c.kind == Kind::Tool)
                .all(|c| c.status != Status::Running)
        );
        assert!(
            m.cells
                .iter()
                .filter(|c| c.kind == Kind::Tool)
                .all(|c| !c.args_full.is_empty() && serde_json::from_str::<Value>(&c.args_full).is_ok())
        );
        let fi = m.final_idx.expect("end_turn message marks the final answer");
        assert!(m.cells[fi].body.starts_with("Based on the Harry Potter files"));
        assert!(m.settled);
        assert!(
            m.cells
                .iter()
                .any(|c| c.kind == Kind::Summary && c.title.starts_with("result success"))
        );
        assert!(
            m.cells
                .iter()
                .any(|c| c.title == "system/thinking_tokens · estimated_tokens 137 (delta 87)")
        );
        assert!(
            m.cells
                .iter()
                .any(|c| c.kind == Kind::Summary && c.title.contains("thinking_tokens "))
        );
        assert_eq!(m.model.as_ref().unwrap().model, "claude-haiku-4-5-20251001");
        assert!(m.tools_available.len() > 10 && m.tools_available.iter().any(|t| t == "Grep"));
        for c in &m.cells {
            let _ = cell_lines(c, &m, 90);
            let _ = detail_lines(c, &m, 90);
        }
    }

    #[test]
    fn without_partial_messages() {
        let m = load(&fixture_or_skip!("claude-nopartial.jsonl"));
        assert_eq!(m.turns.len(), 3);
        assert_eq!(count(&m, Kind::Tool), 2);
        assert_eq!(count(&m, Kind::Answer), 1);
        assert_eq!(count(&m, Kind::Response), 3);
        // no message_delta in the stream → the stop reason is reported as absent, not guessed
        assert!(m.turns.iter().all(|t| t.stop.as_deref() == Some("(not in stream)")));
        assert!(
            m.cells
                .iter()
                .any(|c| c.kind == Kind::Other && c.status == Status::Err && c.title.starts_with("permission denied"))
        );
        assert_eq!(m.tool_stats.iter().map(|t| t.2).sum::<usize>(), 2);
        // final answer found by exact match with result.result
        let fi = m.final_idx.expect("answer equal to result text");
        assert!(m.cells[fi].body.contains("doesn't exist"));
    }
}

/// Append-only rendering of the cell model for `--scroll` on streams term.rs does not know
/// (Claude Code stream-json): each cell is printed once it is complete.
pub struct CellPrinter {
    m: Model,
    printed: usize,
    cur_turn: usize,
    color: bool,
    out: Box<dyn Write>,
}

impl CellPrinter {
    pub fn new(color: bool, out: Box<dyn Write>) -> CellPrinter {
        CellPrinter {
            m: Model::new(),
            printed: 0,
            cur_turn: 0,
            color,
            out,
        }
    }

    pub fn event(&mut self, ev: &Value, lineno: usize) {
        self.m.event(ev, lineno);
        self.flush(false);
    }

    pub fn finish(&mut self) {
        self.m.finish();
        self.flush(true);
    }

    fn emit(&mut self, s: &str) {
        let s = if self.color {
            s.to_string()
        } else {
            strip_ansi(&plain_badges(s))
        };
        let _ = self.out.write_all(s.as_bytes());
        let _ = self.out.write_all(b"\n");
    }

    fn flush(&mut self, all: bool) {
        let w = terminal_size::terminal_size_of(std::io::stderr())
            .map(|(w, _)| w.0 as usize)
            .unwrap_or(100);
        let cw = w.saturating_sub(3).max(20);
        while self.printed < self.m.cells.len() {
            let i = self.printed;
            let c = &self.m.cells[i];
            let done = all
                || if c.kind == Kind::Tool {
                    c.status != Status::Running
                } else {
                    i + 1 < self.m.cells.len()
                };
            if !done {
                break;
            }
            let mut lines = Vec::new();
            if c.turn != self.cur_turn {
                if self.cur_turn > 0
                    && let Some(f) = turn_footer(&self.m, self.cur_turn)
                {
                    lines.push(f);
                    lines.push(String::new());
                }
                if c.turn > 0 {
                    lines.push(String::new());
                    lines.push(turn_header(&self.m, c.turn, cw));
                }
                self.cur_turn = c.turn;
            }
            lines.extend(cell_lines(c, &self.m, cw));
            lines.push(String::new());
            for l in lines {
                self.emit(&l);
            }
            self.printed += 1;
        }
        if all
            && self.cur_turn > 0
            && let Some(f) = turn_footer(&self.m, self.cur_turn)
        {
            self.emit(&f);
        }
        let _ = self.out.flush();
    }
}

/// Plain-text form of the colored badges (same as the scroll view): tool `[name]`, model `<name>`.
fn plain_badges(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    loop {
        let tool = rest.find("\x1b[1;30;4");
        let model = rest.find("\x1b[1;97;45m");
        let (at, is_model) = match (tool, model) {
            (Some(t), Some(m)) if m < t => (m, true),
            (Some(t), _) => (t, false),
            (None, Some(m)) => (m, true),
            (None, None) => break,
        };
        let Some(m_end) = rest[at..].find('m').map(|i| at + i + 1) else {
            break;
        };
        let Some(close) = rest[m_end..].find("\x1b[0m").map(|i| m_end + i) else {
            break;
        };
        out.push_str(&rest[..at]);
        let name = rest[m_end..close].trim();
        out.push_str(&if is_model {
            format!("<{name}>")
        } else {
            format!("[{name}]")
        });
        rest = &rest[close + 4..];
    }
    out.push_str(rest);
    out
}
