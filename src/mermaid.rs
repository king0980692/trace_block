//! Mermaid → SVG/PNG via mermaid-rs-renderer (mmdr), live file outputs and inline terminal images.

use anyhow::{Context, Result};
use base64::Engine;
use mermaid_rs_renderer::{RenderOptions, Theme, render_with_options};
use resvg::{tiny_skia, usvg};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Proto {
    Sixel,
    Kitty,
    Iterm,
    /// character-cell diagram, works in any terminal
    Text,
}

impl Proto {
    pub fn parse(s: &str) -> Option<Proto> {
        match s {
            "sixel" => Some(Proto::Sixel),
            "kitty" => Some(Proto::Kitty),
            "iterm" | "iterm2" | "wezterm" => Some(Proto::Iterm),
            "text" | "ascii" => Some(Proto::Text),
            "auto" => Some(Proto::probe().unwrap_or_else(Proto::detect)),
            _ => None,
        }
    }

    /// Ask the terminal itself (works over ssh, unlike env vars): a kitty graphics query
    /// followed by DA1. Kitty-protocol terminals (kitty, ghostty, wezterm) answer the query;
    /// sixel terminals list attribute 4 in DA1. Returns None when there is no tty to ask.
    pub fn probe() -> Option<Proto> {
        probe_terminal().0
    }

    pub fn detect() -> Proto {
        let term = std::env::var("TERM").unwrap_or_default();
        let prog = std::env::var("TERM_PROGRAM").unwrap_or_default();
        if std::env::var_os("KITTY_WINDOW_ID").is_some()
            || term.contains("kitty")
            || term.contains("ghostty")
            || prog == "ghostty"
        {
            Proto::Kitty
        } else if prog == "iTerm.app" || prog == "WezTerm" {
            Proto::Iterm
        } else {
            // Windows Terminal (WSL), foot, mlterm, xterm -ti vt340, …
            Proto::Sixel
        }
    }
}

#[derive(Clone)]
pub struct Mermaid {
    theme_name: String,
    fontdb: Arc<OnceLock<Arc<usvg::fontdb::Database>>>,
}

impl Mermaid {
    pub fn new(theme: &str) -> Result<Self> {
        Theme::from_name(theme)
            .with_context(|| format!("unknown theme '{theme}' (modern, default, dark, forest, neutral)"))?;
        Ok(Mermaid {
            theme_name: theme.to_string(),
            fontdb: Arc::new(OnceLock::new()),
        })
    }

    fn theme(&self) -> Theme {
        Theme::from_name(&self.theme_name).unwrap()
    }

    pub fn svg(&self, src: &str) -> Result<String> {
        let opts = RenderOptions {
            theme: self.theme(),
            ..RenderOptions::default()
        };
        render_with_options(src, opts)
    }

    /// Rasterize an SVG, scaled so the width is at most `max_w` pixels (never upscaled beyond `scale`).
    pub fn raster(&self, svg: &str, scale: f32, max_w: u32) -> Result<tiny_skia::Pixmap> {
        let db = self
            .fontdb
            .get_or_init(|| {
                let mut db = usvg::fontdb::Database::new();
                db.load_system_fonts();
                Arc::new(db)
            })
            .clone();
        let opt = usvg::Options {
            fontdb: db,
            ..Default::default()
        };
        let tree = usvg::Tree::from_str(svg, &opt)?;
        let size = tree.size();
        let s = scale.min(max_w as f32 / size.width()).max(0.05);
        let (w, h) = ((size.width() * s).ceil() as u32, (size.height() * s).ceil() as u32);
        let mut pm = tiny_skia::Pixmap::new(w.max(1), h.max(1)).context("pixmap alloc")?;
        pm.fill(parse_hex(&self.theme().background).unwrap_or(tiny_skia::Color::WHITE));
        resvg::render(&tree, tiny_skia::Transform::from_scale(s, s), &mut pm.as_mut());
        Ok(pm)
    }

    /// Escape sequence that draws the diagram inline in the terminal.
    pub fn inline(&self, src: &str, proto: Proto, max_w: u32) -> Result<String> {
        if proto == Proto::Text {
            let cols = (max_w / 9).max(40) as usize;
            let (head, body) = crate::textdia::parse(src).render(cols, true);
            return Ok(head.into_iter().chain(body).collect::<Vec<_>>().join("\n") + "\n");
        }
        let svg = self.svg(src)?;
        let pm = self.raster(&svg, 1.5, max_w)?;
        Ok(encode_image(&pm, proto, None)? + "\n")
    }
}

/// Terminal escape sequence that draws `pm` at the cursor position.
/// `cols`: display width in terminal columns (kitty/iTerm scale the image to it).
pub fn encode_image(pm: &tiny_skia::Pixmap, proto: Proto, cols: Option<u16>) -> Result<String> {
    {
        let b64 = base64::engine::general_purpose::STANDARD;
        Ok(match proto {
            Proto::Sixel => {
                let opts = icy_sixel::EncodeOptions {
                    max_colors: 64,
                    diffusion: 0.0,
                    ..Default::default()
                };
                icy_sixel::sixel_encode(pm.data(), pm.width() as usize, pm.height() as usize, &opts)
                    .map_err(|e| anyhow::anyhow!("sixel: {e}"))?
            }
            Proto::Kitty => {
                let data = b64.encode(png_bytes(pm)?);
                let chunks: Vec<&[u8]> = data.as_bytes().chunks(4096).collect();
                let mut out = String::new();
                for (i, c) in chunks.iter().enumerate() {
                    let more = if i + 1 < chunks.len() { 1 } else { 0 };
                    let size = cols.map(|c| format!(",c={c}")).unwrap_or_default();
                    let ctl = if i == 0 {
                        format!("a=T,f=100,q=2{size},m={more}")
                    } else {
                        format!("m={more}")
                    };
                    out += &format!("\x1b_G{ctl};{}\x1b\\", std::str::from_utf8(c).unwrap());
                }
                out
            }
            Proto::Text => anyhow::bail!("text mode has no image encoding"),
            Proto::Iterm => {
                let png = png_bytes(pm)?;
                let width = cols.map(|c| format!(";width={c}")).unwrap_or_default();
                format!(
                    "\x1b]1337;File=inline=1;size={}{width};preserveAspectRatio=1:{}\x07",
                    png.len(),
                    b64.encode(png)
                )
            }
        })
    }
}

/// Pixel size of a recorded (base64) image, read from its header.
pub fn image_dims(b64: &str) -> Option<(u32, u32)> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64.trim()).ok()?;
    let sz = imagesize::blob_size(&bytes).ok()?;
    Some((sz.width as u32, sz.height as u32))
}

/// Decode a recorded image (PNG/JPEG/GIF/WebP, base64) and scale it to fit `max_w`×`max_h`
/// pixels, never enlarging it. Decoding goes through resvg by wrapping the image in an SVG.
pub fn raster_image(mime: &str, b64: &str, max_w: u32, max_h: u32) -> Result<tiny_skia::Pixmap> {
    let (w, h) = image_dims(b64).context("unreadable image data")?;
    let (wf, hf) = (w.max(1) as f32, h.max(1) as f32);
    let s = (max_w as f32 / wf).min(max_h as f32 / hf).min(1.0);
    let svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="{w}" height="{h}"><image width="{w}" height="{h}" xlink:href="data:{mime};base64,{}"/></svg>"#,
        b64.trim()
    );
    let tree = usvg::Tree::from_str(&svg, &usvg::Options::default())?;
    let (pw, ph) = (((wf * s).ceil() as u32).max(1), ((hf * s).ceil() as u32).max(1));
    let mut pm = tiny_skia::Pixmap::new(pw, ph).context("pixmap alloc")?;
    resvg::render(&tree, tiny_skia::Transform::from_scale(s, s), &mut pm.as_mut());
    Ok(pm)
}

/// Terminal cells (cols, rows) an image of `w`×`h` px occupies when fitted into `max_cols`×`max_rows`
/// cells of `cell` pixels (never enlarged), plus the pixel box to rasterise it to.
pub fn fit_cells(w: u32, h: u32, max_cols: u32, max_rows: u32, cell: (u32, u32)) -> ((u32, u32), (u32, u32)) {
    let (cw, ch) = (cell.0.max(1) as f32, cell.1.max(1) as f32);
    let s = ((max_cols as f32 * cw) / w.max(1) as f32)
        .min((max_rows as f32 * ch) / h.max(1) as f32)
        .min(1.0);
    let (pw, ph) = ((w as f32 * s).max(1.0), (h as f32 * s).max(1.0));
    let cols = (pw / cw).ceil().max(1.0) as u32;
    let rows = (ph / ch).ceil().max(1.0) as u32;
    ((cols, rows), (pw as u32, ph as u32))
}

/// Escape sequence drawing a recorded image at the cursor, fitted into the given cells.
pub fn image_escape(mime: &str, b64: &str, proto: Proto, cols: u32, rows: u32, cell: (u32, u32)) -> Result<String> {
    let pm = raster_image(mime, b64, cols * cell.0.max(1), rows * cell.1.max(1))?;
    encode_image(&pm, proto, Some(cols as u16))
}

fn parse_hex(s: &str) -> Option<tiny_skia::Color> {
    let h = s.trim().strip_prefix('#')?;
    let h = if h.len() == 3 {
        h.chars().flat_map(|c| [c, c]).collect()
    } else {
        h.to_string()
    };
    if h.len() < 6 {
        return None;
    }
    let p = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some(tiny_skia::Color::from_rgba8(p(0)?, p(2)?, p(4)?, 255))
}

pub fn png_bytes(pm: &tiny_skia::Pixmap) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, pm.width(), pm.height());
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        // background is filled opaque, so premultiplied == straight alpha
        enc.write_header()?.write_image_data(pm.data())?;
    }
    Ok(out)
}

/// Write via temp file + rename so viewers never see a half-written file.
fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[derive(Default, Clone)]
pub struct Outputs {
    pub mmd: Option<PathBuf>,
    pub svg: Option<PathBuf>,
    pub png: Option<PathBuf>,
}

impl Outputs {
    pub fn any(&self) -> bool {
        self.mmd.is_some() || self.svg.is_some() || self.png.is_some()
    }
}

/// Background writer: renders the newest diagram source (coalescing bursts) to the output files.
pub struct LiveWriter {
    tx: Option<Sender<String>>,
    handle: Option<JoinHandle<()>>,
}

impl LiveWriter {
    pub fn spawn(mm: Mermaid, outs: Outputs, debounce: Duration) -> Self {
        let (tx, rx) = mpsc::channel::<String>();
        let handle = std::thread::spawn(move || writer_loop(mm, outs, rx, debounce));
        LiveWriter {
            tx: Some(tx),
            handle: Some(handle),
        }
    }

    pub fn update(&self, src: String) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(src);
        }
    }

    /// Flush the last update and stop.
    pub fn finish(mut self) {
        drop(self.tx.take());
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn writer_loop(mm: Mermaid, outs: Outputs, rx: Receiver<String>, debounce: Duration) {
    let mut last_err = String::new();
    while let Ok(mut src) = rx.recv() {
        // coalesce: wait a moment, keep only the newest source
        std::thread::sleep(debounce);
        while let Ok(newer) = rx.try_recv() {
            src = newer;
        }
        if let Err(e) = write_all(&mm, &outs, &src) {
            let msg = format!("{e:#}");
            if msg != last_err {
                eprintln!("trace_block: diagram render failed: {msg}");
                last_err = msg;
            }
        }
    }
}

fn write_all(mm: &Mermaid, outs: &Outputs, src: &str) -> Result<()> {
    if let Some(p) = &outs.mmd {
        write_atomic(p, src.as_bytes()).with_context(|| format!("writing {}", p.display()))?;
    }
    if outs.svg.is_none() && outs.png.is_none() {
        return Ok(());
    }
    let svg = mm.svg(src)?;
    if let Some(p) = &outs.svg {
        write_atomic(p, svg.as_bytes()).with_context(|| format!("writing {}", p.display()))?;
    }
    if let Some(p) = &outs.png {
        let pm = mm.raster(&svg, 2.0, 4000)?;
        write_atomic(p, &png_bytes(&pm)?).with_context(|| format!("writing {}", p.display()))?;
    }
    Ok(())
}

pub fn probe_terminal() -> (Option<Proto>, Option<(u32, u32)>) {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    let mut tty = match std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty") {
        Ok(t) => t,
        Err(_) => return (None, None),
    };
    let fd = tty.as_raw_fd();
    let mut old: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut old) } != 0 {
        return (None, None);
    }
    let mut raw = old;
    raw.c_lflag &= !(libc::ICANON | libc::ECHO);
    raw.c_cc[libc::VMIN] = 0;
    raw.c_cc[libc::VTIME] = 0;
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) };
    let _ = tty.write_all(b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[16t\x1b[c");
    let _ = tty.flush();
    let mut resp = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_millis(1500);
    // read until the DA1 reply (ESC [ ? … c) arrives or we time out
    while std::time::Instant::now() < deadline {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut pfd, 1, 100) } <= 0 {
            continue;
        }
        let mut buf = [0u8; 256];
        match tty.read(&mut buf) {
            Ok(n) if n > 0 => resp.extend_from_slice(&buf[..n]),
            _ => {}
        }
        let text = String::from_utf8_lossy(&resp);
        if let Some(i) = text.find("\x1b[?")
            && text[i..].contains('c')
        {
            break;
        }
    }
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &old) };
    let text = String::from_utf8_lossy(&resp);
    let cell = text.find("\x1b[6;").and_then(|i| {
        let rest = &text[i + 4..];
        let rest = &rest[..rest.find('t')?];
        let (h, w) = rest.split_once(';')?;
        Some((w.parse().ok()?, h.parse().ok()?))
    });
    if text.contains("_Gi=31;OK") {
        return (Some(Proto::Kitty), cell);
    }
    if let Some(i) = text.find("\x1b[?") {
        let da1 = &text[i + 3..];
        let da1 = &da1[..da1.find('c').unwrap_or(da1.len())];
        if da1.split(';').any(|p| p == "4") {
            return (Some(Proto::Sixel), cell);
        }
        return (Some(Proto::Text), cell); // terminal answered but supports neither
    }
    (None, cell)
}

#[cfg(test)]
mod image_tests {
    use super::*;

    #[test]
    fn decode_and_fit_a_png() {
        // 3x2 red PNG
        let mut pm = tiny_skia::Pixmap::new(3, 2).unwrap();
        pm.fill(tiny_skia::Color::from_rgba8(255, 0, 0, 255));
        let b64 = base64::engine::general_purpose::STANDARD.encode(png_bytes(&pm).unwrap());
        assert_eq!(image_dims(&b64), Some((3, 2)));
        let out = raster_image("image/png", &b64, 100, 100).unwrap();
        assert_eq!((out.width(), out.height()), (3, 2), "never enlarged");
        let px = out.pixel(1, 1).unwrap();
        assert_eq!((px.red(), px.green(), px.blue()), (255, 0, 0));
        let ((c, r), _) = fit_cells(320, 200, 40, 10, (10, 20));
        assert_eq!((c, r), (32, 10));
        let kitty = image_escape("image/png", &b64, Proto::Kitty, 2, 1, (10, 20)).unwrap();
        assert!(kitty.starts_with("\x1b_Ga=T,f=100,q=2,c=2"));
        let sixel = image_escape("image/png", &b64, Proto::Sixel, 2, 1, (10, 20)).unwrap();
        assert!(sixel.starts_with("\x1bP"));
    }
}
