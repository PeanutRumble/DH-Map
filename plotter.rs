//! Live heat map of a tailed CSV: time_ms,id,x,z,weight,sections
//!
//! * Each reading becomes a blob at (x, z) with intensity from `weight`.
//! * When a new reading arrives for the same `id`, that id's previous blob
//!   starts decaying.
//! * If an id gets no new reading for `--idle` seconds (default 60), its
//!   current blob starts decaying too.
//! * Decaying blobs fade out over `--decay` seconds, then are removed.

use std::{
    error::Error,
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    num::NonZeroU32,
    path::{Path, PathBuf},
    sync::{mpsc, Arc},
    thread,
    time::{Duration, Instant},
};

use clap::Parser;
use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::{Key, NamedKey},
    window::{Window, WindowId},
};

// ───────────────────────────── CLI ─────────────────────────────

#[derive(Parser, Clone)]
#[command(about = "Tails a DistantHorizons.sqlite (path asked on start) and shows a live heat map")]
pub struct Args {
    /// CSV file to tail (written by the tracker)
    #[arg(long, default_value = "heat.csv")]
    path: PathBuf,

    /// Process existing file contents instead of starting at the end
    #[arg(long)]
    from_start: bool,

    /// Replay existing contents using the time_ms deltas (implies --from-start)
    #[arg(long)]
    replay: bool,

    /// Replay speed multiplier
    #[arg(long, default_value_t = 1.0)]
    speed: f64,

    /// Seconds without a new reading before an id's blob starts decaying
    #[arg(long, default_value_t = 60.0)]
    idle: f64,

    /// Seconds a blob takes to fade out once it starts decaying
    #[arg(long, default_value_t = 10.0)]
    decay: f64,

    /// Blob radius in pixels
    #[arg(long, default_value_t = 48.0)]
    radius: f32,

    /// Weight that maps to full heat (default: running maximum)
    #[arg(long)]
    max_weight: Option<f32>,

    /// Fixed world bounds instead of auto-fit: XMIN XMAX ZMIN ZMAX
    #[arg(long, num_args = 4, value_names = ["XMIN", "XMAX", "ZMIN", "ZMAX"], allow_hyphen_values = true)]
    bounds: Option<Vec<f32>>,
}

// ───────────────────────────── CSV tailing ─────────────────────────────

struct Reading {
    id: String,
    x: f32,
    z: f32,
    weight: f32,
    t: Option<i64>,
    at: Instant,
}

#[derive(Clone)]
struct Columns {
    t: usize,
    id: usize,
    x: usize,
    z: usize,
    w: usize,
}

impl Default for Columns {
    fn default() -> Self {
        Self { t: 0, id: 1, x: 2, z: 3, w: 4 }
    }
}

fn parse_header(line: &str) -> Option<Columns> {
    let names: Vec<String> = line.split(',').map(|s| s.trim().to_ascii_lowercase()).collect();
    let find = |n: &str| names.iter().position(|c| c == n);
    Some(Columns {
        t: find("time_ms").unwrap_or(0),
         id: find("id")?,
         x: find("x")?,
         z: find("z")?,
         w: find("weight")?,
    })
}

fn parse_line(line: &str, c: &Columns) -> Option<Reading> {
    let f: Vec<&str> = line.split(',').map(str::trim).collect();
    Some(Reading {
        id: f.get(c.id)?.to_string(),
         x: f.get(c.x)?.parse().ok()?,
         z: f.get(c.z)?.parse().ok()?,
         weight: f.get(c.w)?.parse().ok()?,
         t: f.get(c.t).and_then(|s| s.parse().ok()),
         at: Instant::now(),
    })
}

fn read_header(path: &Path) -> Columns {
    let Ok(file) = File::open(path) else { return Columns::default() };
    let mut first = String::new();
    if BufReader::new(file).read_line(&mut first).is_err() {
        return Columns::default();
    }
    parse_header(first.trim()).unwrap_or_default()
}

struct Tailer {
    path: PathBuf,
    pos: u64,
    pending: Vec<u8>,
    cols: Columns,
}

impl Tailer {
    fn new(path: PathBuf, from_start: bool) -> Self {
        let cols = read_header(&path);
        let pos = if from_start {
            0
        } else {
            std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
        };
        Self { path, pos, pending: Vec::new(), cols }
    }

    /// Read any newly appended complete lines. Re-opens the path each poll so
    /// truncation and log rotation are picked up.
    fn poll(&mut self, mut emit: impl FnMut(Reading)) {
        let Ok(mut f) = File::open(&self.path) else { return };
        let Ok(meta) = f.metadata() else { return };
        let len = meta.len();
        if len < self.pos {
            self.pos = 0;
            self.pending.clear();
        }
        if len == self.pos {
            return;
        }
        if f.seek(SeekFrom::Start(self.pos)).is_err() {
            return;
        }
        let mut chunk = Vec::new();
        if f.take(len - self.pos).read_to_end(&mut chunk).is_err() {
            return;
        }
        self.pos += chunk.len() as u64;
        self.pending.extend_from_slice(&chunk);

        while let Some(i) = self.pending.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = self.pending.drain(..=i).collect();
            let text = String::from_utf8_lossy(&raw);
            let line = text.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(c) = parse_header(line) {
                self.cols = c;
                continue;
            }
            if let Some(r) = parse_line(line, &self.cols) {
                emit(r);
            }
        }
    }
}

fn tail_thread(args: Args, tx: mpsc::Sender<Reading>) {
    let mut tailer = Tailer::new(args.path.clone(), args.from_start || args.replay);

    if args.replay {
        let mut prev: Option<i64> = None;
        tailer.poll(|mut r| {
            if let (Some(p), Some(t)) = (prev, r.t) {
                let ms = (t - p).clamp(0, 2000) as f64 / args.speed.max(0.01);
                thread::sleep(Duration::from_millis(ms as u64));
            }
            prev = r.t;
            r.at = Instant::now();
            let _ = tx.send(r);
        });
    }

    loop {
        tailer.poll(|r| {
            let _ = tx.send(r);
        });
        thread::sleep(Duration::from_millis(100));
    }
}

// ───────────────────────────── Model ─────────────────────────────

#[derive(Clone, Copy)]
struct Bounds {
    x0: f32,
    x1: f32,
    z0: f32,
    z1: f32,
}

enum State {
    Live,
    Decaying(Instant),
}

struct Blob {
    id: String,
    x: f32,
    z: f32,
    weight: f32,
    last_seen: Instant,
    state: State,
}

struct Transform {
    scale: f32,
    cx: f32,
    cz: f32,
    sx: f32,
    sy: f32,
}

#[derive(Clone, Copy)]
struct Rect {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

impl Transform {
    /// World (x, z) -> screen pixels.
    fn apply(&self, x: f32, z: f32) -> (f32, f32) {
        (self.sx + (x - self.cx) * self.scale, self.sy + (z - self.cz) * self.scale)
    }

    /// Screen pixels -> world (x, z).
    fn invert(&self, px: f32, py: f32) -> (f32, f32) {
        (self.cx + (px - self.sx) / self.scale, self.cz + (py - self.sy) / self.scale)
    }
}

struct Model {
    blobs: Vec<Blob>,
    bounds: Option<Bounds>,
    fixed_bounds: bool,
    max_w: f32,
    fixed_max: bool,
    idle: Duration,
    decay: Duration,
}

impl Model {
    fn new(args: &Args) -> Self {
        let bounds = args.bounds.as_ref().map(|b| Bounds { x0: b[0], x1: b[1], z0: b[2], z1: b[3] });
        Self {
            blobs: Vec::new(),
            fixed_bounds: bounds.is_some(),
            bounds,
            max_w: args.max_weight.unwrap_or(1.0).max(1e-6),
            fixed_max: args.max_weight.is_some(),
            idle: Duration::from_secs_f64(args.idle.max(0.0)),
            decay: Duration::from_secs_f64(args.decay.max(0.001)),
        }
    }

    fn ingest(&mut self, r: Reading) {
        // A new reading for this id starts the previous blob decaying.
        for b in &mut self.blobs {
            if b.id == r.id && matches!(b.state, State::Live) {
                b.state = State::Decaying(r.at);
            }
        }
        if !self.fixed_bounds {
            if let Some(b) = self.bounds.as_mut() {
                b.x0 = b.x0.min(r.x);
                b.x1 = b.x1.max(r.x);
                b.z0 = b.z0.min(r.z);
                b.z1 = b.z1.max(r.z);
            } else {
                self.bounds = Some(Bounds { x0: r.x, x1: r.x, z0: r.z, z1: r.z });
            }
        }
        if !self.fixed_max {
            self.max_w = self.max_w.max(r.weight);
        }
        self.blobs.push(Blob {
            id: r.id,
            x: r.x,
            z: r.z,
            weight: r.weight,
            last_seen: r.at,
            state: State::Live,
        });
    }

    fn tick(&mut self, now: Instant) {
        let (idle, decay) = (self.idle, self.decay);
        for b in &mut self.blobs {
            if matches!(b.state, State::Live) && now.saturating_duration_since(b.last_seen) >= idle {
                b.state = State::Decaying(b.last_seen + idle);
            }
        }
        self.blobs.retain(|b| match b.state {
            State::Live => true,
            State::Decaying(t) => now.saturating_duration_since(t) < decay,
        });
    }

    fn fade(&self, b: &Blob, now: Instant) -> f32 {
        match b.state {
            State::Live => 1.0,
            State::Decaying(t) => {
                let p = now.saturating_duration_since(t).as_secs_f32() / self.decay.as_secs_f32();
                let k = (1.0 - p).clamp(0.0, 1.0);
                k * k
            }
        }
    }

    /// Fit the data bounds into `rect` (equal scale on both axes).
    fn transform(&self, rect: Rect, pad: f32) -> Option<Transform> {
        let b = self.bounds?;
        let xr = (b.x1 - b.x0).max(50.0);
        let zr = (b.z1 - b.z0).max(50.0);
        let (rw, rh) = (rect.x1 - rect.x0, rect.y1 - rect.y0);
        let scale = ((rw - 2.0 * pad).max(1.0) / xr).min((rh - 2.0 * pad).max(1.0) / zr);
        Some(Transform {
            scale,
            cx: (b.x0 + b.x1) / 2.0,
             cz: (b.z0 + b.z1) / 2.0,
             sx: (rect.x0 + rect.x1) / 2.0,
             sy: (rect.y0 + rect.y1) / 2.0, // +z points down the screen (top-down map convention)
        })
    }
}

// ───────────────────────────── Rendering ─────────────────────────────

fn build_lut() -> Vec<u32> {
    const STOPS: [(f32, [f32; 3]); 6] = [
        (0.00, [12.0, 12.0, 20.0]),
        (0.15, [20.0, 40.0, 160.0]),
        (0.40, [0.0, 200.0, 220.0]),
        (0.60, [40.0, 220.0, 60.0]),
        (0.80, [255.0, 220.0, 0.0]),
        (1.00, [255.0, 40.0, 0.0]),
    ];
    (0..256)
    .map(|i| {
        let v = i as f32 / 255.0;
        let mut c = STOPS[5].1;
        for w in STOPS.windows(2) {
            let (a, b) = (w[0], w[1]);
            if v <= b.0 {
                let t = (v - a.0) / (b.0 - a.0);
                c = [
                    a.1[0] + (b.1[0] - a.1[0]) * t,
         a.1[1] + (b.1[1] - a.1[1]) * t,
         a.1[2] + (b.1[2] - a.1[2]) * t,
                ];
                break;
            }
        }
        ((c[0] as u32) << 16) | ((c[1] as u32) << 8) | (c[2] as u32)
    })
    .collect()
}

// ───────────────────── Overlay: grid, labels, readouts ─────────────────────

const M_LEFT: f32 = 60.0;
const M_RIGHT: f32 = 16.0;
const M_TOP: f32 = 16.0;
const M_BOTTOM: f32 = 30.0;
const BG: u32 = 0x000C_0C14;
const TEXT: u32 = 0x00C8_C8D2;
const WHITE: u32 = 0x00FF_FFFF;
const FRAME: u32 = 0x0050_5068;

/// 3x5 bitmap glyphs (bit 2 = leftmost pixel). Only what the overlay needs.
fn glyph(c: char) -> [u8; 5] {
    match c {
        '0' => [0b111, 0b101, 0b101, 0b101, 0b111],
        '1' => [0b010, 0b110, 0b010, 0b010, 0b111],
        '2' => [0b111, 0b001, 0b111, 0b100, 0b111],
        '3' => [0b111, 0b001, 0b111, 0b001, 0b111],
        '4' => [0b101, 0b101, 0b111, 0b001, 0b001],
        '5' => [0b111, 0b100, 0b111, 0b001, 0b111],
        '6' => [0b111, 0b100, 0b111, 0b101, 0b111],
        '7' => [0b111, 0b001, 0b001, 0b001, 0b001],
        '8' => [0b111, 0b101, 0b111, 0b101, 0b111],
        '9' => [0b111, 0b101, 0b111, 0b001, 0b111],
        '-' => [0b000, 0b000, 0b111, 0b000, 0b000],
        '.' => [0b000, 0b000, 0b000, 0b000, 0b010],
        'X' | 'x' => [0b101, 0b101, 0b010, 0b101, 0b101],
        'Z' | 'z' => [0b111, 0b001, 0b010, 0b100, 0b111],
        _ => [0; 5],
    }
}

fn text_width(s: &str, scale: i32) -> i32 {
    (s.chars().count() as i32 * 4 * scale - scale).max(0)
}

fn mix(dst: u32, src: u32, a: f32) -> u32 {
    let m = |sh: u32| {
        let d = ((dst >> sh) & 0xFF) as f32;
        let s = ((src >> sh) & 0xFF) as f32;
        ((d + (s - d) * a) as u32 & 0xFF) << sh
    };
    m(16) | m(8) | m(0)
}

struct Canvas<'a> {
    buf: &'a mut [u32],
    w: usize,
    h: usize,
}

impl Canvas<'_> {
    fn inside(&self, x: i32, y: i32) -> bool {
        x >= 0 && y >= 0 && (x as usize) < self.w && (y as usize) < self.h
    }
    fn put(&mut self, x: i32, y: i32, c: u32) {
        if self.inside(x, y) {
            self.buf[y as usize * self.w + x as usize] = c;
        }
    }
    fn blend(&mut self, x: i32, y: i32, c: u32, a: f32) {
        if self.inside(x, y) {
            let i = y as usize * self.w + x as usize;
            self.buf[i] = mix(self.buf[i], c, a);
        }
    }
    fn vline(&mut self, x: i32, y0: i32, y1: i32, c: u32, a: f32) {
        for y in y0..y1 {
            self.blend(x, y, c, a);
        }
    }
    fn hline(&mut self, y: i32, x0: i32, x1: i32, c: u32, a: f32) {
        for x in x0..x1 {
            self.blend(x, y, c, a);
        }
    }
    fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, c: u32, a: f32) {
        for yy in y..y + h {
            for xx in x..x + w {
                self.blend(xx, yy, c, a);
            }
        }
    }
    fn text(&mut self, x: i32, y: i32, s: &str, scale: i32, c: u32) {
        let mut cx = x;
        for ch in s.chars() {
            for (row, bits) in glyph(ch).iter().enumerate() {
                for col in 0..3i32 {
                    if (*bits >> (2 - col)) & 1 == 1 {
                        for dy in 0..scale {
                            for dx in 0..scale {
                                self.put(cx + col * scale + dx, y + row as i32 * scale + dy, c);
                            }
                        }
                    }
                }
            }
            cx += 4 * scale;
        }
    }
}

/// 1 / 2 / 5 × 10^n step closest above `target`.
fn nice_step(target: f32) -> f32 {
    let target = target.max(1e-3);
    let p = 10f32.powf(target.log10().floor());
    let n = target / p;
    let m = if n <= 1.0 {
        1.0
    } else if n <= 2.0 {
        2.0
    } else if n <= 5.0 {
        5.0
    } else {
        10.0
    };
    m * p
}

fn fmt_tick(v: f32, step: f32) -> String {
    let v = if v.abs() < step * 1e-3 { 0.0 } else { v };
    let dec = if step >= 1.0 { 0 } else { (-step.log10().floor()) as usize };
    format!("{:.*}", dec, v)
}

fn draw_grid(cv: &mut Canvas, rect: Rect, tf: &Transform) {
    let (wx0, wz0) = tf.invert(rect.x0, rect.y0);
    let (wx1, wz1) = tf.invert(rect.x1, rect.y1);
    let step = nice_step((wx1 - wx0).abs() / ((rect.x1 - rect.x0) / 90.0));
    let (rx0, ry0, rx1, ry1) = (rect.x0 as i32, rect.y0 as i32, rect.x1 as i32, rect.y1 as i32);

    // Vertical lines (constant x) with labels along the bottom edge.
    let (i0, i1) = ((wx0 / step).ceil() as i64, (wx1 / step).floor() as i64);
    if i1 - i0 < 400 {
        for i in i0..=i1 {
            let v = i as f32 * step;
            let x = tf.apply(v, 0.0).0.round() as i32;
            if x < rx0 || x >= rx1 {
                continue;
            }
            cv.vline(x, ry0, ry1, WHITE, if i == 0 { 0.40 } else { 0.12 });
            let s = fmt_tick(v, step);
            cv.text(x - text_width(&s, 2) / 2, ry1 + 7, &s, 2, TEXT);
        }
    }

    // Horizontal lines (constant z) with labels along the left edge.
    let (j0, j1) = ((wz0 / step).ceil() as i64, (wz1 / step).floor() as i64);
    if j1 - j0 < 400 {
        for j in j0..=j1 {
            let v = j as f32 * step;
            let y = tf.apply(0.0, v).1.round() as i32;
            if y < ry0 || y >= ry1 {
                continue;
            }
            cv.hline(y, rx0, rx1, WHITE, if j == 0 { 0.40 } else { 0.12 });
            let s = fmt_tick(v, step);
            cv.text(rx0 - 6 - text_width(&s, 2), y - 5, &s, 2, TEXT);
        }
    }
}

fn draw_overlay(
    cv: &mut Canvas,
    rect: Rect,
    tf: Option<&Transform>,
    model: &Model,
    cursor: Option<(f32, f32)>,
                lut: &[u32],
) {
    let (rx0, ry0, rx1, ry1) = (rect.x0 as i32, rect.y0 as i32, rect.x1 as i32, rect.y1 as i32);

    if let Some(tf) = tf {
        draw_grid(cv, rect, tf);

        // Live blob markers with their ids.
        for b in model.blobs.iter().filter(|b| matches!(b.state, State::Live)) {
            let (px, py) = tf.apply(b.x, b.z);
            let (ix, iy) = (px as i32, py as i32);
            if ix < rx0 || ix >= rx1 || iy < ry0 || iy >= ry1 {
                continue;
            }
            for dy in -2..=2 {
                for dx in -2..=2 {
                    cv.put(ix + dx, iy + dy, WHITE);
                }
            }
            cv.text(ix + 7, iy - 5, &b.id, 2, WHITE);
        }

        // Colour bar: 0 .. max weight.
        let bx1 = rx1 - 10;
        let bx0 = bx1 - 120;
        let by0 = ry0 + 10;
        for i in 0..120i32 {
            let c = lut[(i * 255 / 119) as usize];
            for y in by0..by0 + 8 {
                cv.put(bx0 + i, y, c);
            }
        }
        let maxs = if model.max_w < 10.0 {
            format!("{:.1}", model.max_w)
        } else {
            format!("{:.0}", model.max_w)
        };
        cv.text(bx0, by0 + 12, "0", 2, TEXT);
        cv.text(bx1 - text_width(&maxs, 2), by0 + 12, &maxs, 2, TEXT);

        // Crosshair and coordinate readout under the mouse.
        if let Some((mx, my)) = cursor {
            let (cx, cy) = (mx as i32, my as i32);
            if cx >= rx0 && cx < rx1 && cy >= ry0 && cy < ry1 {
                cv.vline(cx, ry0, ry1, WHITE, 0.25);
                cv.hline(cy, rx0, rx1, WHITE, 0.25);
                let (wx, wz) = tf.invert(mx, my);
                let s = format!("X {:.0}  Z {:.0}", wx, wz);
                cv.fill_rect(rx0 + 4, ry0 + 4, text_width(&s, 2) + 12, 18, 0x0000_0000, 0.65);
                cv.text(rx0 + 10, ry0 + 8, &s, 2, WHITE);
            }
        }
    }

    // Plot frame.
    cv.hline(ry0, rx0, rx1, FRAME, 1.0);
    cv.hline(ry1, rx0, rx1 + 1, FRAME, 1.0);
    cv.vline(rx0, ry0, ry1 + 1, FRAME, 1.0);
    cv.vline(rx1, ry0, ry1 + 1, FRAME, 1.0);
}

struct App {
    radius: f32,
    rx: mpsc::Receiver<Reading>,
    model: Model,
    window: Option<Arc<Window>>,
    context: Option<softbuffer::Context<Arc<Window>>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    field: Vec<f32>,
    lut: Vec<u32>,
    cursor: Option<(f32, f32)>,
    last_title: Instant,
    title_base: String,
}

impl App {
    fn render(&mut self) {
        let Some(window) = self.window.clone() else { return };
        let Some(surface) = self.surface.as_mut() else { return };

        let size = window.inner_size();
        let (Some(nw), Some(nh)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return; // minimised
        };
        if surface.resize(nw, nh).is_err() {
            return;
        }
        let (w, h) = (size.width as usize, size.height as usize);

        let now = Instant::now();
        self.model.tick(now);

        let rect = Rect {
            x0: M_LEFT,
            y0: M_TOP,
            x1: w as f32 - M_RIGHT,
            y1: h as f32 - M_BOTTOM,
        };
        let usable = rect.x1 - rect.x0 > 100.0 && rect.y1 - rect.y0 > 100.0;
        let r = self.radius.max(2.0);
        let r2 = r * r;
        let tf = if usable { self.model.transform(rect, r + 8.0) } else { None };

        // Accumulate blob heat into the float field, clipped to the plot area.
        self.field.clear();
        self.field.resize(w * h, 0.0);
        if let Some(tf) = &tf {
            let (cx0, cy0) = (rect.x0 as i32, rect.y0 as i32);
            let (cx1, cy1) = (rect.x1 as i32 - 1, rect.y1 as i32 - 1);
            for b in &self.model.blobs {
                let amp = b.weight / self.model.max_w * self.model.fade(b, now);
                if amp <= 0.0 {
                    continue;
                }
                let (px, py) = tf.apply(b.x, b.z);
                let x0 = ((px - r).floor() as i32).max(cx0);
                let x1 = ((px + r).ceil() as i32).min(cx1);
                let y0 = ((py - r).floor() as i32).max(cy0);
                let y1 = ((py + r).ceil() as i32).min(cy1);
                for y in y0..=y1 {
                    let dy = y as f32 + 0.5 - py;
                    for x in x0..=x1 {
                        let dx = x as f32 + 0.5 - px;
                        let d2 = (dx * dx + dy * dy) / r2;
                        if d2 < 1.0 {
                            let k = 1.0 - d2;
                            self.field[y as usize * w + x as usize] += amp * k * k;
                        }
                    }
                }
            }
        }

        let Ok(mut buf) = surface.buffer_mut() else { return };
        buf.fill(BG);
        if usable {
            let (rx0, rx1) = (rect.x0 as usize, rect.x1 as usize);
            let (ry0, ry1) = (rect.y0 as usize, rect.y1 as usize);
            for y in ry0..ry1 {
                for x in rx0..rx1 {
                    let i = y * w + x;
                    buf[i] = self.lut[(self.field[i].clamp(0.0, 1.0) * 255.0) as usize];
                }
            }
            let mut cv = Canvas { buf: &mut buf[..], w, h };
            draw_overlay(&mut cv, rect, tf.as_ref(), &self.model, self.cursor, &self.lut);
        }
        let _ = buf.present();

        if self.last_title.elapsed() > Duration::from_millis(500) {
            self.last_title = Instant::now();
            let live = self.model.blobs.iter().filter(|b| matches!(b.state, State::Live)).count();
            let dec = self.model.blobs.len() - live;
            window.set_title(&format!("{} — {} live, {} decaying", self.title_base, live, dec));
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
        .with_title(self.title_base.clone())
        .with_inner_size(LogicalSize::new(900.0, 700.0));
        let window = Arc::new(el.create_window(attrs).expect("failed to create window"));
        let context = softbuffer::Context::new(window.clone()).expect("softbuffer context");
        let surface = softbuffer::Surface::new(&context, window.clone()).expect("softbuffer surface");
        self.window = Some(window);
        self.context = Some(context);
        self.surface = Some(surface);
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::KeyboardInput { event, .. }
            if event.state.is_pressed() && event.logical_key == Key::Named(NamedKey::Escape) =>
            {
                el.exit()
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = Some((position.x as f32, position.y as f32));
            }
            WindowEvent::CursorLeft { .. } => self.cursor = None,
            WindowEvent::RedrawRequested => self.render(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        while let Ok(r) = self.rx.try_recv() {
            self.model.ingest(r);
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
        el.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(33)));
    }
}

pub fn run(args: Args) -> Result<(), Box<dyn Error>> {
    let (tx, rx) = mpsc::channel();
    {
        let a = args.clone();
        thread::spawn(move || tail_thread(a, tx));
    }

    let title_base = format!(
        "heat map — {}",
        args.path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    );
    let mut app = App {
        radius: args.radius,
        rx,
        model: Model::new(&args),
        window: None,
        context: None,
        surface: None,
        field: Vec::new(),
        lut: build_lut(),
        cursor: None,
        last_title: Instant::now(),
        title_base,
    };

    let event_loop = EventLoop::new()?;
    event_loop.run_app(&mut app)?;
    Ok(())
}
