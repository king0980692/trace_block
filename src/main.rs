/// Test helper: the recorded traces in `fixtures/` are kept local (not in the repository);
/// tests that need them are skipped when the file is absent.
#[cfg(test)]
macro_rules! fixture_or_skip {
    ($name:expr) => {
        match std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/", $name)) {
            Ok(s) => s,
            Err(_) => {
                eprintln!("skipped: fixtures/{} not present", $name);
                return;
            }
        }
    };
}

mod cells;
mod diagram;
mod jsonhl;
mod md;
mod mermaid;
mod replay;
mod term;
mod textdia;
mod trajectory;
mod tui;
mod util;
mod view;

use anyhow::{Context, Result, bail};
use mermaid::{LiveWriter, Mermaid, Outputs, Proto};
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const USAGE: &str = "\
trace_block — transparent live tracer for AI coding-agent JSON event streams

USAGE:
  pi --mode json -p \"…\" | trace_block [OPTIONS]
  claude -p \"…\" --output-format stream-json --verbose --include-partial-messages | trace_block [OPTIONS]

The view goes to stderr (a full-screen browser when stderr is a terminal). The raw input is passed
to stdout byte-for-byte when stdout is a pipe/file (never to a terminal — it would bury the view);
save it with -o. Don't `| tee FILE` into the terminal; use `-o FILE` (or `| tee FILE >/dev/null`).

INTERACTIVE BROWSER (default when stderr is a terminal):
  j/k cell · l open (full content, colored JSON) · h back · / search · n/N next/prev match
  [ ] prev/next turn · g/G top/end (G follows live) · a final answer · t thinking on/off
  b backend cells on/off · f follow · ? all keys · q quit (keeps draining a live stream)
  --scroll           append-only scrolling view instead (also implied by --plain, -q, --inline,
                     or stderr not being a terminal)
  -i, --interactive  force the browser
  --no-thinking      start with thinking hidden (scroll view: never show it)
  --preview N        scroll view: tool result preview lines (default 8, 0 = hide results)

OUTPUT FILES:
  -o, --save FILE    the raw event stream, byte-for-byte (flushed per line)
  --trajectory FILE  every recorded message. FILE.md → readable Markdown; otherwise JSONL, byte-exact:
                     pi → the session line + every message_end line;
                     Claude Code → the system/init, assistant, user and result lines

LIVE MERMAID DIAGRAM (pi streams; user ↔ agent ↔ tools, rendered in-process with mmdr):
  --mmd PATH         keep PATH updated with the Mermaid source
  --svg PATH         keep PATH updated with the rendered SVG
  --png PATH         keep PATH updated with a rendered PNG
  --inline[=WHEN]    draw the diagram in the terminal (scroll view; stderr must be a TTY)
                     WHEN: end (default, at agent_settled/EOF) | turn (after every turn too)
  --proto P          inline protocol: auto (default) | sixel | kitty | iterm | text
  --inline-width PX  max inline image width in pixels (default: ~9px × columns, ≤1600)
  --theme NAME       modern (default) | default | dark | forest | neutral
  --label-width N    max characters per diagram label (default 60)

SUBCOMMANDS:
  trace_block browse FILE       the browser on a saved trace or a session log (~/.pi/agent/sessions,
                                ~/.claude/projects); follows the file while it grows
  trace_block view PATH.mmd     redraw the diagram in place whenever PATH.mmd changes (second pane)
  trace_block replay FILE       re-stream a recorded trace with realistic timing (testing)
  (each takes --help)

IMAGES (image blocks sent to / returned from the model):
  --images MODE      auto (default: ask the terminal) | kitty | sixel | iterm | off
                     draws them in the browser's detail view (l) and in the scroll view;
                     otherwise they are shown as [image · <type> · <size>] placeholders

OTHER:
  --plain            force plain output (no colors / cursor moves)
  -q, --quiet        no view on stderr (output files still written)
  -h, --help         this help
  -V, --version      print the version
";

#[derive(PartialEq)]
enum Inline {
    Off,
    End,
    Turn,
}

struct Args {
    outs: Outputs,
    inline: Inline,
    proto: Option<Proto>,
    inline_width: Option<u32>,
    theme: String,
    label_width: usize,
    plain: bool,
    images: String,
    trajectory: Option<PathBuf>,
    interactive: bool,
    scroll: bool,
    save: Option<PathBuf>,
    preview: usize,
    no_thinking: bool,
    quiet: bool,
}

fn parse_args() -> Result<Args> {
    let mut a = Args {
        outs: Outputs::default(),
        inline: Inline::Off,
        proto: None,
        inline_width: None,
        theme: "modern".into(),
        label_width: 60,
        plain: false,
        images: "auto".into(),
        trajectory: None,
        interactive: false,
        scroll: false,
        save: None,
        preview: 8,
        no_thinking: false,
        quiet: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let (flag, inline_val) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let mut val = |name: &str| -> Result<String> {
            match inline_val.clone() {
                Some(v) => Ok(v),
                None => it.next().with_context(|| format!("{name} needs a value")),
            }
        };
        match flag.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("trace_block {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--mmd" => a.outs.mmd = Some(PathBuf::from(val("--mmd")?)),
            "--svg" => a.outs.svg = Some(PathBuf::from(val("--svg")?)),
            "--png" => a.outs.png = Some(PathBuf::from(val("--png")?)),
            "--inline" => {
                a.inline = match inline_val.as_deref() {
                    None | Some("end") => Inline::End,
                    Some("turn") => Inline::Turn,
                    Some(v) => bail!("--inline: expected end|turn, got '{v}'"),
                }
            }
            "--proto" => {
                let v = val("--proto")?;
                a.proto = Some(Proto::parse(&v).with_context(|| format!("--proto: unknown '{v}'"))?);
            }
            "--inline-width" => a.inline_width = Some(val("--inline-width")?.parse().context("--inline-width")?),
            "--theme" => a.theme = val("--theme")?,
            "--label-width" => a.label_width = val("--label-width")?.parse().context("--label-width")?,
            "--plain" => a.plain = true,
            "--images" => a.images = val("--images")?,
            "-i" | "--interactive" => a.interactive = true,
            "--scroll" => a.scroll = true,
            "-o" | "--save" => a.save = Some(PathBuf::from(val("--save")?)),
            "--trajectory" => a.trajectory = Some(PathBuf::from(val("--trajectory")?)),
            "--preview" => a.preview = val("--preview")?.parse().context("--preview")?,
            "--no-thinking" => a.no_thinking = true,
            "-q" | "--quiet" => a.quiet = true,
            other => bail!("unknown argument '{other}' (see --help)"),
        }
    }
    Ok(a)
}

fn main() {
    if let Err(e) = run() {
        eprintln!("trace_block: {e:#}");
        std::process::exit(2);
    }
}

fn run() -> Result<()> {
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("view") {
        return view::run(&argv[2..]);
    }
    if argv.get(1).map(String::as_str) == Some("replay") {
        return replay::run(&argv[2..]);
    }
    if argv.get(1).map(String::as_str) == Some("browse") {
        return browse(&argv[2..]);
    }
    let args = parse_args()?;
    let tty = std::io::stderr().is_terminal() && !args.plain;
    let sink: Box<dyn Write> = if args.quiet {
        Box::new(std::io::sink())
    } else {
        Box::new(std::io::stderr())
    };
    let mut term = term::Term::new(tty, sink);
    term.preview = args.preview;
    term.show_thinking = !args.no_thinking;

    let mm = Mermaid::new(&args.theme)?;
    let want_diagram = args.outs.any() || (args.inline != Inline::Off && tty);
    let mut dia = diagram::Diagram::new(args.label_width);
    let writer = args
        .outs
        .any()
        .then(|| LiveWriter::spawn(mm.clone(), args.outs.clone(), Duration::from_millis(150)));

    // the interactive browser is the default on a terminal; --scroll/--plain/-q/--inline or a
    // redirected stderr fall back to the append-only scrolling view
    let interactive = args.interactive
        || (!args.scroll
            && !args.plain
            && !args.quiet
            && args.inline == Inline::Off
            && std::io::stderr().is_terminal()
            && std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/tty")
                .is_ok());
    let mut traj = match &args.trajectory {
        Some(p) => Some(trajectory::Trajectory::create(p)?),
        None => None,
    };
    if interactive {
        let (tx, rx) = mpsc::channel::<tui::Input>();
        let txs = tx.clone();
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin().lock();
            loop {
                let mut buf = Vec::new();
                match stdin.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if txs.send(tui::Input::Line(buf)).is_err() {
                            return;
                        }
                    }
                }
            }
            let _ = txs.send(tui::Input::Eof);
        });
        let mut stdout = std::io::stdout();
        let mut stdout_ok = !stdout.is_terminal();
        let mut save = match &args.save {
            Some(p) => Some(std::fs::File::create(p).with_context(|| format!("creating {}", p.display()))?),
            None => None,
        };
        let on_line = |raw: &[u8]| {
            if stdout_ok && (stdout.write_all(raw).is_err() || stdout.flush().is_err()) {
                stdout_ok = false;
            }
            if let Some(f) = &mut save
                && f.write_all(raw).and_then(|_| f.flush()).is_err()
            {
                save = None;
            }
            let parsed = parse_line(raw);
            if let (Some(t), Some(Ok(ev))) = (traj.as_mut(), parsed.as_ref()) {
                t.line(raw, ev);
            }
            parsed
        };
        let w = writer.as_ref();
        let on_event = |ev: &serde_json::Value, eof: bool| {
            let changed = if eof { dia.eof() } else { dia.event(ev) };
            if changed && let Some(w) = w {
                w.update(dia.source());
            }
        };
        let images = image_capability(&args.images)?;
        let opts = tui::RunOpts {
            source: "live",
            drain_on_quit: true,
            hide_thinking: args.no_thinking,
            images,
        };
        tui::run(rx, tx, opts, on_line, on_event)?;
        if let Some(w) = writer {
            w.finish();
        }
        return Ok(());
    }

    let inline_width = args.inline_width.unwrap_or_else(|| {
        let cols = terminal_size::terminal_size_of(std::io::stderr())
            .map(|(w, _)| w.0 as u32)
            .unwrap_or(100);
        (cols * 9).min(1600)
    });
    let proto = match args.proto {
        Some(p) => p,
        None if args.inline != Inline::Off && tty => Proto::probe().unwrap_or_else(Proto::detect),
        None => Proto::detect(),
    };
    let draw_inline = |term: &mut term::Term, src: &str| {
        if !tty {
            return;
        }
        match mm.inline(src, proto, inline_width) {
            Ok(esc) => term.raw(&esc),
            Err(e) => term.raw(&format!("trace_block: inline diagram failed: {e:#}\n")),
        }
    };

    // terminal graphics for image blocks in the scroll view
    let scroll_images = if tty { image_capability(&args.images)? } else { None };
    term.images = scroll_images;
    // stdin reader thread so the main loop can tick timers while waiting
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(1024);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        loop {
            let mut buf = Vec::new();
            match stdin.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx.send(buf).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut stdout = std::io::stdout().lock();
    // raw JSON on a terminal would interleave with (and bury) the stderr view
    let mut stdout_ok = !std::io::stdout().is_terminal();
    let mut save = match &args.save {
        Some(p) => Some(std::fs::File::create(p).with_context(|| format!("creating {}", p.display()))?),
        None => None,
    };
    let mut lineno = 0usize;
    let mut last_running_refresh = Instant::now();
    let mut inlined_final = false;
    let mut printer: Option<tui::CellPrinter> = None;
    let mut pending_session: Option<(serde_json::Value, usize)> = None;
    let mut lineno_of_first_event: Option<usize> = None;

    loop {
        let raw = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(l) => l,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                term.tick();
                if want_diagram && dia.has_running() && last_running_refresh.elapsed() >= Duration::from_secs(1) {
                    if let Some(w) = &writer {
                        w.update(dia.source());
                    }
                    last_running_refresh = Instant::now();
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        lineno += 1;

        // tee-safety: untouched bytes, flushed per line so downstream sees them live
        if stdout_ok && (stdout.write_all(&raw).is_err() || stdout.flush().is_err()) {
            stdout_ok = false; // downstream closed; keep rendering
        }
        if let Some(f) = &mut save
            && f.write_all(&raw).and_then(|_| f.flush()).is_err()
        {
            term.warn("write to --save file failed; saving stopped");
            save = None;
        }

        let text = String::from_utf8_lossy(&raw);
        let trimmed = text.trim_end_matches(['\n', '\r']);
        if trimmed.trim().is_empty() {
            continue;
        }
        let ev: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                term.malformed(lineno, &e.to_string());
                continue;
            }
        };
        if let Some(t) = traj.as_mut() {
            t.line(&raw, &ev);
        }
        // A pi `session` line starts both live streams and saved session logs: hold it until the
        // next event shows which one this is.
        if lineno_of_first_event.is_none() {
            lineno_of_first_event = Some(lineno);
            if ev["type"] == "session" {
                pending_session = Some((ev.clone(), lineno));
                continue;
            }
        }
        let session_log_entry = matches!(
            ev["type"].as_str(),
            Some("message" | "model_change" | "thinking_level_change")
        );
        // Claude Code streams/transcripts and pi session logs: rendered through the cell model
        if printer.is_some() || cells::is_claude_event(&ev) || (session_log_entry && pending_session.is_some()) {
            let p = printer.get_or_insert_with(|| {
                let sink: Box<dyn Write> = if args.quiet {
                    Box::new(std::io::sink())
                } else {
                    Box::new(std::io::stderr())
                };
                let mut p = tui::CellPrinter::new(tty, sink);
                p.images = scroll_images;
                p
            });
            if let Some((sev, sl)) = pending_session.take() {
                p.event(&sev, sl);
            }
            p.event(&ev, lineno);
            continue;
        }
        if let Some((sev, _)) = pending_session.take() {
            term.event(&sev);
        }
        term.event(&ev);
        if want_diagram
            && dia.event(&ev)
            && let Some(w) = &writer
        {
            w.update(dia.source());
        }
        match ev["type"].as_str() {
            Some("turn_end") if args.inline == Inline::Turn => draw_inline(&mut term, &dia.source()),
            Some("agent_settled") if args.inline != Inline::Off => {
                draw_inline(&mut term, &dia.source());
                inlined_final = true;
            }
            _ => {}
        }
    }

    if let Some((sev, _)) = pending_session.take() {
        term.event(&sev);
    }
    if let Some(p) = printer.as_mut() {
        p.finish();
        if let Some(w) = writer {
            w.finish();
        }
        return Ok(());
    }
    term.eof();
    if want_diagram && dia.eof() {
        if let Some(w) = &writer {
            w.update(dia.source());
        }
        if args.inline != Inline::Off && !inlined_final {
            draw_inline(&mut term, &dia.source());
        }
    }
    if let Some(w) = writer {
        w.finish();
    }
    Ok(())
}

/// Parse one raw input line; None for blank lines.
fn parse_line(raw: &[u8]) -> Option<std::result::Result<serde_json::Value, String>> {
    let text = String::from_utf8_lossy(raw);
    let t = text.trim_end_matches(['\n', '\r']);
    if t.trim().is_empty() {
        return None;
    }
    Some(serde_json::from_str(t).map_err(|e| e.to_string()))
}

/// `trace_block browse FILE [--no-follow]`
fn browse(args: &[String]) -> Result<()> {
    let mut file = None;
    let mut follow = true;
    let mut hide_thinking = false;
    let mut images_mode = String::from("auto");
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                println!(
                    "trace_block browse FILE [--no-follow] [--no-thinking]\n\nInteractive browser for a saved trace (e.g. from `-o`), pi or Claude Code. Follows the file\nwhile it grows unless --no-follow. Press ? inside for every key."
                );
                return Ok(());
            }
            "--no-follow" => follow = false,
            "--no-thinking" => hide_thinking = true,
            "--images" => images_mode = it.next().context("--images needs a value")?.clone(),
            p if !p.starts_with('-') && file.is_none() => file = Some(PathBuf::from(p)),
            other => bail!("browse: unknown argument '{other}'"),
        }
    }
    let path = file.context("browse: missing FILE")?;
    let f = std::fs::File::open(&path).with_context(|| format!("opening {}", path.display()))?;
    let (tx, rx) = mpsc::channel::<tui::Input>();
    let txs = tx.clone();
    std::thread::spawn(move || {
        let mut r = std::io::BufReader::new(f);
        let mut pending = Vec::new();
        loop {
            match r.read_until(b'\n', &mut pending) {
                Ok(0) | Err(_) => {
                    if !follow {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(300));
                }
                Ok(_) if pending.ends_with(b"\n") => {
                    if txs.send(tui::Input::Line(std::mem::take(&mut pending))).is_err() {
                        return;
                    }
                }
                Ok(_) => {} // partial line still being written; wait for the rest
            }
        }
        if !pending.is_empty() {
            let _ = txs.send(tui::Input::Line(pending));
        }
        let _ = txs.send(tui::Input::Eof);
    });
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let images = image_capability(&images_mode)?;
    let opts = tui::RunOpts {
        source: &name,
        drain_on_quit: false,
        hide_thinking,
        images,
    };
    tui::run(rx, tx, opts, parse_line, |_, _| {})
}

/// Which image protocol to draw with (and the terminal cell size), per `--images`.
/// `auto` asks the terminal (kitty graphics query + DA1); None → placeholders only.
fn image_capability(mode: &str) -> Result<Option<(Proto, (u32, u32))>> {
    if mode == "off" || !std::io::stderr().is_terminal() {
        return Ok(None);
    }
    // cell size from the window-size ioctl when the terminal reports pixels; the capability probe
    // (kitty graphics query + DA1 + cell-size query) only when asked for `auto` or pixels are unknown
    let ioctl_cell = tty_cell_px();
    let (probed, probed_cell) = if mode == "auto" || ioctl_cell.is_none() {
        mermaid::probe_terminal()
    } else {
        (None, None)
    };
    let cell = ioctl_cell.or(probed_cell).unwrap_or((10, 20));
    let proto = match mode {
        "auto" => match probed {
            Some(p @ (Proto::Kitty | Proto::Sixel)) => Some(p),
            _ if matches!(std::env::var("TERM_PROGRAM").as_deref(), Ok("iTerm.app" | "WezTerm")) => Some(Proto::Iterm),
            _ => None,
        },
        other => match Proto::parse(other) {
            Some(Proto::Text) | None => bail!("--images: expected auto|kitty|sixel|iterm|off, got '{other}'"),
            Some(p) => Some(p),
        },
    };
    Ok(proto.map(|p| (p, cell)))
}

/// Terminal cell size in pixels from TIOCGWINSZ on /dev/tty, when the terminal reports pixels.
fn tty_cell_px() -> Option<(u32, u32)> {
    use std::os::fd::AsRawFd;
    let tty = std::fs::File::open("/dev/tty").ok()?;
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(tty.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) } != 0 {
        return None;
    }
    (ws.ws_col > 0 && ws.ws_row > 0 && ws.ws_xpixel > 0 && ws.ws_ypixel > 0).then(|| {
        (
            ws.ws_xpixel as u32 / ws.ws_col as u32,
            ws.ws_ypixel as u32 / ws.ws_row as u32,
        )
    })
}
