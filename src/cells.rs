//! Event stream → navigable "cells" (one per visible block) for the interactive TUI.

use crate::term::Usage;
use crate::util::{args_summary, offered_tools, pretty_if_json, result_text};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Session,
    System,
    /// tool definitions (JSON schemas) disclosed to the model in a system message
    Tools,
    User,
    Thinking,
    Answer,
    Tool,
    Retry,
    ModelError,
    /// backend metadata of one LLM call (response id, raw stop reason, tokens, timing, diagnostics)
    Response,
    Summary,
    Unfinished,
    Malformed,
    Other,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Status {
    None,
    Running,
    Ok,
    Err,
}

pub struct Cell {
    pub kind: Kind,
    pub turn: usize,
    /// one-line headline (tool args summary, session id, event type, …)
    pub title: String,
    /// full content (thinking/answer text, tool output, system prompt, …)
    pub body: String,
    pub status: Status,
    pub tool: Option<String>,
    /// tool args, pretty JSON
    pub args_full: String,
    pub started: Instant,
    pub secs: Option<f64>,
    pub partial_bytes: usize,
    /// image blocks of the content as (media type, base64), in placeholder order
    pub images: Vec<(String, String)>,
    /// the run's final answer (answer text of the turn that ended with stop, not toolUse)
    pub final_answer: bool,
    /// input line number of the event that created the cell
    pub lineno: usize,
    /// bumped on every change (render cache key)
    pub ver: u64,
}

impl Cell {
    fn new(kind: Kind, turn: usize, lineno: usize) -> Cell {
        Cell {
            kind,
            turn,
            title: String::new(),
            body: String::new(),
            status: Status::None,
            tool: None,
            args_full: String::new(),
            started: Instant::now(),
            secs: None,
            partial_bytes: 0,
            images: Vec::new(),
            final_answer: false,
            lineno,
            ver: 0,
        }
    }
}

#[derive(Default, Clone, PartialEq)]
pub struct ModelInfo {
    pub provider: String,
    pub model: String,
    pub api: String,
    pub thinking: String,
    /// concrete model the provider reported when it differs from the requested one (pi's responseModel)
    pub served: String,
    /// provider base URL from ~/.pi/agent/models.json
    pub endpoint: String,
}

impl ModelInfo {
    /// `opencode/nemotron-3-ultra-free · openai-completions · thinking medium`
    pub fn full(&self) -> String {
        let mut s = if self.provider.is_empty() {
            self.model.clone()
        } else {
            format!("{}/{}", self.provider, self.model)
        };
        if !self.api.is_empty() {
            s += &format!(" · {}", self.api);
        }
        if !self.thinking.is_empty() {
            s += &format!(" · thinking {}", self.thinking);
        }
        if !self.served.is_empty() {
            s += &format!(" · served by {}", self.served);
        }
        s
    }
}

/// `name(req, opt?, …) — description` for a tool definition.
pub fn tool_signature(t: &Value) -> (String, String, String) {
    let name = t["name"].as_str().unwrap_or("?").to_string();
    let required: Vec<&str> = t["parameters"]["required"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let params: Vec<String> = t["parameters"]["properties"]
        .as_object()
        .map(|o| {
            o.keys()
                .map(|k| {
                    if required.contains(&k.as_str()) {
                        k.clone()
                    } else {
                        format!("{k}?")
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let desc = t["description"]
        .as_str()
        .unwrap_or("")
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    (name, params.join(", "), desc)
}

/// provider → baseUrl from pi's models.json (built-in providers have none).
pub fn load_endpoints() -> HashMap<String, String> {
    let mut out = HashMap::new();
    let dir = std::env::var("PI_CODING_AGENT_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".pi/agent"));
    let Ok(text) = std::fs::read_to_string(dir.join("models.json")) else {
        return out;
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return out;
    };
    let providers = if v["providers"].is_object() {
        &v["providers"]
    } else {
        &v
    };
    if let Some(obj) = providers.as_object() {
        for (name, p) in obj {
            if let Some(url) = p["baseUrl"].as_str() {
                out.insert(name.clone(), url.to_string());
            }
        }
    }
    out
}

/// One-line backend summary of an assistant message (shared by the TUI and the scroll view).
pub fn response_summary(m: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(id) = m["responseId"].as_str() {
        parts.push(crate::util::trunc(id, 22));
    }
    if let Some(sm) = m["responseModel"].as_str() {
        parts.push(format!("served by {sm}"));
    }
    let stop = m["stopReason"].as_str().unwrap_or("?");
    match m["rawStopReason"].as_str() {
        Some(raw) if raw != stop => parts.push(format!("stop {stop} ({raw})")),
        _ => parts.push(format!("stop {stop}")),
    }
    if m["endTurn"] == true {
        parts.push("endTurn".into());
    }
    let u = &m["usage"];
    let g = |k: &str| u[k].as_u64().unwrap_or(0);
    if g("input") + g("output") > 0 {
        let mut t = format!("in {} / out {}", g("input"), g("output"));
        if g("reasoning") > 0 {
            t += &format!(" (reasoning {})", g("reasoning"));
        }
        if g("cacheRead") > 0 {
            t += &format!(" · cache {}", g("cacheRead"));
        }
        parts.push(t);
    }
    if let Some(cost) = u["cost"]["total"].as_f64().filter(|c| *c > 0.0) {
        parts.push(format!("${cost:.4}"));
    }
    if let Some(e) = m["providerThinkingLevel"].as_str() {
        parts.push(format!("effort {e}"));
    }
    if let Some(d) = m["diagnostics"].as_array().filter(|d| !d.is_empty()) {
        parts.push(format!("⚠ {} diagnostics", d.len()));
    }
    parts.join(" · ")
}

#[derive(Default)]
pub struct TurnInfo {
    /// seconds since the stream started
    pub at: f64,
    pub stop: Option<String>,
    pub usage: Option<(u64, u64)>,
    pub secs: Option<f64>,
    pub tools: Vec<String>,
    /// model that answered this turn
    pub model: String,
    started: Option<Instant>,
}

pub struct Model {
    pub cells: Vec<Cell>,
    pub turns: Vec<TurnInfo>,
    pub tools_available: Vec<String>,
    pub model: Option<ModelInfo>,
    session_cell: Option<usize>,
    endpoints: HashMap<String, String>,
    /// input is a saved session log (pi `~/.pi/agent/sessions`, Claude Code `~/.claude/projects`),
    /// not a live event stream: no deltas, no completion marker
    pub session_log: bool,
    /// events come from a file read at once (`browse`), not arriving live
    pub from_file: bool,
    /// per tool: (name, calls, failures)
    pub tool_stats: Vec<(String, usize, usize)>,
    pub usage: Usage,
    pub retries: usize,
    pub settled: bool,
    pub eof: bool,
    /// cell index of the final answer, once known
    pub final_idx: Option<usize>,
    /// turn that produced the final answer
    pub final_turn: Option<usize>,
    by_call: HashMap<String, usize>,
    stream: Option<usize>,
    /// Claude Code stream-json: per assistant message id
    cmsgs: HashMap<String, ClaudeMsg>,
    /// message ids in arrival order
    cmsg_order: Vec<String>,
    open_retry: Option<usize>,
    started: Instant,
}

impl Model {
    pub fn new() -> Model {
        Model {
            cells: Vec::new(),
            turns: Vec::new(),
            tools_available: Vec::new(),
            model: None,
            session_cell: None,
            endpoints: load_endpoints(),
            session_log: false,
            from_file: false,
            tool_stats: Vec::new(),
            usage: Usage::default(),
            retries: 0,
            settled: false,
            eof: false,
            final_idx: None,
            final_turn: None,
            by_call: HashMap::new(),
            stream: None,
            cmsgs: HashMap::new(),
            cmsg_order: Vec::new(),
            open_retry: None,
            started: Instant::now(),
        }
    }

    /// Wall-clock timings (tool durations, turn clocks) only mean something for a live stream.
    pub fn timed(&self) -> bool {
        !self.session_log && !self.from_file
    }

    pub fn turn(&self) -> usize {
        self.turns.len()
    }

    pub fn elapsed(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    fn push(&mut self, kind: Kind, lineno: usize) -> usize {
        let c = Cell::new(kind, self.turn(), lineno);
        self.cells.push(c);
        self.cells.len() - 1
    }

    fn touch(&mut self, i: usize) {
        self.cells[i].ver += 1;
    }

    pub fn malformed(&mut self, lineno: usize, err: &str) {
        let i = self.push(Kind::Malformed, lineno);
        self.cells[i].title = format!("line {lineno}: malformed JSON passed through");
        self.cells[i].body = err.to_string();
    }

    pub fn finish(&mut self) {
        self.eof = true;
        if let Some(id) = self.cmsg_order.last().cloned() {
            self.claude_finalize(&id);
        }
        if self.session_log && !self.settled {
            let n = self.cells.last().map(|c| c.lineno).unwrap_or(0);
            let i = self.push(Kind::Other, n);
            self.cells[i].title = "end of session log (session logs record no completion marker)".into();
            return;
        }
        if !self.settled {
            let n = self.cells.last().map(|c| c.lineno).unwrap_or(0);
            let i = self.push(Kind::Unfinished, n);
            self.cells[i].title = "stream ended without agent_settled — run killed or unfinished".into();
        }
    }

    fn response_cell(&mut self, m: &Value, lineno: usize) {
        let i = self.push(Kind::Response, lineno);
        let c = &mut self.cells[i];
        c.title = response_summary(m);
        if m["stopReason"] == "error" {
            c.status = Status::Err;
        }
        // the assistant message exactly as recorded
        c.body = serde_json::to_string_pretty(m).unwrap_or_default();
    }

    /// Mark the answer cells of `turn` as the final answer.
    fn mark_final(&mut self, turn: usize) {
        let mut last = None;
        for (i, c) in self.cells.iter_mut().enumerate() {
            if c.kind == Kind::Answer && c.turn == turn && !c.body.trim().is_empty() {
                c.final_answer = true;
                c.ver += 1;
                last = Some(i);
            }
        }
        if let Some(i) = last {
            // an earlier provisional mark (e.g. a later retry produced the real answer) is replaced
            if let Some(old) = self.final_idx.filter(|&o| self.cells[o].turn != turn) {
                self.cells[old].final_answer = false;
                self.cells[old].ver += 1;
            }
            self.final_idx = Some(i);
            self.final_turn = Some(turn);
        }
    }

    /// pi system message: prompt sections + tool definitions (stream `message_start` or a session-log entry).
    fn system_message(&mut self, m: &Value, lineno: usize) {
        self.tools_available = offered_tools(m);
        let i = self.push(Kind::System, lineno);
        self.cells[i].title = format!("system prompt · {} tools", self.tools_available.len());
        let mut body = String::new();
        if let Some(sec) = m["sections"].as_object() {
            for (k, v) in sec {
                body += &format!("── {k} ──\n{}\n\n", v.as_str().unwrap_or(""));
            }
        }
        self.cells[i].body = body.trim_end().to_string();
        if let Some(added) = m["toolsAdded"].as_array().filter(|a| !a.is_empty()) {
            let t = self.push(Kind::Tools, lineno);
            self.cells[t].title = format!("system message · toolsAdded ({})", added.len());
            self.cells[t].body = serde_json::to_string_pretty(added).unwrap_or_default();
        }
        if let Some(removed) = m["toolsRemoved"].as_array().filter(|a| !a.is_empty()) {
            let t = self.push(Kind::Tools, lineno);
            let names: Vec<String> = removed
                .iter()
                .map(|r| r["name"].as_str().map(String::from).unwrap_or_else(|| r.to_string()))
                .collect();
            self.cells[t].title = format!("tools removed: {}", names.join(", "));
            self.cells[t].body = serde_json::to_string_pretty(removed).unwrap_or_default();
        }
    }

    /// One recorded message of a pi session log.
    fn pi_session_message(&mut self, m: &Value, lineno: usize) {
        self.session_log = true;
        match m["role"].as_str().unwrap_or("") {
            "system" => self.system_message(m, lineno),
            "user" => {
                let text = &crate::util::content_text(&m["content"]);
                let i = self.push(Kind::User, lineno);
                self.cells[i].body = text.to_string();
                self.cells[i].images = crate::util::collect_images(&m["content"]);
            }
            "assistant" => {
                // each assistant message is one LLM call → one turn
                self.observe_model(m, lineno);
                let model = self.model.as_ref().map(|mi| mi.model.clone()).unwrap_or_default();
                self.turns.push(TurnInfo {
                    at: self.elapsed(),
                    model,
                    ..Default::default()
                });
                let turn = self.turn();
                for b in m["content"].as_array().into_iter().flatten() {
                    match b["type"].as_str().unwrap_or("") {
                        "thinking" => {
                            let i = self.push(Kind::Thinking, lineno);
                            self.cells[i].body = b["thinking"].as_str().unwrap_or("").to_string();
                        }
                        "text" => {
                            let i = self.push(Kind::Answer, lineno);
                            self.cells[i].body = b["text"].as_str().unwrap_or("").to_string();
                        }
                        "toolCall" => {
                            let name = b["name"].as_str().unwrap_or("?").to_string();
                            let i = self.push(Kind::Tool, lineno);
                            let c = &mut self.cells[i];
                            c.tool = Some(name.clone());
                            c.status = Status::Running;
                            c.title = args_summary(&b["arguments"]);
                            c.args_full = serde_json::to_string_pretty(&b["arguments"]).unwrap_or_default();
                            if let Some(id) = b["id"].as_str() {
                                self.by_call.insert(id.to_string(), i);
                            }
                            if let Some(t) = self.turns.last_mut() {
                                t.tools.push(name.clone());
                            }
                            match self.tool_stats.iter_mut().find(|t| t.0 == name) {
                                Some(t) => t.1 += 1,
                                None => self.tool_stats.push((name, 1, 0)),
                            }
                        }
                        _ => {}
                    }
                }
                self.usage.add(&m["usage"]);
                if m["stopReason"] == "error" {
                    let i = self.push(Kind::ModelError, lineno);
                    self.cells[i].title = m["errorMessage"].as_str().unwrap_or("unknown error").to_string();
                }
                self.response_cell(m, lineno);
                if let Some(t) = self.turns.last_mut() {
                    t.stop = m["stopReason"].as_str().map(String::from);
                    let (i, o) = (
                        m["usage"]["input"].as_u64().unwrap_or(0),
                        m["usage"]["output"].as_u64().unwrap_or(0),
                    );
                    if i + o > 0 {
                        t.usage = Some((i, o));
                    }
                }
                let stop = m["stopReason"].as_str().unwrap_or("");
                if !matches!(stop, "toolUse" | "error" | "aborted" | "") {
                    self.mark_final(turn);
                }
            }
            "toolResult" => {
                let Some(&i) = m["toolCallId"].as_str().and_then(|id| self.by_call.get(id)) else {
                    return;
                };
                let err = m["isError"].as_bool().unwrap_or(false);
                let c = &mut self.cells[i];
                c.status = if err { Status::Err } else { Status::Ok };
                c.body = pretty_if_json(&result_text(m));
                c.images = crate::util::collect_images(&m["content"]);
                c.secs = None; // no execution timing is recorded in a session log
                if err && let Some(t) = self.tool_stats.iter_mut().find(|t| Some(&t.0) == c.tool.as_ref()) {
                    t.2 += 1;
                }
                self.touch(i);
            }
            other => {
                let i = self.push(Kind::Other, lineno);
                self.cells[i].title = format!("message ({other})");
                self.cells[i].body = serde_json::to_string_pretty(m).unwrap_or_default();
            }
        }
    }

    /// Track provider/model/api/thinking from assistant messages; note changes mid-run.
    fn observe_model(&mut self, m: &Value, _lineno: usize) {
        let g = |k: &str| m[k].as_str().unwrap_or("").to_string();
        let mut info = self.model.clone().unwrap_or_default();
        for (field, key) in [
            (&mut info.provider, "provider"),
            (&mut info.model, "model"),
            (&mut info.api, "api"),
            (&mut info.thinking, "thinkingLevel"),
            (&mut info.served, "responseModel"),
        ] {
            let v = g(key);
            if !v.is_empty() {
                *field = v;
            }
        }
        if info.model.is_empty() {
            return;
        }
        if let Some(url) = self.endpoints.get(&info.provider) {
            info.endpoint = url.clone();
        }
        if self.model.as_ref() == Some(&info) {
            return;
        }
        if let Some(t) = self.turns.last_mut() {
            t.model = info.model.clone();
        }
        if let Some(si) = self.session_cell {
            let body = &mut self.cells[si].body;
            let kept: Vec<&str> = body.lines().filter(|l| !l.starts_with("model")).collect();
            let kept: Vec<&str> = kept.into_iter().filter(|l| !l.starts_with("backend")).collect();
            *body = format!("{}\nmodel {}", kept.join("\n"), info.full());
            if !info.endpoint.is_empty() {
                *body += &format!(
                    "\nbackend {}  (baseUrl for provider {} in ~/.pi/agent/models.json)",
                    info.endpoint, info.provider
                );
            }
            self.cells[si].ver += 1;
        }
        self.model = Some(info);
    }

    pub fn event(&mut self, ev: &Value, lineno: usize) {
        let ty = ev["type"].as_str().unwrap_or("");
        if is_claude_event(ev) {
            return self.claude_event(ev, lineno);
        }
        match ty {
            "session" => {
                let i = self.push(Kind::Session, lineno);
                let g = |k: &str| ev[k].as_str().unwrap_or("?").to_string();
                self.cells[i].title = format!("pi session {}", g("id"));
                self.cells[i].body = format!("cwd  {}\ntime {}", g("cwd"), g("timestamp"));
                self.session_cell = Some(i);
            }
            "turn_start" => {
                let model = self.model.as_ref().map(|m| m.model.clone()).unwrap_or_default();
                self.turns.push(TurnInfo {
                    at: self.elapsed(),
                    started: Some(Instant::now()),
                    model,
                    ..Default::default()
                });
                self.stream = None;
            }
            "turn_end" => {
                let m = &ev["message"];
                let timed = self.timed();
                if let Some(t) = self.turns.last_mut() {
                    t.stop = m["stopReason"].as_str().map(String::from);
                    let (i, o) = (
                        m["usage"]["input"].as_u64().unwrap_or(0),
                        m["usage"]["output"].as_u64().unwrap_or(0),
                    );
                    if i + o > 0 {
                        t.usage = Some((i, o));
                    }
                    t.secs = if timed {
                        t.started.map(|s| s.elapsed().as_secs_f64())
                    } else {
                        None
                    };
                }
                let stop = m["stopReason"].as_str().unwrap_or("");
                if !matches!(stop, "toolUse" | "error" | "aborted" | "") {
                    self.mark_final(self.turn());
                }
            }
            "message_start" => {
                let m = &ev["message"];
                if m["role"] == "assistant" {
                    self.observe_model(m, lineno);
                }
                if m["role"] == "system" {
                    self.system_message(m, lineno);
                }
            }
            "message_update" => {
                let ame = &ev["assistantMessageEvent"];
                let sub = ame["type"].as_str().unwrap_or("");
                let kind = match sub {
                    "thinking_start" | "thinking_delta" => Kind::Thinking,
                    "text_start" | "text_delta" => Kind::Answer,
                    _ => return,
                };
                let reuse = self
                    .stream
                    .filter(|&i| self.cells[i].kind == kind && !sub.ends_with("_start"));
                let i = match reuse {
                    Some(i) => i,
                    None => self.push(kind, lineno),
                };
                self.stream = Some(i);
                if let Some(d) = ame["delta"].as_str() {
                    self.cells[i].body.push_str(d);
                }
                self.touch(i);
            }
            "message_end" => {
                let m = &ev["message"];
                match m["role"].as_str().unwrap_or("") {
                    "user" => {
                        let text = &crate::util::content_text(&m["content"]);
                        let i = self.push(Kind::User, lineno);
                        self.cells[i].body = text.to_string();
                        self.cells[i].images = crate::util::collect_images(&m["content"]);
                    }
                    "assistant" => {
                        self.usage.add(&m["usage"]);
                        self.observe_model(m, lineno);
                        if m["stopReason"] == "error" {
                            let i = self.push(Kind::ModelError, lineno);
                            self.cells[i].title = m["errorMessage"].as_str().unwrap_or("unknown error").to_string();
                        }
                        self.response_cell(m, lineno);
                    }
                    _ => {}
                }
            }
            "tool_execution_start" => {
                let name = ev["toolName"].as_str().unwrap_or("?").to_string();
                let i = self.push(Kind::Tool, lineno);
                let c = &mut self.cells[i];
                c.title = args_summary(&ev["args"]);
                c.args_full = serde_json::to_string_pretty(&ev["args"]).unwrap_or_default();
                c.status = Status::Running;
                c.tool = Some(name.clone());
                if let Some(id) = ev["toolCallId"].as_str() {
                    self.by_call.insert(id.to_string(), i);
                }
                if let Some(t) = self.turns.last_mut() {
                    t.tools.push(name.clone());
                }
                match self.tool_stats.iter_mut().find(|t| t.0 == name) {
                    Some(t) => t.1 += 1,
                    None => self.tool_stats.push((name, 1, 0)),
                }
                self.stream = None;
            }
            "tool_execution_update" => {
                if let Some(&i) = ev["toolCallId"].as_str().and_then(|id| self.by_call.get(id)) {
                    self.cells[i].partial_bytes = result_text(&ev["partialResult"]).len();
                    self.touch(i);
                }
            }
            "tool_execution_end" => {
                let Some(&i) = ev["toolCallId"].as_str().and_then(|id| self.by_call.get(id)) else {
                    return;
                };
                let err = ev["isError"].as_bool().unwrap_or(false);
                let timed = self.timed();
                let c = &mut self.cells[i];
                c.status = if err { Status::Err } else { Status::Ok };
                c.body = pretty_if_json(&result_text(&ev["result"]));
                c.images = crate::util::collect_images(&ev["result"]["content"]);
                c.secs = timed.then(|| c.started.elapsed().as_secs_f64());
                if err && let Some(t) = self.tool_stats.iter_mut().find(|t| Some(&t.0) == c.tool.as_ref()) {
                    t.2 += 1;
                }
                self.touch(i);
            }
            "auto_retry_start" => {
                self.retries += 1;
                let i = self.push(Kind::Retry, lineno);
                self.cells[i].title = format!(
                    "retry {}/{} in {:.1}s",
                    ev["attempt"],
                    ev["maxAttempts"],
                    ev["delayMs"].as_f64().unwrap_or(0.0) / 1000.0
                );
                self.cells[i].body = ev["errorMessage"].as_str().unwrap_or("").to_string();
                self.cells[i].status = Status::Running;
                self.open_retry = Some(i);
            }
            "auto_retry_end" => {
                if let Some(i) = self.open_retry.take() {
                    let ok = ev["success"].as_bool().unwrap_or(false);
                    self.cells[i].status = if ok { Status::Ok } else { Status::Err };
                    self.touch(i);
                }
            }
            "agent_settled" => {
                self.settled = true;
                let i = self.push(Kind::Summary, lineno);
                self.cells[i].title = format!("settled in {}", crate::util::human_dur(self.elapsed()));
            }
            "agent_start" | "agent_end" | "queue_update" | "bash_execution_update" | "entry_appended" => {}
            // pi session log (~/.pi/agent/sessions): complete messages, no deltas
            "message" => self.pi_session_message(&ev["message"], lineno),
            "model_change" | "thinking_level_change" => {
                let i = self.push(Kind::Other, lineno);
                let pick = |k: &str| ev[k].as_str().map(String::from);
                let what = pick("model")
                    .or_else(|| pick("modelId"))
                    .map(|mo| match pick("provider") {
                        Some(p) => format!("{p}/{mo}"),
                        None => mo,
                    })
                    .or_else(|| {
                        pick("thinkingLevel")
                            .or_else(|| pick("level"))
                            .map(|l| format!("thinking {l}"))
                    })
                    .unwrap_or_else(|| crate::util::trunc(&serde_json::to_string(ev).unwrap_or_default(), 80));
                self.cells[i].title = format!("{} → {what}", ty.replace('_', " "));
                self.cells[i].body = serde_json::to_string_pretty(ev).unwrap_or_default();
            }
            other => {
                let i = self.push(Kind::Other, lineno);
                self.cells[i].title = if other.is_empty() {
                    "(untyped event)".into()
                } else {
                    other.to_string()
                };
                self.cells[i].body = serde_json::to_string_pretty(ev).unwrap_or_default();
            }
        }
    }

    /// Any tool still running (its elapsed timer needs redrawing).
    pub fn running(&self) -> bool {
        self.cells
            .iter()
            .any(|c| c.status == Status::Running && c.kind == Kind::Tool)
    }
}

/// Claude Code event: a stream-json event type pi never emits, or any transcript entry (Claude Code
/// transcripts carry a camelCase `sessionId`; pi session entries never do).
pub fn is_claude_event(ev: &Value) -> bool {
    ev["sessionId"].is_string()
        || matches!(
            ev["type"].as_str(),
            Some(
                "system"
                | "assistant"
                | "user"
                | "stream_event"
                | "result"
                | "rate_limit_event"
                // transcript-only entry types (~/.claude/projects/*/<session>.jsonl)
                | "attachment"
                | "queue-operation"
                | "atis-latch"
                | "last-prompt"
                | "cost-state"
                | "summary"
            )
        )
}

/// Per-message bookkeeping for Claude Code streams.
#[derive(Default)]
pub struct ClaudeMsg {
    turn: usize,
    /// cell index per content-block index
    blocks: Vec<Option<usize>>,
    /// number of `assistant` events seen (each carries one block)
    assistant_events: usize,
    /// recorded events for the ⇄ cell: assistant events + the message_delta stream event
    recorded: Vec<Value>,
    stop_reason: Option<String>,
    response_cell: Option<usize>,
    /// message arrived via stream events (message_start … message_stop)
    streamed: bool,
}

fn claude_text(content: &Value) -> String {
    crate::util::content_text(content)
}

impl Model {
    fn claude_msg(&mut self, id: &str, model: Option<&str>) -> usize {
        if !self.cmsgs.contains_key(id) {
            // a new assistant message = a new API request: finish the previous one first
            if let Some(prev) = self.cmsg_order.last().cloned() {
                self.claude_finalize(&prev);
            }
            self.turns.push(TurnInfo {
                at: self.elapsed(),
                started: Some(Instant::now()),
                model: model.unwrap_or("").to_string(),
                ..Default::default()
            });
            self.cmsgs.insert(
                id.to_string(),
                ClaudeMsg {
                    turn: self.turns.len(),
                    ..Default::default()
                },
            );
            self.cmsg_order.push(id.to_string());
            self.stream = None;
        }
        self.cmsgs[id].turn
    }

    /// Push the ⇄ cell of a message once (after its blocks).
    fn claude_finalize(&mut self, id: &str) {
        let Some(cm) = self.cmsgs.get(id) else { return };
        if cm.response_cell.is_some() || cm.recorded.is_empty() {
            return;
        }
        let recorded = Value::Array(cm.recorded.clone());
        let stop = cm.stop_reason.clone();
        let turn = cm.turn;
        // usage: the message_delta's if streamed, else the last assistant event's
        let usage = cm
            .recorded
            .iter()
            .rev()
            .find_map(|e| {
                e["event"]["usage"]
                    .as_object()
                    .cloned()
                    .or_else(|| e["message"]["usage"].as_object().cloned())
            })
            .map(Value::Object)
            .unwrap_or(Value::Null);
        let lineno = self.cells.last().map(|c| c.lineno).unwrap_or(0);
        let c = Cell::new(Kind::Response, turn, lineno);
        self.cells.push(c);
        let i = self.cells.len() - 1;
        let g = |k: &str| usage[k].as_u64().unwrap_or(0);
        let mut parts = vec![
            crate::util::trunc(id, 22),
            format!("stop {}", stop.as_deref().unwrap_or("(not in stream)")),
        ];
        parts.push(format!("in {} / out {}", g("input_tokens"), g("output_tokens")));
        if g("cache_read_input_tokens") > 0 {
            parts.push(format!("cache read {}", g("cache_read_input_tokens")));
        }
        if g("cache_creation_input_tokens") > 0 {
            parts.push(format!("cache write {}", g("cache_creation_input_tokens")));
        }
        self.cells[i].title = parts.join(" · ");
        self.cells[i].body = serde_json::to_string_pretty(&recorded).unwrap_or_default();
        let timed = self.timed();
        if let Some(t) = self.turns.get_mut(turn.saturating_sub(1)) {
            t.stop = stop.clone().or_else(|| Some("(not in stream)".into()));
            t.usage = Some((g("input_tokens"), g("output_tokens")));
            t.secs = if timed {
                t.started.map(|s| s.elapsed().as_secs_f64())
            } else {
                None
            };
        }
        if let Some(cm) = self.cmsgs.get_mut(id) {
            cm.response_cell = Some(i);
        }
        if stop.as_deref() == Some("end_turn") {
            self.mark_final(turn);
        }
    }

    /// Create the cell for content block `index` of message `id`.
    fn claude_block(&mut self, id: &str, index: usize, block: &Value, lineno: usize) -> usize {
        let turn = self.cmsgs[id].turn;
        let kind = match block["type"].as_str().unwrap_or("") {
            "thinking" | "redacted_thinking" => Kind::Thinking,
            "text" => Kind::Answer,
            "tool_use" | "server_tool_use" => Kind::Tool,
            _ => Kind::Other,
        };
        let mut c = Cell::new(kind, turn, lineno);
        match kind {
            Kind::Tool => {
                let name = block["name"].as_str().unwrap_or("?").to_string();
                c.tool = Some(name.clone());
                c.status = Status::Running;
                if block["input"].as_object().is_some_and(|o| !o.is_empty()) {
                    c.title = args_summary(&block["input"]);
                    c.args_full = serde_json::to_string_pretty(&block["input"]).unwrap_or_default();
                }
                if let Some(t) = self.turns.get_mut(turn.saturating_sub(1)) {
                    t.tools.push(name.clone());
                }
                match self.tool_stats.iter_mut().find(|t| t.0 == name) {
                    Some(t) => t.1 += 1,
                    None => self.tool_stats.push((name, 1, 0)),
                }
            }
            Kind::Thinking => c.body = block["thinking"].as_str().unwrap_or("").to_string(),
            Kind::Answer => c.body = block["text"].as_str().unwrap_or("").to_string(),
            _ => {
                c.title = format!("content block {}", block["type"].as_str().unwrap_or("?"));
                c.body = serde_json::to_string_pretty(block).unwrap_or_default();
            }
        }
        self.cells.push(c);
        let i = self.cells.len() - 1;
        if kind == Kind::Tool
            && let Some(tid) = block["id"].as_str()
        {
            self.by_call.insert(tid.to_string(), i);
        }
        let cm = self.cmsgs.get_mut(id).unwrap();
        if cm.blocks.len() <= index {
            cm.blocks.resize(index + 1, None);
        }
        cm.blocks[index] = Some(i);
        i
    }

    fn claude_event(&mut self, ev: &Value, lineno: usize) {
        let ty = ev["type"].as_str().unwrap_or("");
        let sub = ev["subtype"].as_str().unwrap_or("");
        // transcript entries (camelCase `sessionId`) vs stream-json events (`session_id`)
        if let Some(sid) = ev["sessionId"].as_str() {
            self.session_log = true;
            if self.session_cell.is_none() {
                let i = self.push(Kind::Session, lineno);
                self.cells[i].title = format!("claude session {sid} (transcript)");
                self.session_cell = Some(i);
            }
            // cwd/version/gitBranch are not on every entry: take them from the first one that has them
            if let Some(si) = self.session_cell
                && self.cells[si].body.is_empty()
                && ev["cwd"].is_string()
            {
                let g = |k: &str| ev[k].as_str().unwrap_or("?").to_string();
                self.cells[si].body = format!(
                    "cwd  {}\nversion {}\ngitBranch {}",
                    g("cwd"),
                    g("version"),
                    g("gitBranch")
                );
                self.cells[si].ver += 1;
            }
        }
        match (ty, sub) {
            ("attachment", _) => {
                let a = &ev["attachment"];
                let at = a["type"].as_str().unwrap_or("?");
                match at {
                    // the full system prompt the CLI sent, as recorded
                    "prompt_snapshot" => {
                        let i = self.push(Kind::System, lineno);
                        let parts: Vec<String> = a["systemPrompt"]
                            .as_array()
                            .map(|v| {
                                v.iter()
                                    .map(|p| p.as_str().map(String::from).unwrap_or_else(|| p.to_string()))
                                    .collect()
                            })
                            .unwrap_or_default();
                        self.cells[i].title =
                            format!("attachment/prompt_snapshot · system prompt ({} parts)", parts.len());
                        self.cells[i].body = parts.join("\n\n── next part ──\n\n");
                    }
                    _ => {
                        if at == "model"
                            && let Some(mid) = a["identity"]["modelId"].as_str()
                        {
                            self.observe_model(&serde_json::json!({ "model": mid }), lineno);
                        }
                        let i = self.push(Kind::Other, lineno);
                        self.cells[i].title = format!("attachment/{at}");
                        self.cells[i].body = serde_json::to_string_pretty(ev).unwrap_or_default();
                    }
                }
            }
            ("queue-operation" | "atis-latch" | "last-prompt" | "cost-state" | "summary", _) => {
                let i = self.push(Kind::Other, lineno);
                self.cells[i].title = match ev["operation"].as_str() {
                    Some(op) => format!("{ty} · {op}"),
                    None => ty.to_string(),
                };
                self.cells[i].body = serde_json::to_string_pretty(ev).unwrap_or_default();
            }
            ("system", "init") => {
                let i = self.push(Kind::Session, lineno);
                let g = |k: &str| ev[k].as_str().unwrap_or("?").to_string();
                self.cells[i].title = format!("claude session {}", g("session_id"));
                self.cells[i].body = format!(
                    "cwd  {}\nclaude_code_version {}\npermissionMode {}\napiKeySource {}",
                    g("cwd"),
                    g("claude_code_version"),
                    g("permissionMode"),
                    g("apiKeySource")
                );
                self.session_cell = Some(i);
                self.tools_available = ev["tools"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|t| t.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let t = self.push(Kind::Tools, lineno);
                self.cells[t].title = format!("system/init · tools ({})", self.tools_available.len());
                self.cells[t].body = serde_json::to_string_pretty(ev).unwrap_or_default();
                if let Some(model) = ev["model"].as_str() {
                    self.observe_model(&serde_json::json!({ "model": model }), lineno);
                }
            }
            ("system", "status") => {}
            ("system", _) => {
                let i = self.push(Kind::Other, lineno);
                let c = &mut self.cells[i];
                c.title = match sub {
                    "permission_denied" => {
                        c.status = Status::Err;
                        format!(
                            "permission denied · {} · {}",
                            ev["tool_name"].as_str().unwrap_or("?"),
                            ev["message"].as_str().unwrap_or("")
                        )
                    }
                    "hook_started" | "hook_response" => format!(
                        "system/{sub} · {}{}",
                        ev["hook_name"].as_str().unwrap_or("?"),
                        ev["outcome"].as_str().map(|o| format!(" · {o}")).unwrap_or_default()
                    ),
                    // recorded fields, shown as-is
                    "thinking_tokens" => format!(
                        "system/thinking_tokens · estimated_tokens {} (delta {})",
                        ev["estimated_tokens"], ev["estimated_tokens_delta"]
                    ),
                    _ => format!("system/{sub}"),
                };
                c.body = serde_json::to_string_pretty(ev).unwrap_or_default();
            }
            ("rate_limit_event", _) => {
                let i = self.push(Kind::Other, lineno);
                let info = &ev["rate_limit_info"];
                self.cells[i].title = format!(
                    "rate_limit_event · {} · {}",
                    info["status"].as_str().unwrap_or("?"),
                    info["rateLimitType"].as_str().unwrap_or("?")
                );
                self.cells[i].body = serde_json::to_string_pretty(ev).unwrap_or_default();
            }
            ("stream_event", _) => {
                let e = &ev["event"];
                let cur = self.cmsg_order.last().cloned();
                match e["type"].as_str().unwrap_or("") {
                    "message_start" => {
                        let id = e["message"]["id"].as_str().unwrap_or("?").to_string();
                        self.claude_msg(&id, e["message"]["model"].as_str());
                        self.cmsgs.get_mut(&id).unwrap().streamed = true;
                    }
                    "content_block_start" => {
                        let Some(id) = cur else { return };
                        let index = e["index"].as_u64().unwrap_or(0) as usize;
                        let i = self.claude_block(&id, index, &e["content_block"], lineno);
                        self.stream = Some(i);
                    }
                    "content_block_delta" => {
                        let Some(id) = cur else { return };
                        let index = e["index"].as_u64().unwrap_or(0) as usize;
                        let Some(i) = self.cmsgs[&id].blocks.get(index).copied().flatten() else {
                            return;
                        };
                        let d = &e["delta"];
                        let c = &mut self.cells[i];
                        match d["type"].as_str().unwrap_or("") {
                            "text_delta" => c.body.push_str(d["text"].as_str().unwrap_or("")),
                            "thinking_delta" => c.body.push_str(d["thinking"].as_str().unwrap_or("")),
                            "input_json_delta" => {
                                // raw partial JSON as it streams; replaced by the parsed input later
                                c.args_full.push_str(d["partial_json"].as_str().unwrap_or(""));
                                c.title = c.args_full.clone();
                            }
                            _ => {}
                        }
                        self.touch(i);
                    }
                    "message_delta" => {
                        let Some(id) = cur else { return };
                        let cm = self.cmsgs.get_mut(&id).unwrap();
                        cm.stop_reason = e["delta"]["stop_reason"].as_str().map(String::from);
                        cm.recorded.push(ev.clone());
                    }
                    "message_stop" => {
                        if let Some(id) = cur {
                            self.claude_finalize(&id);
                        }
                    }
                    _ => {}
                }
            }
            ("assistant", _) => {
                let m = &ev["message"];
                let id = m["id"].as_str().unwrap_or("?").to_string();
                self.claude_msg(&id, m["model"].as_str());
                if let Some(model) = m["model"].as_str() {
                    self.observe_model(&serde_json::json!({ "model": model }), lineno);
                }
                let index = self.cmsgs[&id].assistant_events;
                self.cmsgs.get_mut(&id).unwrap().assistant_events += 1;
                if let Some(sr) = m["stop_reason"].as_str() {
                    self.cmsgs.get_mut(&id).unwrap().stop_reason = Some(sr.to_string());
                }
                self.cmsgs.get_mut(&id).unwrap().recorded.push(ev.clone());
                let block = &m["content"][0];
                let existing = self.cmsgs[&id].blocks.get(index).copied().flatten();
                let i = match existing {
                    Some(i) => i,
                    None => self.claude_block(&id, index, block, lineno),
                };
                // transcripts record stop_reason on each entry: an answer block of an end_turn message
                // is the final answer
                if block["type"] == "text" && self.cmsgs[&id].stop_reason.as_deref() == Some("end_turn") {
                    let turn = self.cmsgs[&id].turn;
                    self.cells[i].body = block["text"].as_str().unwrap_or("").to_string();
                    self.mark_final(turn);
                }
                // the assistant event carries the complete block: it is authoritative
                let c = &mut self.cells[i];
                match block["type"].as_str().unwrap_or("") {
                    "thinking" => c.body = block["thinking"].as_str().unwrap_or("").to_string(),
                    "text" => c.body = block["text"].as_str().unwrap_or("").to_string(),
                    "tool_use" | "server_tool_use" => {
                        c.title = args_summary(&block["input"]);
                        c.args_full = serde_json::to_string_pretty(&block["input"]).unwrap_or_default();
                    }
                    _ => {}
                }
                self.touch(i);
            }
            ("user", _) => {
                // tool results can arrive before the stream's message_delta; a streamed message is
                // finished at message_stop, a non-streamed one here
                if let Some(id) = self.cmsg_order.last().cloned()
                    && !self.cmsgs[&id].streamed
                {
                    self.claude_finalize(&id);
                }
                let content = &ev["message"]["content"];
                for b in content.as_array().into_iter().flatten() {
                    if b["type"] != "tool_result" {
                        continue;
                    }
                    let Some(&i) = b["tool_use_id"].as_str().and_then(|t| self.by_call.get(t)) else {
                        continue;
                    };
                    let err = b["is_error"].as_bool().unwrap_or(false);
                    let live = self.timed();
                    let c = &mut self.cells[i];
                    c.status = if err { Status::Err } else { Status::Ok };
                    c.body = pretty_if_json(&claude_text(&b["content"]));
                    c.images = crate::util::collect_images(&b["content"]);
                    c.secs = live.then(|| c.started.elapsed().as_secs_f64());
                    if err && let Some(t) = self.tool_stats.iter_mut().find(|t| Some(&t.0) == c.tool.as_ref()) {
                        t.2 += 1;
                    }
                    self.touch(i);
                }
                // a user message that is not tool results (e.g. an injected prompt)
                if !content
                    .as_array()
                    .is_some_and(|a| a.iter().any(|b| b["type"] == "tool_result"))
                {
                    let i = self.push(Kind::User, lineno);
                    self.cells[i].body = claude_text(content);
                    self.cells[i].images = crate::util::collect_images(content);
                }
            }
            ("result", _) => {
                if let Some(id) = self.cmsg_order.last().cloned() {
                    self.claude_finalize(&id);
                }
                self.settled = true;
                let u = &ev["usage"];
                let g = |k: &str| u[k].as_u64().unwrap_or(0);
                self.usage = Usage {
                    input: g("input_tokens"),
                    output: g("output_tokens"),
                    cache_read: g("cache_read_input_tokens"),
                    total: g("input_tokens")
                        + g("output_tokens")
                        + g("cache_read_input_tokens")
                        + g("cache_creation_input_tokens"),
                };
                // final answer: the answer cell whose text is exactly the recorded result text
                if self.final_idx.is_none()
                    && let Some(res) = ev["result"].as_str()
                    && let Some(t) = self
                        .cells
                        .iter()
                        .rev()
                        .find(|c| c.kind == Kind::Answer && c.body.trim() == res.trim())
                        .map(|c| c.turn)
                {
                    self.mark_final(t);
                }
                let i = self.push(Kind::Summary, lineno);
                self.cells[i].title = format!(
                    "result {} · {} ms · {} turns · ${:.4}{}",
                    sub,
                    ev["duration_ms"].as_u64().unwrap_or(0),
                    ev["num_turns"].as_u64().unwrap_or(0),
                    ev["total_cost_usd"].as_f64().unwrap_or(0.0),
                    if ev["is_error"] == true { " · is_error" } else { "" }
                );
                if let Some(t) = ev["usage"]["output_tokens_details"]["thinking_tokens"].as_u64() {
                    self.cells[i].title += &format!(" · thinking_tokens {t}");
                }
                self.cells[i].body = serde_json::to_string_pretty(ev).unwrap_or_default();
            }
            _ => {
                let i = self.push(Kind::Other, lineno);
                self.cells[i].title = if sub.is_empty() {
                    ty.to_string()
                } else {
                    format!("{ty}/{sub}")
                };
                self.cells[i].body = serde_json::to_string_pretty(ev).unwrap_or_default();
            }
        }
    }
}
