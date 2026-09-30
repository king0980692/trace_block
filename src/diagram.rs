//! Incrementally builds a Mermaid sequence diagram from the pi event stream.

use crate::term::Usage;
use crate::util::{args_summary, human_dur, one_line, result_text, trunc};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Instant;

enum ToolState {
    Running,
    Done { ok: bool, secs: f64, lines: usize },
}

enum Item {
    Prompt(String),
    Thinking(String),
    Tool {
        part: usize,
        args: String,
        state: ToolState,
        started: Instant,
    },
    Retry {
        attempt: String,
        max: String,
        msg: String,
        ok: Option<bool>,
    },
    ModelError(String),
    Answer(String),
    Settled(String),
    Unfinished,
}

pub struct Diagram {
    model: String,
    items: Vec<Item>,
    /// tool participants in order of first use
    tools: Vec<String>,
    by_call: HashMap<String, usize>,
    open_retry: Option<usize>,
    stream_item: Option<usize>,
    turns: usize,
    tool_calls: usize,
    retries: usize,
    usage: Usage,
    started: Instant,
    settled: bool,
    max_label: usize,
}

/// Make free text safe for a Mermaid message/note label.
fn clean(s: &str, max: usize) -> String {
    let s: String = one_line(s)
        .chars()
        .map(|c| match c {
            ';' => ',',
            '#' => '№',
            '%' => '٪',
            '<' => '‹',
            '>' => '›',
            '{' => '(',
            '}' => ')',
            '"' => '\'',
            '`' => '\'',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let s = trunc(s.trim(), max);
    if s.is_empty() { "…".into() } else { s }
}

impl Diagram {
    pub fn new(max_label: usize) -> Self {
        Diagram {
            model: "Agent".into(),
            items: Vec::new(),
            tools: Vec::new(),
            by_call: HashMap::new(),
            open_retry: None,
            stream_item: None,
            turns: 0,
            tool_calls: 0,
            retries: 0,
            usage: Usage::default(),
            started: Instant::now(),
            settled: false,
            max_label,
        }
    }

    fn tool_part(&mut self, name: &str) -> usize {
        if let Some(i) = self.tools.iter().position(|t| t == name) {
            return i;
        }
        self.tools.push(name.to_string());
        self.tools.len() - 1
    }

    /// Apply one event. Returns true when the diagram changed.
    pub fn event(&mut self, ev: &Value) -> bool {
        let ty = ev["type"].as_str().unwrap_or("");
        match ty {
            "turn_start" => {
                self.turns += 1;
                self.stream_item = None;
                false
            }
            "message_start" => {
                let m = &ev["message"];
                if m["role"] == "assistant"
                    && let Some(model) = m["model"].as_str()
                {
                    self.model = model.to_string();
                }
                false
            }
            "message_update" => {
                let ame = &ev["assistantMessageEvent"];
                match ame["type"].as_str().unwrap_or("") {
                    "thinking_start" => {
                        self.items.push(Item::Thinking(String::new()));
                        self.stream_item = Some(self.items.len() - 1);
                        true
                    }
                    "text_start" => {
                        self.items.push(Item::Answer(String::new()));
                        self.stream_item = Some(self.items.len() - 1);
                        true
                    }
                    "thinking_delta" | "text_delta" => {
                        let delta = ame["delta"].as_str().unwrap_or("");
                        let want_text = ame["type"] == "text_delta";
                        let idx = match self.stream_item {
                            Some(i)
                                if matches!(
                                    (&self.items[i], want_text),
                                    (Item::Answer(_), true) | (Item::Thinking(_), false)
                                ) =>
                            {
                                i
                            }
                            _ => {
                                self.items.push(if want_text {
                                    Item::Answer(String::new())
                                } else {
                                    Item::Thinking(String::new())
                                });
                                self.items.len() - 1
                            }
                        };
                        self.stream_item = Some(idx);
                        let (Item::Answer(t) | Item::Thinking(t)) = &mut self.items[idx] else {
                            return false;
                        };
                        // only the label prefix is shown; stop growing once it is full
                        let full = t.chars().count() > self.max_label * 2;
                        if !full {
                            t.push_str(delta);
                        }
                        !full
                    }
                    _ => false,
                }
            }
            "message_end" => {
                let m = &ev["message"];
                match m["role"].as_str().unwrap_or("") {
                    "user" => {
                        let text = &crate::util::content_text(&m["content"]);
                        self.items.push(Item::Prompt(text.to_string()));
                        true
                    }
                    "assistant" => {
                        self.usage.add(&m["usage"]);
                        if m["stopReason"] == "error" {
                            let msg = m["errorMessage"].as_str().unwrap_or("error").to_string();
                            self.items.push(Item::ModelError(msg));
                            return true;
                        }
                        false
                    }
                    _ => false,
                }
            }
            "tool_execution_start" => {
                self.tool_calls += 1;
                let name = ev["toolName"].as_str().unwrap_or("tool").to_string();
                let part = self.tool_part(&name);
                self.items.push(Item::Tool {
                    part,
                    args: args_summary(&ev["args"]),
                    state: ToolState::Running,
                    started: Instant::now(),
                });
                if let Some(id) = ev["toolCallId"].as_str() {
                    self.by_call.insert(id.to_string(), self.items.len() - 1);
                }
                self.stream_item = None;
                true
            }
            "tool_execution_end" => {
                let Some(&idx) = ev["toolCallId"].as_str().and_then(|id| self.by_call.get(id)) else {
                    return false;
                };
                if let Item::Tool { state, started, .. } = &mut self.items[idx] {
                    *state = ToolState::Done {
                        ok: !ev["isError"].as_bool().unwrap_or(false),
                        secs: started.elapsed().as_secs_f64(),
                        lines: result_text(&ev["result"]).lines().count(),
                    };
                }
                true
            }
            "auto_retry_start" => {
                self.retries += 1;
                // the failed attempt's error note is redundant with the retry note
                if matches!(self.items.last(), Some(Item::ModelError(_))) {
                    self.items.pop();
                }
                self.items.push(Item::Retry {
                    attempt: ev["attempt"].to_string(),
                    max: ev["maxAttempts"].to_string(),
                    msg: ev["errorMessage"].as_str().unwrap_or("").to_string(),
                    ok: None,
                });
                self.open_retry = Some(self.items.len() - 1);
                true
            }
            "auto_retry_end" => {
                if let Some(i) = self.open_retry.take() {
                    if let Item::Retry { ok, .. } = &mut self.items[i] {
                        *ok = Some(ev["success"].as_bool().unwrap_or(false));
                    }
                    return true;
                }
                false
            }
            "agent_settled" => {
                self.settled = true;
                let u = &self.usage;
                self.items.push(Item::Settled(format!(
                    "settled in {} · {} turns · {} tools · {} retries · {} tok (in {} / out {})",
                    human_dur(self.started.elapsed().as_secs_f64()),
                    self.turns,
                    self.tool_calls,
                    self.retries,
                    u.total,
                    u.input,
                    u.output
                )));
                true
            }
            _ => false,
        }
    }

    pub fn eof(&mut self) -> bool {
        if self.settled {
            return false;
        }
        self.items.push(Item::Unfinished);
        true
    }

    /// Current Mermaid source.
    pub fn source(&self) -> String {
        let m = self.max_label;
        let mut s = String::from("sequenceDiagram\n    autonumber\n");
        s += "    participant U as User\n";
        s += &format!("    participant A as {}\n", clean(&self.model, 40));
        for (i, t) in self.tools.iter().enumerate() {
            s += &format!("    participant T{i} as {}\n", clean(t, 30));
        }
        for item in &self.items {
            match item {
                Item::Prompt(t) => s += &format!("    U->>A: {}\n", clean(t, m)),
                Item::Thinking(t) => s += &format!("    Note over A: thinking: {}\n", clean(t, m)),
                Item::Answer(t) => s += &format!("    A-->>U: {}\n", clean(t, m)),
                Item::Tool {
                    part,
                    args,
                    state,
                    started,
                } => {
                    s += &format!("    A->>T{part}: {}\n", clean(args, m));
                    match state {
                        ToolState::Running => {
                            s += &format!(
                                "    Note over T{part}: running {}\n",
                                human_dur(started.elapsed().as_secs_f64())
                            );
                        }
                        ToolState::Done { ok: true, secs, lines } => {
                            s += &format!("    T{part}-->>A: ok · {} · {lines} lines\n", human_dur(*secs));
                        }
                        ToolState::Done { ok: false, secs, lines } => {
                            s += &format!("    T{part}--xA: ERROR · {} · {lines} lines\n", human_dur(*secs));
                        }
                    }
                }
                Item::Retry { attempt, max, msg, ok } => {
                    let status = match ok {
                        None => "retrying",
                        Some(true) => "recovered",
                        Some(false) => "FAILED",
                    };
                    s += &format!("    Note over A: retry {attempt}/{max} {status} · {}\n", clean(msg, m));
                }
                Item::ModelError(msg) => s += &format!("    Note over A: model error · {}\n", clean(msg, m)),
                Item::Settled(t) => s += &format!("    Note over U,A: {}\n", clean(t, 200)),
                Item::Unfinished => s += "    Note over U,A: stream ended without agent_settled\n",
            }
        }
        s
    }

    /// True while some tool is running (its elapsed-time note needs refreshing).
    pub fn has_running(&self) -> bool {
        self.items.iter().any(|i| {
            matches!(
                i,
                Item::Tool {
                    state: ToolState::Running,
                    ..
                }
            )
        })
    }
}
