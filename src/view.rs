//! `trace_block view FILE.mmd` — full-screen live viewer: redraws the diagram in place whenever
//! the file changes, showing the newest (bottom) part scaled to the terminal width.

use crate::mermaid::{Mermaid, Proto, encode_image, probe_terminal};
use anyhow::{Context, Result, bail};
use resvg::tiny_skia;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

pub const USAGE: &str = "\
trace_block view — live full-screen diagram viewer (run it in a second terminal pane)

USAGE:
  trace_block view PATH.mmd [--proto auto|kitty|sixel|iterm|text] [--theme NAME] [--cell WxH] [--all]
  trace_block view --probe      ask the terminal what it supports, print the answer, exit

Watches PATH.mmd (written by `trace_block --mmd PATH.mmd`) and redraws on every change.
By default the newest part of the diagram is shown, scaled to the window width.
  --proto      auto (default) queries the terminal: kitty graphics (kitty, ghostty, wezterm),
               else sixel, else a character-cell diagram (text). Works over ssh.
  --all        scale the whole diagram to fit the window instead
  --cell WxH   terminal cell size in pixels when the terminal doesn't report it (default 10x20)
";

struct Win {
    cols: u16,
    rows: u16,
    px_w: u32,
    px_h: u32,
}

fn window(cell: (u32, u32)) -> Win {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0;
    let (cols, rows) = if ok { (ws.ws_col, ws.ws_row) } else { (100, 40) };
    let (px_w, px_h) = if ok && ws.ws_xpixel > 0 && ws.ws_ypixel > 0 {
        (ws.ws_xpixel as u32, ws.ws_ypixel as u32)
    } else {
        (cols as u32 * cell.0, rows as u32 * cell.1)
    };
    Win { cols, rows, px_w, px_h }
}

pub fn run(args: &[String]) -> Result<()> {
    let mut path: Option<PathBuf> = None;
    let mut proto: Option<Proto> = None;
    let mut probe_only = false;
    let mut theme = "modern".to_string();
    let mut cell = (10u32, 20u32);
    let mut fit_all = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            "--proto" => {
                let v = it.next().context("--proto needs a value")?;
                proto = Some(Proto::parse(v).with_context(|| format!("--proto: unknown '{v}'"))?);
            }
            "--probe" => probe_only = true,
            "--theme" => theme = it.next().context("--theme needs a value")?.clone(),
            "--cell" => {
                let v = it.next().context("--cell needs WxH")?;
                let (w, h) = v.split_once('x').context("--cell expects WxH, e.g. 10x20")?;
                cell = (w.parse()?, h.parse()?);
            }
            "--all" => fit_all = true,
            p if !p.starts_with('-') && path.is_none() => path = Some(PathBuf::from(p)),
            other => bail!("view: unknown argument '{other}' (see `trace_block view --help`)"),
        }
    }
    // ask the terminal before entering the alternate screen
    let (probed, cell_px) = probe_terminal();
    if let Some(c) = cell_px {
        cell = c;
    }
    if probe_only {
        println!(
            "terminal answer : {}",
            probed
                .map(|p| format!("{p:?}"))
                .unwrap_or("no reply (not a tty?)".into())
        );
        println!(
            "cell size (px)  : {}",
            cell_px.map(|(w, h)| format!("{w}x{h}")).unwrap_or("unknown".into())
        );
        println!(
            "env guess       : {:?}  (TERM={} TERM_PROGRAM={})",
            Proto::detect(),
            std::env::var("TERM").unwrap_or_default(),
            std::env::var("TERM_PROGRAM").unwrap_or_default()
        );
        return Ok(());
    }
    let (proto, how) = match (proto, probed) {
        (Some(p), _) => (p, "--proto"),
        (None, Some(p)) => (p, "detected"),
        (None, None) => (Proto::detect(), "guessed"),
    };
    let proto_label = format!("{proto:?} ({how})").to_lowercase();
    let path = path.context("view: missing PATH.mmd (see `trace_block view --help`)")?;
    let mm = Mermaid::new(&theme)?;

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        ctrlc::set_handler(move || stop.store(true, Ordering::SeqCst)).context("installing Ctrl+C handler")?;
    }

    let mut out = std::io::stdout().lock();
    // alternate screen, hide cursor
    write!(out, "\x1b[?1049h\x1b[?25l\x1b[2J")?;
    out.flush()?;

    let mut last_seen: Option<(SystemTime, u64)> = None;
    let mut last_size = (0u16, 0u16);
    let mut status = format!("waiting for {} …", path.display());
    let mut shown_status = String::new();

    while !stop.load(Ordering::SeqCst) {
        let win = window(cell);
        let meta = std::fs::metadata(&path)
            .ok()
            .and_then(|m| Some((m.modified().ok()?, m.len())));
        let resized = (win.cols, win.rows) != last_size;
        if meta.is_some() && (meta != last_seen || resized) {
            last_seen = meta;
            last_size = (win.cols, win.rows);
            match draw(&mm, &path, proto, &win, fit_all) {
                Ok((esc, info)) => {
                    write!(out, "\x1b[H\x1b[2J")?;
                    if proto == Proto::Kitty {
                        write!(out, "\x1b_Ga=d,d=A,q=2\x1b\\")?; // drop previous images
                    }
                    out.write_all(esc.as_bytes())?;
                    status = format!(
                        "{} · {proto_label} · {info} · {}",
                        path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(),
                        clock()
                    );
                }
                Err(e) => status = format!("render failed: {e:#}"),
            }
            shown_status.clear();
        }
        if status != shown_status {
            let line = format!(" {status} · Ctrl+C to quit");
            let line: String = line.chars().take(win.cols as usize).collect();
            write!(out, "\x1b[{};1H\x1b[2K\x1b[7m{line}\x1b[0m", win.rows)?;
            out.flush()?;
            shown_status = status.clone();
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    write!(out, "\x1b[?25h\x1b[?1049l")?;
    out.flush()?;
    Ok(())
}

fn draw(mm: &Mermaid, path: &PathBuf, proto: Proto, win: &Win, fit_all: bool) -> Result<(String, String)> {
    let src = std::fs::read_to_string(path)?;
    if proto == Proto::Text {
        let (head, body) = crate::textdia::parse(&src).render(win.cols as usize, true);
        let room = (win.rows as usize).saturating_sub(head.len() + 1);
        let skip = body.len().saturating_sub(room);
        let mut info = format!("{} lines", src.lines().count());
        if skip > 0 {
            info += &format!(" · showing newest {} of {} rows", room, body.len());
        }
        let rows: Vec<&String> = head.iter().chain(body[skip..].iter()).collect();
        let screen = rows.iter().map(|r| r.as_str()).collect::<Vec<_>>().join("\r\n");
        return Ok((screen, info));
    }
    let svg = mm.svg(&src)?;
    // leave the last text row for the status bar
    let avail_h = win.px_h.saturating_sub(win.px_h / win.rows.max(1) as u32).max(50);
    let max_w = win.px_w.saturating_sub(4).max(100);
    let mut pm = mm.raster(&svg, 2.0, max_w)?;
    let full_h = pm.height();
    if fit_all && pm.height() > avail_h {
        // re-rasterize small enough that the whole diagram fits
        let w = (pm.width() as f32 * avail_h as f32 / pm.height() as f32) as u32;
        pm = mm.raster(&svg, 2.0, w.max(50))?;
    }
    let mut info = format!("{} lines", src.lines().count());
    if pm.height() > avail_h {
        // newest events are at the bottom: show the tail
        let rect = tiny_skia::IntRect::from_xywh(0, (pm.height() - avail_h) as i32, pm.width(), avail_h)
            .context("crop rect")?;
        pm = pm.clone_rect(rect).context("crop")?;
        info += &format!(" · showing newest {}%", (avail_h as u64 * 100 / full_h as u64).max(1));
    }
    // kitty/iTerm: let the terminal scale to the column count so a wrong pixel guess can't overflow
    let cols = (pm.width() * win.cols as u32 / win.px_w.max(1)).clamp(1, win.cols as u32) as u16;
    Ok((encode_image(&pm, proto, Some(cols))?, info))
}

fn clock() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t = secs as libc::time_t;
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("updated {:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}
