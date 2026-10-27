// SPDX-License-Identifier: MIT
//! The Tuning tab: the camera's picture, live, with snapshots, and its image
//! controls around it. The camera keeps every change it accepts (it saves them
//! itself about two seconds after the last one), so there is nothing to save here.
//! Video and controls each run on a thread of their own, so a camera that
//! stops answering can't hold up the window.
//!
//! For a camera on another computer (remote mode) the picture is a snapshot
//! taken on request, or twice a second with Auto snapshot, instead of live
//! video; the controls work the same.

use crate::overlay::{self, Item, Metering};
use eframe::egui::{self, RichText};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::controls::{Def, Group, Tuner, Value, DEFS};
use crate::remote::{Remote, RemoteTuner, Snap};

#[cfg(target_os = "linux")]
use crate::v4l2 as video;

/// Elsewhere there is no V4L2 here; the tab says so, or works through a daemon.
#[cfg(not(target_os = "linux"))]
mod video {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    pub fn node() -> Option<PathBuf> {
        None
    }
    pub struct Stream {
        pub width: u32,
        pub height: u32,
    }
    impl Stream {
        pub fn start(_: &Path, _: u32, _: u32, _: u32) -> Result<Self, String> {
            Err("not available on this system".into())
        }
        pub fn frame(&mut self, _: Duration) -> Result<Option<Vec<u8>>, String> {
            Ok(None)
        }
    }
}

/// Auto snapshot asks this often at most; a slower answer just means fewer.
const AUTO_EVERY: Duration = Duration::from_millis(500);

/// A snapshot asked of the daemon, decoded on its way.
type Asking = Receiver<Result<(Snap, Arc<egui::ColorImage>), String>>;

/// A decoded frame, with the JPEG it came from for snapshots.
struct Frame {
    jpeg: Vec<u8>,
    image: Arc<egui::ColorImage>,
    fps: f32,
}

enum StreamMsg {
    Started(u32, u32),
    Failed(String),
}

/// The capture thread and what it hands over.
struct Capture {
    stop: Arc<AtomicBool>,
    latest: Arc<Mutex<Option<Frame>>>,
    rx: Receiver<StreamMsg>,
    thread: JoinHandle<()>,
    /// The size the camera agreed to, once streaming.
    size: Option<(u32, u32)>,
}

fn decode(jpeg: &[u8]) -> Option<egui::ColorImage> {
    let img = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg).ok()?.to_rgb8();
    Some(egui::ColorImage::from_rgb([img.width() as usize, img.height() as usize], img.as_raw()))
}

fn capture(
    node: PathBuf,
    size: (u32, u32),
    fps: u32,
    stop: Arc<AtomicBool>,
    latest: Arc<Mutex<Option<Frame>>>,
    tx: Sender<StreamMsg>,
    ctx: egui::Context,
) {
    let mut s = match video::Stream::start(&node, size.0, size.1, fps) {
        Ok(s) => s,
        Err(e) => {
            let _ = tx.send(StreamMsg::Failed(e));
            ctx.request_repaint();
            return;
        }
    };
    let _ = tx.send(StreamMsg::Started(s.width, s.height));
    let mut times: VecDeque<Instant> = VecDeque::new();
    let mut next = Duration::from_millis(250);
    while !stop.load(Ordering::Relaxed) {
        // Every frame counts toward the rate; only the newest is decoded, so
        // the picture keeps up even where decoding is slower than the camera.
        let mut jpeg = None;
        while !stop.load(Ordering::Relaxed) {
            match s.frame(next) {
                Ok(Some(f)) => {
                    times.push_back(Instant::now());
                    jpeg = Some(f);
                    next = Duration::ZERO;
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = tx.send(StreamMsg::Failed(e));
                    ctx.request_repaint();
                    return;
                }
            }
        }
        next = Duration::from_millis(250);
        let Some(jpeg) = jpeg else { continue };
        let now = Instant::now();
        while times.front().is_some_and(|t| now - *t > Duration::from_secs(2)) {
            times.pop_front();
        }
        let fps = match (times.front(), times.len()) {
            (Some(t), n) if n > 1 && now > *t => (n - 1) as f32 / (now - *t).as_secs_f32(),
            _ => 0.0,
        };
        // A damaged frame is skipped; the next one replaces it.
        if let Some(image) = decode(&jpeg) {
            *latest.lock().unwrap() = Some(Frame { jpeg, image: Arc::new(image), fps });
            ctx.request_repaint();
        }
    }
    ctx.request_repaint();
}

enum Req {
    Load,
    Set(usize, i32),
    Reset,
}

enum Resp {
    /// The MJPEG frame sizes the camera offers.
    Sizes(Result<Vec<(u32, u32)>, String>),
    Loaded(Result<Vec<Option<Value>>, String>),
    /// The value asked for, and what the camera then reports.
    Set(usize, i32, Result<i32, String>),
    Reset(Result<(), String>),
    /// Where the controls an automatic mode sets are now.
    Live(Vec<(usize, i32)>),
}

/// How often those are read while the tab shows.
const LIVE: Duration = Duration::from_secs(1);

/// Opens the controls, on the worker's thread: a camera that stops answering
/// can hold an open up.
pub type OpenTuner = Box<dyn FnOnce() -> Result<Box<dyn Tuner>, String> + Send>;

/// The controls on this computer's video node.
fn local_tuner(node: PathBuf) -> OpenTuner {
    #[cfg(target_os = "linux")]
    return Box::new(move || Ok(Box::new(crate::controls::Local::open(&node)?) as Box<dyn Tuner>));
    #[cfg(not(target_os = "linux"))]
    return Box::new(move || Err(format!("{}: not available on this system", node.display())));
}

fn worker(open: OpenTuner, rx: Receiver<Req>, tx: Sender<Resp>, stop: Arc<AtomicBool>, watch: Arc<AtomicBool>, ctx: egui::Context) {
    let ctl = match open() {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(Resp::Loaded(Err(e)));
            ctx.request_repaint();
            return;
        }
    };
    let _ = tx.send(Resp::Sizes(ctl.sizes()));
    // The controls an automatic mode sets (inactive ones), followed while `watch` is set.
    let mut auto: Vec<usize> = Vec::new();
    loop {
        let first = match rx.recv_timeout(LIVE) {
            Ok(r) => Some(r),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let Some(first) = first else {
            if watch.load(Ordering::Relaxed) && !auto.is_empty() {
                let _ = tx.send(Resp::Live(ctl.get(&auto)));
                ctx.request_repaint();
            }
            continue;
        };
        // Only the newest value of each control matters (a slider sends many).
        let (mut load, mut reset, mut sets) = (false, false, Vec::<(usize, i32)>::new());
        for r in std::iter::once(first).chain(rx.try_iter()) {
            match r {
                Req::Load => load = true,
                Req::Reset => reset = true,
                Req::Set(i, v) => {
                    sets.retain(|&(j, _)| j != i);
                    sets.push((i, v));
                }
            }
        }
        if reset {
            let _ = tx.send(Resp::Reset(ctl.reset()));
            load = true;
        }
        for (i, v) in sets {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let r = ctl.set(i, v);
            // A refused value is read back, so that what shows is what applies.
            load |= DEFS[i].governs() || r.is_err();
            let _ = tx.send(Resp::Set(i, v, r));
        }
        if load && !stop.load(Ordering::Relaxed) {
            let r = ctl.load();
            if let Ok(vals) = &r {
                auto = (0..vals.len()).filter(|&i| vals[i].is_some_and(|v| v.inactive)).collect();
            }
            let _ = tx.send(Resp::Loaded(r));
        }
        ctx.request_repaint();
    }
}

struct Worker {
    tx: Sender<Req>,
    rx: Receiver<Resp>,
    stop: Arc<AtomicBool>,
    /// The tab shows: follow what the automatic modes set.
    watch: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Space between the preview and the boxes the controls flow through.
const GAP: f32 = 8.0;
/// Inside a box: the margin, and the space between its columns, with a rule
/// down the middle.
const PAD: f32 = 8.0;
const RULE: f32 = 17.0;
/// The narrowest a column of controls gets, and the widest.
const COLUMN: f32 = 300.0;
const COLUMN_MAX: f32 = 440.0;
/// Between rows, more before a group's heading, and between a control's name
/// and the control.
const ROW_GAP: f32 = 4.0;
const HEAD_GAP: f32 = 6.0;
const CELL_GAP: f32 = 10.0;
/// The preview shrinks no further to keep the controls in view; below this
/// the tab scrolls.
const SMALLEST: f32 = 160.0;

/// A line of the controls: a group's heading, or one of its rows.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Line {
    group: usize,
    heading: bool,
}

/// Where a line goes. `line` is None for a heading repeated at the top of a
/// column that a group runs on into.
#[derive(Clone, Copy, Debug)]
struct Placed {
    line: Option<usize>,
    group: usize,
    rect: egui::Rect,
}

/// Where everything goes, from the top left of the tab's room: the preview,
/// the boxes the lines flow through (beside the preview, then below it), the
/// rules between the columns in use, and the lines. `height` is more than the
/// room's when the tab scrolls.
#[derive(Debug)]
struct Arrangement {
    preview: egui::Rect,
    boxes: Vec<egui::Rect>,
    rules: Vec<[egui::Pos2; 2]>,
    lines: Vec<Placed>,
    height: f32,
}

/// How many columns a box this wide holds.
fn fit(width: f32) -> usize {
    ((width - 2.0 * PAD + RULE) / (COLUMN + RULE)).floor().max(0.0) as usize
}

/// A box's columns, `k` of them, from its left.
fn columns(b: egui::Rect, k: usize) -> Vec<egui::Rect> {
    let cw = ((b.width() - 2.0 * PAD - k.saturating_sub(1) as f32 * RULE) / k.max(1) as f32).min(COLUMN_MAX);
    let at = |j: usize| b.left() + PAD + j as f32 * (cw + RULE);
    (0..k).map(|j| egui::Rect::from_x_y_ranges(at(j)..=at(j) + cw, b.top() + PAD..=b.bottom() - PAD)).collect()
}

/// The lines down the columns in turn, each `line` high: with each, the column
/// it is in. A heading keeps its first two rows with it, and a group that runs
/// on into another column has its heading repeated there. None when they don't
/// all fit, unless `run_on`, when the last column takes the rest.
fn flow(lines: &[Line], cols: &[egui::Rect], line: f32, run_on: bool) -> Option<Vec<(usize, Placed)>> {
    let mut out = Vec::with_capacity(lines.len() + cols.len());
    let (mut c, mut next) = (0, None);
    for (i, l) in lines.iter().enumerate() {
        loop {
            let col = *cols.get(c)?;
            let mut y = next.unwrap_or(col.top());
            let again = next.is_none() && !l.heading;
            let lead = if l.heading && next.is_some() { HEAD_GAP } else { 0.0 };
            let mut need = lead + line;
            if again {
                need += line + ROW_GAP;
            }
            if l.heading {
                let rows = lines[i + 1..].iter().take_while(|n| !n.heading).count().min(2);
                need += rows as f32 * (ROW_GAP + line);
            }
            if y + need > col.bottom() + 0.01 && !(run_on && c + 1 == cols.len()) {
                (c, next) = (c + 1, None);
                continue;
            }
            let at = |y: f32| egui::Rect::from_min_size(egui::pos2(col.left(), y), egui::vec2(col.width(), line));
            if again {
                out.push((c, Placed { line: None, group: l.group, rect: at(y) }));
                y += line + ROW_GAP;
            }
            y += lead;
            out.push((c, Placed { line: Some(i), group: l.group, rect: at(y) }));
            next = Some(y + line + ROW_GAP);
            break;
        }
    }
    Some(out)
}

/// The preview at the top left, as large as it can be with every control in
/// view, and the controls flowing past it the way a page wraps text around a
/// picture: down the columns of a box beside it, as tall as the picture, then
/// of a box across the room below it, so that they read in order. Only when
/// they don't fit beside a preview `SMALLEST` wide does the tab scroll.
fn arrange(room: egui::Vec2, aspect: f32, lines: &[Line], line: f32) -> Arrangement {
    let (w, h, aspect) = (room.x.max(1.0), room.y.max(1.0), aspect.max(0.1));
    let largest = w.min(h * aspect).floor().max(1.0);
    let smallest = SMALLEST.min(largest);
    let mut pw = largest;
    while pw >= smallest {
        if let Some(a) = place(w, h, pw, pw / aspect, lines, line, false) {
            return a;
        }
        pw -= 1.0;
    }
    place(w, h, smallest, smallest / aspect, lines, line, true).expect("the last column takes what is left")
}

/// The arrangement with a preview `pw` by `ph`, if the lines fit (or anyway,
/// with `run_on`).
fn place(w: f32, h: f32, pw: f32, ph: f32, lines: &[Line], line: f32, run_on: bool) -> Option<Arrangement> {
    // Beside the picture only as far down as it goes, so that nothing beside it
    // comes after what starts below it.
    let beside = egui::Rect::from_min_max(egui::pos2(pw + GAP, 0.0), egui::pos2(w, ph.min(h)));
    let below = egui::Rect::from_min_max(egui::pos2(0.0, ph + GAP), egui::pos2(w, h));
    let mut cols = columns(beside, fit(beside.width()));
    let split = cols.len();
    cols.extend(columns(below, fit(w).max(1)));
    let placed = flow(lines, &cols, line, run_on)?;
    // The boxes in use, as wide as their columns in use; the one below as tall
    // as the room or the lines run on.
    let end = placed.iter().map(|(_, p)| p.rect.bottom() + PAD).fold(h, f32::max);
    let used = |c: usize| placed.iter().any(|(at, _)| *at == c);
    let (mut boxes, mut rules) = (Vec::new(), Vec::new());
    for (b, range, bottom) in [(beside, 0..split, beside.bottom()), (below, split..cols.len(), end)] {
        let Some(last) = range.clone().rev().find(|&c| used(c)) else { continue };
        let b = egui::Rect::from_min_max(b.min, egui::pos2(cols[last].right() + PAD, bottom));
        for c in range.skip(1).filter(|c| used(*c)) {
            let x = cols[c].left() - RULE / 2.0;
            rules.push([egui::pos2(x, b.top() + PAD), egui::pos2(x, b.bottom() - PAD)]);
        }
        boxes.push(b);
    }
    let height = boxes.iter().map(|b| b.bottom()).fold(ph, f32::max);
    let preview = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(pw, ph));
    Some(Arrangement { preview, boxes, rules, lines: placed.into_iter().map(|(_, p)| p).collect(), height })
}

/// How wide a line of text is.
fn text_width(ui: &egui::Ui, text: String, style: egui::TextStyle) -> f32 {
    egui::WidgetText::from(text).into_galley(ui, Some(egui::TextWrapMode::Extend), f32::INFINITY, style).size().x
}

/// The widest a slider's value box gets.
fn value_width(ui: &egui::Ui, d: &Def, v: &Value) -> f32 {
    let style = ui.style().drag_value_text_style.clone();
    let text = |n: i32| text_width(ui, format!("{n}{}", d.suffix), style.clone());
    (text(v.min).max(text(v.max)) + 2.0 * ui.spacing().button_padding.x + 2.0).max(ui.spacing().interact_size.x)
}

/// Which overlays show over the preview.
#[derive(Default)]
struct Overlays {
    metering: bool,
    clipping: bool,
    histogram: bool,
    grid: bool,
}

/// The frame on screen measured for the overlays, in the view it shows, and
/// whether the clipping mask was made from it.
struct Measured {
    at: Instant,
    view: overlay::View,
    masked: bool,
    m: overlay::Measure,
}

/// The frame on screen.
struct Shown {
    jpeg: Vec<u8>,
    image: Arc<egui::ColorImage>,
    fps: f32,
    at: Instant,
}

pub struct Tuning {
    own: crate::gui::Own,
    /// The daemon whose camera this is, in remote mode.
    remote: Option<Arc<Remote>>,
    /// Remote: Auto snapshot is on. Off for each new connection; kept while
    /// the tab is away.
    auto: bool,
    /// Remote: the snapshot asked for (one at a time), when the last one was
    /// asked, and the newest one's sequence number.
    asking: Option<Asking>,
    snapshot_thread: Option<JoinHandle<()>>,
    asked: Option<Instant>,
    seq: u64,
    /// Remote: the host's automatic capture runs, to be stopped when not wanted.
    auto_running: bool,
    snap_error: Option<String>,
    /// When a control last changed here; a snapshot from before is stale.
    changed: Option<Instant>,
    /// Remote: the controls when the snapshot shown arrived, for the overlays.
    frozen: Vec<Option<Value>>,
    connected: bool,
    node: Option<PathBuf>,
    scanned: Option<Instant>,
    sizes: Vec<(u32, u32)>,
    size: (u32, u32),
    fps: u32,
    /// The preview should run (while the tab shows and the camera is there).
    live: bool,
    stream: Option<Capture>,
    /// Capture and control threads finishing their device access.
    retiring: Vec<JoinHandle<()>>,
    retry: Instant,
    note: String,
    shown: Option<Shown>,
    texture: Option<egui::TextureHandle>,
    worker: Option<Worker>,
    vals: Vec<Option<Value>>,
    loaded: bool,
    ctl_error: Option<String>,
    confirm_reset: bool,
    log: Vec<String>,
    overlays: Overlays,
    measured: Option<Measured>,
    mask: Option<egui::TextureHandle>,
}

const RATES: [u32; 6] = [30, 25, 20, 15, 10, 5];

/// What the toolbar says about the controls, the longest that fits.
const HINTS: [&str; 3] = [
    "The camera keeps every change; there is nothing to save. Hover over a control for what it does.",
    "The camera keeps every change. Hover over a control for what it does.",
    "The camera keeps every change.",
];

impl Tuning {
    pub fn new(own: crate::gui::Own) -> Self {
        Self {
            own,
            remote: None,
            auto: false,
            asking: None,
            snapshot_thread: None,
            asked: None,
            seq: 0,
            auto_running: false,
            snap_error: None,
            changed: None,
            frozen: Vec::new(),
            connected: false,
            node: None,
            scanned: None,
            sizes: Vec::new(),
            size: (1280, 720),
            fps: 30,
            live: true,
            stream: None,
            retiring: Vec::new(),
            retry: Instant::now(),
            note: String::new(),
            shown: None,
            texture: None,
            worker: None,
            vals: Vec::new(),
            loaded: false,
            ctl_error: None,
            confirm_reset: false,
            log: Vec::new(),
            overlays: Overlays::default(),
            measured: None,
            mask: None,
        }
    }

    /// Connected, or its threads are still letting go of the camera.
    pub fn in_use(&self) -> bool {
        self.connected || !self.retiring.is_empty()
    }

    /// Lines for the app's log (saved snapshots, resets).
    pub fn take_log(&mut self) -> Vec<String> {
        std::mem::take(&mut self.log)
    }

    /// Here, or another computer's camera: whatever was connected disconnects.
    pub fn set_remote(&mut self, remote: Option<Arc<Remote>>) {
        self.disconnect();
        self.remote = remote;
    }

    /// Follows a connection the user requested; an attachment alone does no I/O.
    pub fn poll(&mut self, ctx: &egui::Context, visible: bool, camera_up: bool) {
        self.retiring.retain(|t| !t.is_finished());
        if !camera_up {
            self.disconnect();
        }
        if !self.connected {
            return;
        }
        if self.remote.is_some() {
            self.poll_snapshots(ctx, visible);
        } else {
            if self.node.is_none() && self.scanned.is_none_or(|t| t.elapsed() > Duration::from_secs(1)) {
                self.scanned = Some(Instant::now());
                self.node = video::node();
            }
            self.poll_stream(ctx, visible);
        }
        self.poll_controls(ctx, visible);
    }

    /// Remote: takes in an answered snapshot, and asks for the next one while
    /// Auto snapshot is on and the tab shows. Nothing is asked otherwise.
    fn poll_snapshots(&mut self, ctx: &egui::Context, visible: bool) {
        if let Some(rx) = &self.asking {
            match rx.try_recv() {
                Ok(Ok((snap, image))) => {
                    self.asking = None;
                    let opts = egui::TextureOptions::LINEAR;
                    match &mut self.texture {
                        Some(t) => t.set(image.clone(), opts),
                        None => self.texture = Some(ctx.load_texture("preview", image.clone(), opts)),
                    }
                    self.seq = snap.seq;
                    let at = Instant::now().checked_sub(snap.age).unwrap_or_else(Instant::now);
                    self.shown = Some(Shown { jpeg: snap.jpeg, image, fps: 0.0, at });
                    self.frozen = self.vals.clone();
                    self.snap_error = None;
                }
                // A failed one leaves the last picture.
                Ok(Err(e)) => {
                    self.asking = None;
                    self.snap_error = Some(e);
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.asking = None,
            }
        }
        let auto = self.auto && visible;
        if auto && self.asking.is_none() && self.asked.is_none_or(|t| t.elapsed() >= AUTO_EVERY) {
            self.ask(ctx, true);
        }
        // The host stops its capture by itself a few seconds after the last
        // request; it is told at once.
        if !auto && self.auto_running && self.asking.is_none() {
            self.auto_running = false;
            if let Some(r) = self.remote.clone() {
                self.retiring.push(std::thread::spawn(move || r.stop_snapshots()));
            }
        }
        if auto || self.shown.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    /// Asks the daemon for a snapshot, unless one is on its way.
    fn ask(&mut self, ctx: &egui::Context, auto: bool) {
        let Some(r) = self.remote.clone() else { return };
        if self.asking.is_some() {
            return;
        }
        let (tx, rx) = channel();
        let (size, after, ctx) = (self.size, if auto { self.seq } else { 0 }, ctx.clone());
        self.snapshot_thread = Some(std::thread::spawn(move || {
            let got = r.snapshot(size, auto, after).and_then(|s| {
                let image = decode(&s.jpeg).ok_or("the snapshot is not a whole JPEG")?;
                Ok((s, Arc::new(image)))
            });
            let _ = tx.send(got);
            ctx.request_repaint();
        }));
        self.asked = Some(Instant::now());
        self.auto_running |= auto;
        self.asking = Some(rx);
    }

    fn poll_stream(&mut self, ctx: &egui::Context, visible: bool) {
        if let Some(c) = &mut self.stream {
            let mut failed = None;
            loop {
                match c.rx.try_recv() {
                    Ok(StreamMsg::Started(w, h)) => {
                        c.size = Some((w, h));
                        self.note.clear();
                    }
                    Ok(StreamMsg::Failed(e)) => failed = Some(e),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break,
                }
            }
            if let Some(f) = c.latest.lock().unwrap().take() {
                let opts = egui::TextureOptions::LINEAR;
                match &mut self.texture {
                    Some(t) => t.set(f.image.clone(), opts),
                    None => self.texture = Some(ctx.load_texture("preview", f.image.clone(), opts)),
                }
                self.shown = Some(Shown { jpeg: f.jpeg, image: f.image, fps: f.fps, at: Instant::now() });
            }
            if let Some(e) = failed {
                // Tried again in a moment: a camera that reboots comes back.
                self.note = e;
                self.retire();
                self.retry = Instant::now() + Duration::from_secs(3);
            }
        }
        let want = visible && self.live && self.node.is_some();
        let free = self.retiring.is_empty();
        if want && self.stream.is_none() && free && Instant::now() >= self.retry {
            self.start(ctx);
        } else if !want && self.stream.is_some() {
            self.retire();
        }
    }

    fn start(&mut self, ctx: &egui::Context) {
        let Some(node) = self.node.clone() else { return };
        let (tx, rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let latest = Arc::new(Mutex::new(None));
        let (size, fps, ctx) = (self.size, self.fps, ctx.clone());
        let (stop2, latest2) = (stop.clone(), latest.clone());
        let thread = std::thread::spawn(move || capture(node, size, fps, stop2, latest2, tx, ctx));
        self.stream = Some(Capture { stop, latest, rx, thread, size: None });
    }

    fn retire(&mut self) {
        if let Some(c) = self.stream.take() {
            c.stop.store(true, Ordering::Relaxed);
            self.retiring.push(c.thread);
        }
    }

    fn retire_worker(&mut self) {
        if let Some(mut w) = self.worker.take() {
            w.stop.store(true, Ordering::Relaxed);
            if let Some(t) = w.thread.take() {
                self.retiring.push(t);
            }
        }
    }

    fn disconnect(&mut self) {
        let remote = self.remote.clone().filter(|_| self.connected);
        self.auto = false;
        self.asking = None;
        self.asked = None;
        self.seq = 0;
        self.auto_running = false;
        self.snap_error = None;
        self.changed = None;
        self.frozen.clear();
        self.connected = false;
        self.retire();
        self.retire_worker();
        if let Some(t) = self.snapshot_thread.take() {
            self.retiring.push(t);
        }
        if let Some(r) = remote {
            // Workers and snapshots may still have requests on the way. Wait
            // for them before closing tuning, and let jobs wait for that close.
            let retiring = std::mem::take(&mut self.retiring);
            self.retiring.push(std::thread::spawn(move || {
                for t in retiring {
                    let _ = t.join();
                }
                r.close_tuning();
            }));
        }
        self.node = None;
        self.scanned = None;
        self.loaded = false;
        self.ctl_error = None;
        self.confirm_reset = false;
        self.vals.clear();
        self.note.clear();
        self.shown = None;
        self.texture = None;
        self.measured = None;
        self.mask = None;
    }

    /// Transfer all active threads to the job so it can wait for their devices.
    pub fn stop(&mut self) -> Vec<JoinHandle<()>> {
        self.disconnect();
        std::mem::take(&mut self.retiring)
    }

    fn restart(&mut self) {
        self.retire();
        self.retry = Instant::now();
    }

    fn poll_controls(&mut self, ctx: &egui::Context, visible: bool) {
        if visible && self.worker.is_none() && self.retiring.is_empty() && self.ctl_error.is_none() {
            let open: Option<OpenTuner> = match (&self.remote, &self.node) {
                (Some(r), _) => {
                    let r = r.clone();
                    Some(Box::new(move || Ok(Box::new(RemoteTuner(r)) as Box<dyn Tuner>)))
                }
                (None, Some(node)) => Some(local_tuner(node.clone())),
                (None, None) => None,
            };
            if let Some(open) = open {
                let (tx, wrx) = channel();
                let (wtx, rx) = channel();
                let (stop, watch) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
                let (stop2, watch2) = (stop.clone(), watch.clone());
                let ctx = ctx.clone();
                let thread = std::thread::spawn(move || worker(open, wrx, wtx, stop2, watch2, ctx));
                let _ = tx.send(Req::Load);
                self.worker = Some(Worker { tx, rx, stop, watch, thread: Some(thread) });
            }
        }
        let Some(w) = &self.worker else { return };
        w.watch.store(visible, Ordering::Relaxed);
        let (mut lost, mut restart) = (false, false);
        loop {
            match w.rx.try_recv() {
                Ok(Resp::Sizes(Ok(sizes))) => {
                    if !sizes.contains(&self.size) {
                        self.size = sizes[0];
                        restart = true;
                    }
                    self.sizes = sizes;
                }
                Ok(Resp::Sizes(Err(e))) => self.note = e,
                Ok(Resp::Loaded(Ok(vals))) => {
                    self.vals = vals;
                    self.loaded = true;
                }
                Ok(Resp::Loaded(Err(e))) => {
                    self.ctl_error = Some(e);
                    lost = true;
                }
                Ok(Resp::Set(i, asked, r)) => match r {
                    // Unless the user has moved it on since.
                    Ok(v) => {
                        if let Some(val) = self.vals.get_mut(i).and_then(Option::as_mut).filter(|val| val.value == asked) {
                            val.value = v;
                        }
                    }
                    Err(e) => self.log.push(format!("Setting {} to {asked} failed: {e}", DEFS[i].name)),
                },
                Ok(Resp::Reset(r)) => self.log.push(match r {
                    Ok(()) => "Image controls reset to factory defaults".into(),
                    Err(e) => format!("FAILED: {e}"),
                }),
                // Only while the mode still sets them.
                Ok(Resp::Live(live)) => {
                    for (i, v) in live {
                        if let Some(val) = self.vals.get_mut(i).and_then(Option::as_mut).filter(|val| val.inactive) {
                            val.value = v;
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    lost = true;
                    break;
                }
            }
        }
        if lost {
            self.retire_worker();
        }
        if restart {
            self.restart();
        }
    }

    fn send(&self, req: Req) {
        if let Some(w) = &self.worker {
            let _ = w.tx.send(req);
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, camera_up: bool) {
        if cfg!(not(target_os = "linux")) && self.remote.is_none() {
            ui.label("The live preview and the image controls need Linux (V4L2), or a camera on another computer: see the connection menu at the top right.");
            return;
        }
        if !camera_up {
            ui.label(RichText::new("No camera running k1cam firmware is attached.").color(ui.visuals().weak_text_color()));
            return;
        }
        self.toolbar(ui);
        if !self.connected {
            return;
        }
        ui.add_space(6.0);
        let rows = self.lines();
        let lines: Vec<Line> = rows.iter().map(|&(g, c)| Line { group: g as usize, heading: c.is_none() }).collect();
        let aspect = match &self.shown {
            Some(s) => s.image.size[0] as f32 / s.image.size[1] as f32,
            None => self.size.0 as f32 / self.size.1 as f32,
        };
        let line = ui.spacing().interact_size.y;
        egui::ScrollArea::vertical().id_salt("tuning").auto_shrink([false, false]).show(ui, |ui| {
            let room = ui.max_rect();
            let a = arrange(room.size(), aspect, &lines, line);
            let origin = room.min.to_vec2();
            self.preview_ui(ui, a.preview.translate(origin));
            self.controls_ui(ui, &a, &rows, origin);
            ui.advance_cursor_after_rect(egui::Rect::from_min_size(room.min, egui::vec2(room.width(), a.height)));
        });
        self.reset_prompt(ui);
    }

    /// Connect; once connected, the preview's settings and snapshots, and on
    /// the right what the controls are doing and their factory reset.
    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if !self.connected {
                if ui.add_enabled(self.retiring.is_empty(), egui::Button::new("Connect")).clicked() {
                    // Here, this computer's flashers take turns with the camera.
                    match if self.remote.is_some() { Ok(()) } else { self.own.hold() } {
                        Ok(()) => {
                            self.note.clear();
                            self.connected = true;
                            self.retry = Instant::now();
                        }
                        Err(e) => self.note = e,
                    }
                }
                if self.note.is_empty() {
                    ui.label(RichText::new("to preview the camera and change its image controls").color(ui.visuals().weak_text_color()));
                } else {
                    ui.label(RichText::new(&self.note).color(ui.visuals().warn_fg_color));
                }
                return;
            }
            if ui.button("Disconnect").clicked() {
                self.disconnect();
                return;
            }
            ui.separator();
            if self.remote.is_some() {
                self.snapshot_tools(ui);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| self.controls_state(ui));
                return;
            }
            let (label, tip) = if self.live {
                ("Pause", "Stop the preview, which frees the camera's video for other programs")
            } else {
                ("Live", "Start the preview")
            };
            if ui.button(label).on_hover_text(tip).clicked() {
                self.live = !self.live;
                self.retry = Instant::now();
            }
            let (size, fps) = (self.size, self.fps);
            egui::ComboBox::from_id_salt("preview-size").selected_text(format!("{}×{}", size.0, size.1)).show_ui(ui, |ui| {
                for &s in &self.sizes {
                    ui.selectable_value(&mut self.size, s, format!("{}×{}", s.0, s.1));
                }
            });
            egui::ComboBox::from_id_salt("preview-fps").width(70.0).selected_text(format!("{fps} fps")).show_ui(ui, |ui| {
                for r in RATES {
                    ui.selectable_value(&mut self.fps, r, format!("{r} fps"));
                }
            });
            if (self.size, self.fps) != (size, fps) {
                self.restart();
            }
            let have = self.shown.is_some();
            if ui
                .add_enabled(have, egui::Button::new("Snapshot…"))
                .on_hover_text("Save the frame on screen as the camera sent it (JPEG)")
                .clicked()
            {
                self.snapshot();
            }
            if ui.add_enabled(have, egui::Button::new("Copy")).on_hover_text("Copy the frame on screen").clicked() {
                if let Some(s) = &self.shown {
                    ui.ctx().copy_image((*s.image).clone());
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| self.controls_state(ui));
        });
    }

    /// Remote: Snapshot, Auto snapshot, the size, and saving the picture shown.
    fn snapshot_tools(&mut self, ui: &mut egui::Ui) {
        let pending = self.asking.is_some();
        let why = if self.auto { "Auto snapshot is taking them" } else { "Taking one…" };
        if ui
            .add_enabled(!pending && !self.auto, egui::Button::new("Snapshot"))
            .on_hover_text("Take a picture now and show it")
            .on_disabled_hover_text(why)
            .clicked()
        {
            self.ask(ui.ctx(), false);
        }
        let r = ui.checkbox(&mut self.auto, "Auto snapshot (2 fps)");
        if r.on_hover_text("Take one twice a second while this tab shows; the camera's video runs meanwhile").changed() && self.auto {
            // The first one at once.
            self.asked = None;
        }
        let size = self.size;
        egui::ComboBox::from_id_salt("preview-size").selected_text(format!("{}×{}", size.0, size.1)).show_ui(ui, |ui| {
            for &s in &self.sizes {
                ui.selectable_value(&mut self.size, s, format!("{}×{}", s.0, s.1));
            }
        });
        let have = self.shown.is_some();
        if ui
            .add_enabled(have, egui::Button::new("Save snapshot…"))
            .on_hover_text("Save the snapshot on screen as the camera sent it (JPEG), without taking another")
            .clicked()
        {
            self.snapshot();
        }
        if ui.add_enabled(have, egui::Button::new("Copy")).on_hover_text("Copy the snapshot on screen").clicked() {
            if let Some(s) = &self.shown {
                ui.ctx().copy_image((*s.image).clone());
            }
        }
    }

    /// Right to left: the factory reset, then what the controls are doing.
    fn controls_state(&mut self, ui: &mut egui::Ui) {
        let weak = ui.visuals().weak_text_color();
        if let Some(e) = self.ctl_error.clone() {
            if ui.button("Try again").clicked() {
                self.ctl_error = None;
            }
            let text = RichText::new(format!("The camera's controls did not answer: {e}")).color(ui.visuals().warn_fg_color);
            ui.add(egui::Label::new(text).truncate());
            return;
        }
        let ready = self.worker.is_some() && self.loaded;
        if ui
            .add_enabled(ready, egui::Button::new("Factory defaults…"))
            .on_hover_text("Put every control back to its factory default")
            .clicked()
        {
            self.confirm_reset = true;
        }
        let loading = self.worker.is_some() && !self.loaded;
        // As much as fits whole, left of the spinner while loading.
        let room = ui.available_width() - if loading { ui.spacing().interact_size.y + ui.spacing().item_spacing.x } else { 0.0 };
        let says: &[&str] = if loading { &["Reading the camera's controls…"] } else { &HINTS };
        if let Some(t) = says.iter().find(|t| text_width(ui, t.to_string(), egui::TextStyle::Body) <= room) {
            ui.label(RichText::new(*t).color(weak));
        }
        if loading {
            ui.spinner();
        }
    }

    /// The picture in `rect` with the overlays chosen, or why there is none.
    fn preview_ui(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        let streaming = self.stream.as_ref().is_some_and(|c| c.size.is_some());
        let (Some(tex), Some(s)) = (&self.texture, &self.shown) else {
            let text: String = if let Some(r) = &self.remote {
                if self.asking.is_some() {
                    format!("Taking a snapshot on {}…", r.host)
                } else if let Some(e) = &self.snap_error {
                    format!("No snapshot: {e}")
                } else {
                    "No snapshot yet. Snapshot takes one now; Auto snapshot takes one twice a second while this tab shows. \
                     Nothing is captured until you ask."
                        .into()
                }
            } else if self.node.is_none() {
                "The camera's video device was not found (/dev/video*).".into()
            } else if !self.note.is_empty() {
                self.note.clone()
            } else if self.live {
                "Starting the preview…".into()
            } else {
                "The preview is paused.".into()
            };
            ui.painter().rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);
            ui.scope_builder(egui::UiBuilder::new().max_rect(rect.shrink(12.0)), |ui| {
                ui.centered_and_justified(|ui| ui.label(RichText::new(text).color(ui.visuals().weak_text_color())));
            });
            return;
        };
        let (image, at) = (s.image.clone(), s.at);
        // What is shown, and why it may be stale.
        let [w, h] = image.size;
        let (mut caption, mut warn) = (format!("{w}×{h}"), String::new());
        if self.remote.is_some() {
            caption += &format!("  ·  {} KiB  ·  {}", s.jpeg.len().div_ceil(1024), age(at.elapsed()));
            if self.auto {
                caption += "  ·  auto, twice a second";
            } else if self.changed.is_some_and(|c| c > at) {
                warn += "  ·  taken before your last change";
            }
            if let Some(e) = &self.snap_error {
                warn += &format!("  ·  {e}; showing the last good one");
            }
        } else if streaming && at.elapsed() < Duration::from_secs(2) {
            caption += &format!("  ·  {:.0} fps  ·  {} KiB per frame", s.fps, s.jpeg.len().div_ceil(1024));
        } else if !self.live {
            caption += "  ·  paused";
        } else if !self.note.is_empty() {
            caption += &format!("  ·  {}", self.note);
        } else {
            caption += "  ·  waiting for frames";
        }
        egui::Image::new((tex.id(), rect.size())).corner_radius(4.0).paint_at(ui, rect);
        let painter = ui.painter_at(rect);
        self.overlays_ui(ui.ctx(), &painter, rect, &image, at);
        // The pixel under the pointer, for checking colours.
        if let Some(p) = ui.interact(rect, ui.scope_id().with("preview"), egui::Sense::hover()).hover_pos() {
            let x = (((p.x - rect.left()) / rect.width() * w as f32) as usize).min(w - 1);
            let y = (((p.y - rect.top()) / rect.height() * h as f32) as usize).min(h - 1);
            let c = image.pixels[y * w + x];
            caption += &format!("  ·  at {x}, {y}: R {} G {} B {}", c.r(), c.g(), c.b());
        }
        let font = egui::FontId::proportional(12.0);
        let mut job = egui::text::LayoutJob::default();
        job.append(&caption, 0.0, egui::TextFormat::simple(font.clone(), egui::Color32::WHITE));
        job.append(&warn, 0.0, egui::TextFormat::simple(font, egui::Color32::from_rgb(255, 179, 71)));
        let galley = painter.layout_job(job);
        let at = rect.left_bottom() + egui::vec2(6.0, -6.0 - galley.size().y);
        painter.rect_filled(egui::Rect::from_min_size(at, galley.size()).expand(4.0), 4.0, overlay::BACKDROP);
        painter.galley(at, galley, egui::Color32::WHITE);
        self.overlay_menu(ui, rect);
    }

    /// A control's value as it was for the picture shown: for a snapshot,
    /// when it arrived; for live video, now.
    fn view_value(&self, name: &str) -> Option<i32> {
        if self.remote.is_none() {
            return self.value(name);
        }
        let i = DEFS.iter().position(|d| d.name == name)?;
        Some(self.frozen.get(i).copied().flatten()?.value)
    }

    /// A control's value, if the camera has it.
    fn value(&self, name: &str) -> Option<i32> {
        let i = DEFS.iter().position(|d| d.name == name)?;
        Some(self.vals.get(i).copied().flatten()?.value)
    }

    /// The overlays chosen, over the frame shown in `rect`.
    fn overlays_ui(&mut self, ctx: &egui::Context, p: &egui::Painter, rect: egui::Rect, image: &egui::ColorImage, at: Instant) {
        let o = &self.overlays;
        let (zoom, pan, tilt) =
            (self.view_value("zoom").unwrap_or(100), self.view_value("pan").unwrap_or(0), self.view_value("tilt").unwrap_or(0));
        let view = overlay::View::new(image.size, zoom, pan, tilt);
        if !(o.metering || o.clipping || o.histogram) {
            if o.grid {
                overlay::draw_grid(p, rect);
            }
            return;
        }
        // Measured once a frame, and again when the view moves.
        if self.measured.as_ref().is_none_or(|m| m.at != at || m.view != view || (o.clipping && !m.masked)) {
            let (m, mask) = overlay::measure(image, &view);
            if o.clipping {
                match &mut self.mask {
                    Some(t) => t.set(mask, egui::TextureOptions::LINEAR),
                    None => self.mask = Some(ctx.load_texture("clipping", mask, egui::TextureOptions::LINEAR)),
                }
            }
            self.measured = Some(Measured { at, view, masked: o.clipping, m });
        }
        let Some(Measured { m, .. }) = &self.measured else { return };
        let mut legend = Vec::new();
        if let (true, Some(mask)) = (o.clipping, &self.mask) {
            let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
            p.image(mask.id(), rect, uv, egui::Color32::WHITE);
            let text = format!("Clipped (stripes) {:.1}%  ·  crushed (blue) {:.1}%", m.clipped * 100.0, m.crushed * 100.0);
            legend.push(vec![Item::Text(text)]);
        }
        if o.grid {
            overlay::draw_grid(p, rect);
        }
        if o.metering {
            let mode = self.view_value("metering").and_then(Metering::from_value);
            let (slices, [min, median, max]) = overlay::slices(&m.zones);
            let none = overlay::Grid::<bool>::default();
            let (weights, marked) = match mode {
                Some(Metering::Subject) => {
                    let (w, marked) = overlay::subject(&view, &overlay::linear(&m.zones));
                    (Some(w), marked)
                }
                Some(Metering::Tuning) | None => (None, none),
                Some(mode) => (Some(overlay::weights(&view, mode)), none),
            };
            overlay::draw_metering(p, rect, &view, weights.as_ref(), &slices, &marked, mode == Some(Metering::Spot));
            let mut name = match mode {
                Some(Metering::Tuning) => "tuning default, whose weights the camera doesn't report",
                Some(Metering::Average) => "average",
                Some(Metering::Center) => "center-weighted",
                Some(Metering::Spot) => "spot",
                Some(Metering::Subject) => "subject, estimated from the picture (X: the subject)",
                None => "not reported by this camera",
            }
            .to_string();
            if self.view_value("auto-exposure") == Some(1) {
                name += "; unused, as exposure is manual";
            }
            legend.push(vec![Item::Text(format!("Metering: {name}"))]);
            let size = if weights.is_some() { "Dot size: zone weight" } else { "Rings: weight unknown" };
            legend.push(vec![Item::Text(format!("{size}  ·  colour: zone brightness"))]);
            let s = overlay::SLICES;
            legend.push(vec![
                Item::Swatch(s[0]),
                Item::Text(format!("{min:.0}")),
                Item::Swatch(s[1]),
                Item::Swatch(s[2]),
                Item::Text(format!("median {median:.0}")),
                Item::Swatch(s[3]),
                Item::Swatch(s[4]),
                Item::Text(format!("{max:.0}")),
            ]);
        }
        if o.histogram {
            let width = (rect.width() * 0.3).clamp(100.0, 256.0);
            let size = egui::vec2(width, width * 0.4);
            overlay::draw_histogram(p, egui::Rect::from_min_size(rect.right_bottom() - size - egui::vec2(6.0, 6.0), size), &m.hist);
            let (median, top) = (overlay::level(&m.hist, 0.5), overlay::level(&m.hist, 0.01));
            legend.push(vec![Item::Text(format!("Luma: median {median} (white)  ·  brightest 1% from {top} (yellow)"))]);
        }
        if !legend.is_empty() {
            overlay::draw_legend(p, rect.min + egui::vec2(6.0, 6.0), &legend);
        }
    }

    /// The overlays to choose from, at the top right of the picture.
    fn overlay_menu(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        let o = &mut self.overlays;
        let on = [o.metering, o.clipping, o.histogram, o.grid].into_iter().filter(|b| *b).count();
        let label = if on == 0 { "Overlays".to_string() } else { format!("Overlays ({on})") };
        let corner = egui::UiBuilder::new().max_rect(rect.shrink(6.0)).layout(egui::Layout::right_to_left(egui::Align::Min));
        ui.scope_builder(corner, |ui| {
            let stay = egui::containers::menu::MenuConfig::new().close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside);
            egui::containers::menu::MenuButton::new(label).config(stay).ui(ui, |ui| {
                ui.checkbox(&mut o.metering, "Metering")
                    .on_hover_text("Where the camera meters exposure (dot size) and how bright each of its zones is (colour)");
                ui.checkbox(&mut o.clipping, "Clipping").on_hover_text("Stripes over blown highlights, blue over crushed shadows");
                ui.checkbox(&mut o.histogram, "Histogram").on_hover_text("How bright the picture is, pixel by pixel");
                ui.checkbox(&mut o.grid, "Grid").on_hover_text("Thirds and the centre, for aiming the camera");
            });
        });
    }

    fn snapshot(&mut self) {
        let Some(s) = &self.shown else { return };
        let (jpeg, [w, h]) = (s.jpeg.clone(), s.image.size);
        let name = format!("k1cam-snapshot-{}.jpg", crate::ops::stamp());
        if let Some(path) = rfd::FileDialog::new().set_file_name(name).add_filter("JPEG image", &["jpg", "jpeg"]).save_file() {
            self.log.push(match std::fs::write(&path, &jpeg) {
                Ok(()) => format!("Snapshot saved: {} ({w}×{h}, {} KiB)", path.display(), jpeg.len().div_ceil(1024)),
                Err(e) => format!("FAILED: {}: {e}", path.display()),
            });
        }
    }

    /// The lines of the controls the camera has (all of them until it says):
    /// each group's heading (None) and its controls, less the switches shown
    /// on the rows of the controls they switch.
    fn lines(&self) -> Vec<(Group, Option<usize>)> {
        let has = |i: usize| !self.loaded || self.vals.get(i).is_some_and(Option::is_some);
        let on_row = |i: usize| (0..DEFS.len()).any(|j| has(j) && DEFS[j].switch.is_some_and(|(name, ..)| name == DEFS[i].name));
        let mut out = Vec::new();
        for g in Group::ALL {
            let rows: Vec<usize> = (0..DEFS.len()).filter(|&i| DEFS[i].group == g && has(i) && !on_row(i)).collect();
            if !rows.is_empty() {
                out.push((g, None));
                out.extend(rows.into_iter().map(|i| (g, Some(i))));
            }
        }
        out
    }

    /// The boxes, headings and rows where `a` puts them, moved by `origin`.
    /// Rows wait for the camera to say what its controls are.
    fn controls_ui(&mut self, ui: &mut egui::Ui, a: &Arrangement, rows: &[(Group, Option<usize>)], origin: egui::Vec2) {
        let stroke = ui.visuals().widgets.noninteractive.bg_stroke;
        for b in &a.boxes {
            ui.painter().rect_stroke(b.translate(origin), 6.0, stroke, egui::StrokeKind::Inside);
        }
        for r in &a.rules {
            ui.painter().line_segment([r[0] + origin, r[1] + origin], stroke);
        }
        // The same width for every name, so that the controls line up.
        let label = DEFS.iter().map(|d| text_width(ui, d.label.into(), egui::TextStyle::Body)).fold(0.0, f32::max);
        let ready = self.worker.is_some();
        for p in &a.lines {
            let rect = p.rect.translate(origin);
            match p.line.and_then(|i| rows[i].1) {
                Some(i) if self.loaded => self.control_row(ui, rect, i, ready, label),
                Some(_) => {}
                None => heading(ui, rect, Group::ALL[p.group], p.line.is_none()),
            }
        }
    }

    /// Asks before putting every control back to its factory default.
    fn reset_prompt(&mut self, ui: &egui::Ui) {
        if !self.confirm_reset {
            return;
        }
        let m = egui::Modal::new(egui::Id::unique("tuning-reset")).show(ui.ctx(), |ui| {
            ui.set_max_width(360.0);
            ui.heading("Reset the image controls?");
            ui.add_space(4.0);
            ui.label("Every control, the encoder settings included, goes back to its factory default.");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Reset all").clicked() {
                    self.send(Req::Reset);
                    self.changed = Some(Instant::now());
                    self.confirm_reset = false;
                }
                if ui.button("Keep them").clicked() {
                    self.confirm_reset = false;
                }
            });
        });
        if m.should_close() {
            self.confirm_reset = false;
        }
    }

    /// A control's row in `rect`: its name, `label` wide, then its slider,
    /// choice or check box, after an "auto" check box where another control
    /// makes it automatic. While automatic, the slider follows the camera.
    fn control_row(&mut self, ui: &mut egui::Ui, rect: egui::Rect, i: usize, ready: bool, label: f32) {
        let d = &DEFS[i];
        let Some(val) = self.vals[i] else { return };
        let switch = d.switch.and_then(|(name, auto, manual)| {
            let j = DEFS.iter().position(|s| s.name == name)?;
            Some((j, self.vals[j]?, auto, manual))
        });
        let mut tip = format!("{} ({}), default {}", d.name, d.id, val.default);
        if !d.help.is_empty() {
            tip = format!("{}\n\n{tip}", d.help);
        }
        if let Some((j, s, auto, _)) = switch {
            let default = if s.default == auto { "auto" } else { "not auto" };
            tip += &format!("\n\nauto: the camera sets it, and the slider follows ({} ({}), default {default})", DEFS[j].name, DEFS[j].id);
        }
        // A changed control stands out, and its name puts the default back.
        // An inactive control reports what its automatic mode chose, not a choice.
        let changed = (val.value != val.default && !val.inactive) || switch.is_some_and(|(_, s, ..)| s.value != s.default);
        let name = if changed { RichText::new(d.label).color(ui.visuals().selection.stroke.color) } else { RichText::new(d.label) };
        let (mut v, mut sw) = (val.value, switch.map(|(_, s, ..)| s.value));
        let row = egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::left_to_right(egui::Align::Center));
        ui.scope_builder(row, |ui| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
            let cell = egui::vec2(label, rect.height());
            let sense = if changed && ready { egui::Sense::click() } else { egui::Sense::hover() };
            let r = ui.allocate_ui_with_layout(cell, egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add(egui::Label::new(name).sense(sense))
            });
            let r = r.inner.on_hover_text(if changed { format!("{tip}\n\nClick to go back to the default.") } else { tip });
            if changed && r.clicked() {
                v = val.default;
                sw = switch.map(|(_, s, ..)| s.default);
            }
            ui.add_space(CELL_GAP - ui.spacing().item_spacing.x);
            if let Some((_, _, auto, manual)) = switch {
                let mut on = sw != Some(manual);
                if ui.add_enabled(ready, egui::Checkbox::new(&mut on, "auto")).changed() {
                    sw = Some(if on { auto } else { manual });
                }
            }
            let why = if switch.is_some() { "Set by the camera while auto is on" } else { "Has no effect in the current mode" };
            ui.add_enabled_ui(ready && !val.inactive, |ui| {
                if !d.choices.is_empty() {
                    let shown = d.choices.iter().find(|c| c.0 == v).map_or(v.to_string(), |c| c.1.to_string());
                    egui::ComboBox::from_id_salt(("ctl", d.id)).width(ui.available_width()).selected_text(shown).show_ui(ui, |ui| {
                        for &(cv, cl) in d.choices.iter().filter(|c| (val.min..=val.max).contains(&c.0)) {
                            ui.selectable_value(&mut v, cv, cl);
                        }
                    });
                } else if !d.check.is_empty() {
                    let mut on = v != 0;
                    ui.checkbox(&mut on, d.check);
                    v = on as i32;
                } else {
                    // The slider fills what its value box leaves.
                    ui.spacing_mut().slider_width =
                        (ui.available_width() - value_width(ui, d, &val) - ui.spacing().item_spacing.x).max(24.0);
                    let mut s = egui::Slider::new(&mut v, val.min..=val.max).step_by(val.step as f64);
                    // "auto" alone, where a value means automatic.
                    s = match d.auto {
                        Some(auto) => {
                            let unit = d.suffix.trim();
                            s.custom_formatter(
                                move |n, _| if n as i32 == auto { "auto".into() } else { format!("{n:.0} {unit}").trim_end().into() },
                            )
                            .custom_parser(move |t| match t.trim().trim_end_matches(unit).trim() {
                                "auto" => Some(auto as f64),
                                t => t.parse().ok(),
                            })
                        }
                        None => s.suffix(d.suffix),
                    };
                    ui.add(s);
                }
            })
            .response
            .on_disabled_hover_text(if val.inactive { why } else { "" });
        });
        // A manual value only holds while manual: going to automatic, the
        // value goes first; going to manual (which starts from what automatic
        // had), the switch does.
        let to_manual = switch.is_some_and(|(_, s, _, manual)| sw == Some(manual) && s.value != manual);
        let mut sets = vec![(i, v, val.value)];
        if let (Some((j, s, ..)), Some(nv)) = (switch, sw) {
            sets.insert(if to_manual { 0 } else { 1 }, (j, nv, s.value));
        }
        for (k, new, old) in sets {
            if new != old {
                if let Some(x) = self.vals[k].as_mut() {
                    x.value = new;
                }
                self.send(Req::Set(k, new));
                self.changed = Some(Instant::now());
            }
        }
    }
}

/// How old a snapshot is, for its caption.
fn age(d: Duration) -> String {
    match d.as_secs() {
        0..=9 => format!("{:.1} s old", d.as_secs_f32()),
        10..=59 => format!("taken {} s ago", d.as_secs()),
        s => format!("taken {} min ago", s / 60),
    }
}

/// A group's heading across `rect`, with a rule to its end; `again` where the
/// group runs on from another column.
fn heading(ui: &egui::Ui, rect: egui::Rect, g: Group, again: bool) {
    let p = ui.painter();
    let font = egui::TextStyle::Body.resolve(ui.style());
    let mut end = p.text(rect.left_center(), egui::Align2::LEFT_CENTER, g.title(), font.clone(), ui.visuals().strong_text_color()).right();
    if again {
        let at = egui::pos2(end + 6.0, rect.center().y);
        end = p.text(at, egui::Align2::LEFT_CENTER, "continued", font, ui.visuals().weak_text_color()).right();
    }
    if end + 8.0 < rect.right() {
        p.hline(end + 8.0..=rect.right(), rect.center().y, ui.visuals().widgets.noninteractive.bg_stroke);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_and_tab_selection_do_not_start_tuning() {
        let mut tuning = Tuning::new(crate::gui::Own::default());
        tuning.poll(&egui::Context::default(), true, true);
        assert!(tuning.node.is_none());
        assert!(tuning.stream.is_none());
        assert!(tuning.worker.is_none());
        tuning.connected = true;
        tuning.poll(&egui::Context::default(), false, false);
        tuning.poll(&egui::Context::default(), true, true);
        assert!(!tuning.connected, "replugging must require another Connect");
        assert!(tuning.stream.is_none());
        assert!(tuning.worker.is_none());
    }

    #[test]
    fn stopping_tuning_signals_and_returns_every_device_thread() {
        let mut tuning = Tuning::new(crate::gui::Own::default());
        tuning.connected = true;
        let capture_stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::new(AtomicBool::new(false));
        let (_, stream_rx) = channel();
        tuning.stream = Some(Capture {
            stop: capture_stop.clone(),
            latest: Arc::new(Mutex::new(None)),
            rx: stream_rx,
            thread: std::thread::spawn(|| {}),
            size: None,
        });
        let (tx, _) = channel();
        let (_, rx) = channel();
        let watch = Arc::new(AtomicBool::new(false));
        tuning.worker = Some(Worker { tx, rx, stop: worker_stop.clone(), watch, thread: Some(std::thread::spawn(|| {})) });
        tuning.retiring.push(std::thread::spawn(|| {}));
        let (release, waiting) = channel();
        tuning.snapshot_thread = Some(std::thread::spawn(move || waiting.recv().unwrap()));
        let threads = tuning.stop();
        assert!(capture_stop.load(Ordering::Relaxed));
        assert!(worker_stop.load(Ordering::Relaxed));
        assert_eq!(threads.len(), 4);
        release.send(()).unwrap();
        for t in threads {
            t.join().unwrap();
        }
        assert!(!tuning.connected);
        tuning.poll(&egui::Context::default(), true, true);
        assert!(tuning.stream.is_none());
        assert!(tuning.worker.is_none());
    }

    /// Remote: no picture until asked; one request at a time; Auto snapshot
    /// at most twice a second, only while the tab shows, off again for a new
    /// connection.
    #[test]
    #[cfg(target_os = "linux")]
    fn remote_snapshots_come_only_when_asked_and_one_at_a_time() {
        use crate::daemon::tests::{daemon, scratch, Fake, PASSWORD};
        use std::sync::atomic::Ordering::SeqCst;
        let dir = scratch("tuning-remote");
        let fake = Arc::new(Fake::default());
        let mut jpeg = std::io::Cursor::new(Vec::new());
        image::RgbImage::new(16, 9).write_to(&mut jpeg, image::ImageFormat::Jpeg).unwrap();
        *fake.picture.lock().unwrap() = Some(jpeg.into_inner());
        let d = daemon(&dir, fake.clone());
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        std::thread::spawn(move || crate::daemon::serve(l, d));
        let r = Remote::connect(&addr, PASSWORD).unwrap();
        let ctx = egui::Context::default();
        let mut t = Tuning::new(crate::gui::Own::default());
        t.set_remote(Some(r));
        t.connected = true;
        let settle = |t: &mut Tuning, visible: bool, ms: u64| {
            let end = Instant::now() + Duration::from_millis(ms);
            while Instant::now() < end {
                t.poll(&ctx, visible, true);
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        // Connecting, reading the controls and showing the tab take no picture.
        settle(&mut t, true, 400);
        assert!(t.loaded && !t.auto && t.shown.is_none());
        assert_eq!(fake.frames_opened.load(SeqCst), 0);
        // Snapshot asks once; asking again while it is on its way does nothing.
        t.ask(&ctx, false);
        t.ask(&ctx, false);
        settle(&mut t, true, 600);
        assert!(t.shown.is_some() && t.asking.is_none());
        assert_eq!((fake.frames_opened.load(SeqCst), fake.frames_open.load(SeqCst)), (1, 0), "one capture, stopped again");
        // A change after it makes it stale.
        t.changed = Some(Instant::now());
        assert!(t.changed > t.shown.as_ref().map(|s| s.at));
        // Auto snapshot: the first at once, then at most every 500 ms.
        t.auto = true;
        t.asked = None;
        let mut seqs = Vec::new();
        let end = Instant::now() + Duration::from_millis(1200);
        while Instant::now() < end {
            t.poll(&ctx, true, true);
            if seqs.last() != Some(&t.seq) {
                seqs.push(t.seq);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!((3..=4).contains(&seqs.len()), "{seqs:?}");
        // Hidden: no more requests, and the host's capture stops.
        let before = t.seq;
        settle(&mut t, false, 700);
        assert_eq!(t.seq, before);
        assert!(t.auto, "kept while the tab is away");
        assert_eq!(fake.frames_open.load(SeqCst), 0);
        // A new connection starts with it off.
        t.disconnect();
        assert!(!t.auto);
        let _ = std::fs::remove_dir_all(dir);
    }

    const WIDE: f32 = 16.0 / 9.0;
    const LINE: f32 = 20.0;

    /// The real lines, as before the camera says which controls it has.
    fn real() -> Vec<Line> {
        Tuning::new(crate::gui::Own::default()).lines().iter().map(|&(g, c)| Line { group: g as usize, heading: c.is_none() }).collect()
    }

    /// The preview at the top left in its aspect. Every line placed once, in
    /// order, in a box, over nothing else and within the width. A heading has
    /// its first row under it in its column, and one repeated for a group that
    /// runs on is at the top of a column.
    fn sound(room: egui::Vec2, aspect: f32, lines: &[Line], a: &Arrangement) {
        let p = a.preview;
        assert!(p.min == egui::Pos2::ZERO && (p.width() / p.height() - aspect).abs() < 1e-3, "{p:?}");
        let order: Vec<usize> = a.lines.iter().filter_map(|l| l.line).collect();
        assert_eq!(order, (0..lines.len()).collect::<Vec<_>>());
        // They read in order: a line left of the one before it is below it.
        for w in a.lines.windows(2) {
            let (r, n) = (w[0].rect, w[1].rect);
            assert!(n.left() >= r.left() || n.top() >= r.bottom(), "{n:?} after {r:?} in {room:?}");
        }
        for (k, l) in a.lines.iter().enumerate() {
            let r = l.rect;
            assert!(r.left() >= 0.0 && r.right() <= room.x + 1e-3, "{r:?} outside {room:?}");
            assert!(a.boxes.iter().any(|b| b.contains_rect(r)), "{r:?} outside the boxes in {room:?}");
            assert!(!r.intersects(p), "{r:?} over the preview in {room:?}");
            for q in &a.lines[k + 1..] {
                assert!(!r.shrink(0.5).intersects(q.rect.shrink(0.5)), "{r:?} overlaps {:?} in {room:?}", q.rect);
            }
            if l.line.is_none_or(|i| lines[i].heading) {
                let n = a.lines.get(k + 1).expect("a heading at the end");
                assert!(n.line.is_some_and(|i| !lines[i].heading), "{l:?} then {n:?}");
                assert!(n.rect.left() == r.left() && n.rect.top() > r.top() && n.group == l.group, "{l:?} then {n:?} in {room:?}");
            }
            if l.line.is_none() {
                assert!(!a.lines.iter().any(|q| q.rect.left() == r.left() && q.rect.top() < r.top()), "{l:?} not at the top");
            }
        }
    }

    #[test]
    fn a_group_that_runs_on_repeats_its_heading() {
        // Two groups of ten rows, through columns of five lines.
        let group = |g| std::iter::once(Line { group: g, heading: true }).chain(std::iter::repeat_n(Line { group: g, heading: false }, 10));
        let lines: Vec<Line> = group(0).chain(group(1)).collect();
        let col = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(300.0, 5.0 * (LINE + ROW_GAP)));
        let cols: Vec<egui::Rect> = (0..6).map(|k| col.translate(egui::vec2(k as f32 * 320.0, 0.0))).collect();
        let placed = flow(&lines, &cols, LINE, false).unwrap();
        let top = |c: usize| placed.iter().find(|(at, _)| *at == c).map(|(_, p)| (p.group, p.line)).unwrap();
        // The second group's heading doesn't stay at the foot of the third column.
        let tops: Vec<_> = (0..6).map(top).collect();
        assert_eq!(tops, [(0, Some(0)), (0, None), (0, None), (1, Some(11)), (1, None), (1, None)]);
        assert!(flow(&lines, &cols[..5], LINE, false).is_none());
        let run_on = flow(&lines, &cols[..5], LINE, true).unwrap();
        assert!(run_on.iter().any(|(_, p)| p.rect.bottom() > col.bottom()));
    }

    #[test]
    fn a_wide_room_puts_the_controls_beside_a_preview_of_full_height() {
        let (room, lines) = (egui::vec2(2200.0, 900.0), real());
        let a = arrange(room, WIDE, &lines, LINE);
        sound(room, WIDE, &lines, &a);
        assert_eq!(a.preview.height(), room.y);
        assert!(a.lines.iter().all(|l| l.rect.left() > a.preview.right()));
    }

    #[test]
    fn a_tall_room_puts_the_controls_below_a_preview_of_full_width() {
        let (room, lines) = (egui::vec2(1000.0, 1200.0), real());
        let a = arrange(room, WIDE, &lines, LINE);
        sound(room, WIDE, &lines, &a);
        assert_eq!(a.preview.width(), room.x);
        assert!(a.lines.iter().all(|l| l.rect.top() > a.preview.bottom()));
        assert!(a.height <= room.y);
    }

    #[test]
    fn the_controls_wrap_around_the_preview_and_fit() {
        // The tab in the default 1280x800 window, and in 1100x720 and 1145x842 ones.
        for room in [egui::vec2(1264.0, 682.0), egui::vec2(1084.0, 648.0), egui::vec2(1129.0, 760.0)] {
            let lines = real();
            let a = arrange(room, WIDE, &lines, LINE);
            sound(room, WIDE, &lines, &a);
            assert!(a.height <= room.y, "{room:?} scrolls");
            assert!(a.lines.iter().any(|l| l.rect.left() > a.preview.right()));
            assert!(a.lines.iter().any(|l| l.rect.top() > a.preview.bottom()));
            assert!(a.preview.width() >= 700.0, "{room:?}: {:?}", a.preview);
        }
    }

    #[test]
    fn a_small_room_scrolls_once_the_preview_is_as_small_as_it_gets() {
        let (room, lines) = (egui::vec2(624.0, 400.0), real());
        let a = arrange(room, WIDE, &lines, LINE);
        sound(room, WIDE, &lines, &a);
        assert!(a.height > room.y);
        assert_eq!(a.preview.width(), SMALLEST);
    }

    #[test]
    fn every_room_gets_a_sound_arrangement() {
        let lines = real();
        for aspect in [WIDE, 4.0 / 3.0] {
            for w in (320..2600).step_by(149) {
                for h in (160..1500).step_by(113) {
                    let room = egui::vec2(w as f32, h as f32);
                    let a = arrange(room, aspect, &lines, LINE);
                    sound(room, aspect, &lines, &a);
                    // It scrolls only once the preview is as small as it gets.
                    assert!(a.height <= room.y + 1e-3 || a.preview.width() == SMALLEST.min((room.y * aspect).floor()), "{room:?}");
                }
            }
        }
        let a = arrange(egui::vec2(800.0, 600.0), WIDE, &[], LINE);
        assert_eq!((a.preview.width(), a.lines.len(), a.boxes.len()), (800.0, 0, 0));
    }

    #[test]
    fn controls_are_uvcdctls() {
        // Numbers and names are what uvcdctl takes: never two the same.
        for (i, a) in DEFS.iter().enumerate() {
            for b in &DEFS[i + 1..] {
                assert!(a.id != b.id && a.name != b.name && a.kind != b.kind, "{} and {}", a.name, b.name);
            }
        }
        // Groups are listed together, in the order they are shown.
        let order: Vec<usize> = DEFS.iter().map(|d| Group::ALL.iter().position(|g| *g == d.group).unwrap()).collect();
        assert!(order.windows(2).all(|w| w[0] <= w[1]));
    }
}
