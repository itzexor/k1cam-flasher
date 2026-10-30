// SPDX-License-Identifier: MIT
//! The GUI: cloner backup and flash, A/B update, tuning with a live preview,
//! the camera's console, and help on forcing USB boot mode; for a camera
//! attached here, or to another computer running `k1cam-flasher daemon`.

use crate::job::{Event, Job};
use crate::remote::{self, Link, Remote};
use crate::{api, bundle, cloner, console, ops, tuning, xu};
use eframe::egui::{self, Color32, RichText};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Cloner,
    Update,
    Tuning,
    Console,
    Help,
}

/// What a job does, so that its outcome shows where it was started.
#[derive(PartialEq, Eq, Hash, Clone, Copy)]
enum Op {
    Detect,
    Backup,
    Flash,
    Dump,
    Update,
    Reboot,
    Cancel,
    /// Saving a backup another computer kept.
    Fetch,
}

impl Op {
    /// A job found running on the host, by the name the daemon gives it.
    fn from_name(name: &str) -> Op {
        match name {
            "detect" => Op::Detect,
            "backup" => Op::Backup,
            "flash" => Op::Flash,
            "dump" => Op::Dump,
            "update" => Op::Update,
            "reboot" => Op::Reboot,
            _ => Op::Cancel,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Op::Fetch => "Save a backup from the host",
            Op::Detect => "Detect flash chip",
            Op::Backup => "Back up with the cloner",
            Op::Flash => "Flash with the cloner",
            Op::Dump => "Dump flash",
            Op::Update => "A/B update",
            Op::Reboot => "Reboot into trial",
            Op::Cancel => "Cancel trial",
        }
    }

    /// Installing, rebooting into a trial and cancelling it: the A/B update,
    /// which keeps its tab open while it runs.
    fn ab(self) -> bool {
        matches!(self, Op::Update | Op::Reboot | Op::Cancel)
    }

    /// What quitting halfway leaves behind.
    fn risk(self) -> &'static str {
        match self {
            Op::Flash => "Stopping a flash halfway leaves the camera unable to boot until it is flashed again with the cloner.",
            Op::Update => "The camera keeps booting what it runs now; an install it has already started still finishes.",
            Op::Backup | Op::Dump => "Stopping now leaves no backup.",
            Op::Fetch => "The host keeps the backup until it is saved.",
            _ => "It stops cleanly.",
        }
    }
}

/// The job that runs.
struct Running {
    op: Op,
    rx: Receiver<Event>,
    started: Instant,
    progress: f32,
    /// When the current step began: a flash backs up, writes and verifies,
    /// each from 0 to 100%.
    step: Instant,
}

#[derive(Clone, Copy, PartialEq)]
enum Level {
    Info,
    Start,
    Ok,
    Failed,
}

struct Line {
    level: Level,
    text: String,
    /// The time of day for the reader, and the instant for durations.
    time: String,
    at: Instant,
    /// A Start line's operation shows its lines; only the newest does by default.
    open: bool,
}

struct App {
    tab: Tab,
    /// The tab shown until now, to notice it being left.
    shown_tab: Tab,
    /// Until a tab is chosen, the first look at what is attached picks one.
    tab_chosen: bool,
    log: Vec<Line>,
    job: Option<Running>,
    cancel: Arc<AtomicBool>,
    /// Each operation's last outcome, shown where it was started.
    results: HashMap<Op, Result<String, String>>,
    cloner_up: bool,
    camera_up: bool,
    last_poll: Instant,
    image: Option<Image>,
    backup_first: bool,
    verify: bool,
    reboot_after: bool,
    confirm: bool,
    bundle: Option<Loaded>,
    reboot_trial: bool,
    live_dump: bool,
    cam_info: Option<Result<String, String>>,
    info_rx: Option<Receiver<Result<String, String>>>,
    info_thread: Option<JoinHandle<()>>,
    console: console::Console,
    tuning: tuning::Tuning,
    /// The window was asked to close while a job ran.
    quit_prompt: bool,
    /// Close as soon as the job has stopped.
    quit_after: bool,
    /// The log beside Backup & Flash and A/B Update is folded to a strip, and
    /// how many of its lines have been seen.
    log_collapsed: bool,
    log_seen: usize,
    own: Own,
    /// The other computer whose camera this is, in remote mode.
    remote: Option<Arc<Remote>>,
    conn: ConnMenu,
    /// Backups the host keeps that this computer hasn't saved.
    held: Vec<api::Job>,
    held_rx: Option<Receiver<Result<Vec<api::Job>, String>>>,
    /// Quit while a remote job runs, leaving it running there.
    quit_anyway: bool,
}

/// This computer's flasher lock (lock.rs), held while the GUI uses the camera
/// here: a job, an info read, tuning or the console; let go once none does.
/// In remote mode the host's daemon holds its own.
#[derive(Clone, Default)]
pub struct Own(Arc<Mutex<OwnState>>);

#[derive(Default)]
struct OwnState {
    enabled: bool,
    #[cfg(target_os = "linux")]
    lock: Option<crate::lock::Lock>,
}

impl Own {
    /// `Own::default()` takes nothing (for tests).
    fn new(enabled: bool) -> Self {
        Self(Arc::new(Mutex::new(OwnState { enabled, ..Default::default() })))
    }

    pub fn hold(&self) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        {
            let mut s = self.0.lock().unwrap();
            if s.enabled && s.lock.is_none() {
                s.lock = Some(crate::lock::acquire(format!("k1cam-flasher GUI (pid {})", std::process::id()))?);
            }
        }
        Ok(())
    }

    fn release(&self) {
        #[cfg(target_os = "linux")]
        {
            self.0.lock().unwrap().lock = None;
        }
    }

    fn set_enabled(&self, enabled: bool) {
        self.0.lock().unwrap().enabled = enabled;
        if !enabled {
            self.release();
        }
    }
}

/// The connection menu: where the camera is plugged in.
struct ConnMenu {
    /// "Another computer" is chosen, so its fields show.
    elsewhere: bool,
    host: String,
    password: String,
    remember: bool,
    connecting: Option<Receiver<Result<Arc<Remote>, String>>>,
    /// A refused or taken connection taking its session again.
    reconnecting: Option<Receiver<Result<(), String>>>,
    error: Option<String>,
    /// Open the menu (a connection failed, or needs a new code, without it open).
    open: bool,
    /// Close the menu (a connection it started succeeded).
    close: bool,
    /// The connection is refused or taken: the menu opened for it once.
    stuck: bool,
    /// Put the cursor in the code field.
    focus_code: bool,
}

/// A chosen .upd, read and checked when chosen and again whenever the file changes.
struct Loaded {
    path: PathBuf,
    modified: Option<SystemTime>,
    bundle: Result<Arc<bundle::Bundle>, String>,
}

impl Loaded {
    fn load(path: PathBuf) -> Self {
        let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let bundle = bundle::load(&path).map(Arc::new);
        Self { path, modified, bundle }
    }

    fn changed(&self) -> bool {
        std::fs::metadata(&self.path).and_then(|m| m.modified()).ok() != self.modified
    }
}

/// A chosen flash image, checked for size when chosen (the flash checks again).
struct Image {
    path: PathBuf,
    check: Result<(), String>,
}

impl Image {
    fn load(path: PathBuf) -> Self {
        let check = match std::fs::metadata(&path) {
            Ok(m) if m.len() == ops::FLASH_SIZE as u64 => Ok(()),
            Ok(_) if path.extension().is_some_and(|e| e == "upd") => Err("This is an update bundle: it goes on the A/B Update tab".into()),
            Ok(m) => Err(format!("{}; a whole-flash image is exactly 8 MiB", bytes(m.len()))),
            Err(e) => Err(e.to_string()),
        };
        Self { path, check }
    }
}

/// Status colours that read on the light theme as well as on the dark one.
pub fn ok_color(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgb(110, 200, 120)
    } else {
        Color32::from_rgb(20, 130, 50)
    }
}

pub fn warn_color(ui: &egui::Ui) -> Color32 {
    ui.visuals().warn_fg_color
}

pub fn err_color(ui: &egui::Ui) -> Color32 {
    ui.visuals().error_fg_color
}

pub fn weak_color(ui: &egui::Ui) -> Color32 {
    ui.visuals().weak_text_color()
}

/// A dot in a colour, then the text.
pub fn status(ui: &mut egui::Ui, color: Color32, text: impl Into<egui::WidgetText>) -> egui::Response {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, ui.text_style_height(&egui::TextStyle::Body)), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.0, color);
        ui.add(egui::Label::new(text).wrap());
    })
    .response
}

/// A dot, then the text cut to one line (all of it on hover), for a line
/// that is always there so that what comes and goes in it moves nothing else.
fn line(ui: &mut egui::Ui, color: Color32, text: &str) -> egui::Response {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, ui.text_style_height(&egui::TextStyle::Body)), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
    ui.add(egui::Label::new(RichText::new(text).color(color)).truncate())
}

/// The time of day, local where the platform tells what local is.
fn wall_clock(t: SystemTime) -> String {
    let secs = t.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    #[cfg(unix)]
    {
        let t = secs as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        if !unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
            return format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec);
        }
    }
    let s = secs % 86400;
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// A chevron pointing along `dir`, painted: the fonts have no arrows.
fn chevron(painter: &egui::Painter, center: egui::Pos2, dir: egui::Vec2, color: Color32) {
    let (s, side) = (3.5, egui::vec2(-dir.y, dir.x));
    let back = center - dir * s * 0.5;
    painter.line(vec![back + side * s, center + dir * s * 0.5, back - side * s], egui::Stroke::new(1.5, color));
}

/// The log's unread mark: how many operations finished, red if one failed,
/// or a dot for lines alone.
fn badge(ui: &egui::Ui, center: egui::Pos2, (done, failed): (usize, bool)) {
    let painter = ui.painter();
    if done == 0 {
        painter.circle_filled(center, 3.5, ui.visuals().text_color());
        return;
    }
    let fill = if failed { Color32::from_rgb(210, 60, 60) } else { ui.visuals().widgets.inactive.bg_fill };
    let text = if failed { Color32::WHITE } else { ui.visuals().strong_text_color() };
    let galley = painter.layout_no_wrap(done.to_string(), egui::FontId::proportional(11.0), text);
    let rect = egui::Rect::from_center_size(center, egui::vec2((galley.size().x + 10.0).max(18.0), 17.0));
    painter.rect_filled(rect, 8.5, fill);
    painter.galley(rect.center() - galley.size() / 2.0, galley, text);
}

/// Sizes as the firmware's documents give them: whole MiB, else KiB.
pub fn bytes(n: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    match n {
        n if n >= MIB && n % MIB == 0 => format!("{} MiB", n / MIB),
        n if n >= 16 * MIB => format!("{:.1} MiB", n as f64 / MIB as f64),
        n if n >= 1024 => format!("{} KiB", n.div_ceil(1024)),
        n => format!("{n} bytes"),
    }
}

fn backup_name() -> String {
    format!("k1cam-backup-{}.bin", ops::stamp())
}

fn clock(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

fn camera_info() -> Result<String, String> {
    xu::Camera::open().and_then(|mut c| c.info())
}

/// A titled box for one task on a tab.
fn card<R>(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let r = egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::same(12))
        .corner_radius(6.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).strong().size(15.0));
            ui.add_space(2.0);
            add(ui)
        })
        .inner;
    ui.add_space(8.0);
    r
}

/// A chosen file: its name, its folder dimmed, the whole path on hover.
fn file_row(ui: &mut egui::Ui, path: Option<&Path>, none: &str) {
    match path {
        Some(p) => {
            let name = p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned());
            let dir = p.parent().map(|d| d.display().to_string()).unwrap_or_default();
            ui.label(RichText::new(name).strong()).on_hover_text(p.display().to_string());
            ui.add(egui::Label::new(RichText::new(dir).color(weak_color(ui))).truncate()).on_hover_text(p.display().to_string());
        }
        None => {
            ui.label(RichText::new(none).color(weak_color(ui)));
        }
    }
}

impl App {
    fn new() -> Self {
        let own = Own::new(!cfg!(test));
        let saved = if cfg!(test) { None } else { remote::saved::load() };
        Self {
            tab: Tab::Update,
            shown_tab: Tab::Update,
            tab_chosen: false,
            log: Vec::new(),
            job: None,
            cancel: Arc::new(AtomicBool::new(false)),
            results: HashMap::new(),
            cloner_up: false,
            camera_up: false,
            last_poll: Instant::now() - Duration::from_secs(10),
            image: None,
            backup_first: true,
            verify: true,
            reboot_after: true,
            confirm: false,
            bundle: None,
            reboot_trial: true,
            live_dump: false,
            cam_info: None,
            info_rx: None,
            info_thread: None,
            console: console::Console::new(own.clone()),
            tuning: tuning::Tuning::new(own.clone()),
            quit_prompt: false,
            quit_after: false,
            log_collapsed: false,
            log_seen: 0,
            own,
            remote: None,
            conn: ConnMenu {
                elsewhere: false,
                remember: true,
                host: saved.unwrap_or_default(),
                password: String::new(),
                connecting: None,
                reconnecting: None,
                error: None,
                open: false,
                close: false,
                stuck: false,
                focus_code: false,
            },
            held: Vec::new(),
            held_rx: None,
            quit_anyway: false,
        }
    }

    /// Starts `op` here, or on the host in remote mode.
    fn start_op<L, M>(&mut self, op: Op, here: L, there: M)
    where
        L: FnOnce(&Job) -> Result<String, String> + Send + 'static,
        M: FnOnce(&Remote, &Job) -> Result<String, String> + Send + 'static,
    {
        match self.remote.clone() {
            Some(r) => self.start(op, move |j| there(&r, j)),
            None => self.start(op, here),
        }
    }

    /// Reads the camera's firmware record, here or there.
    fn info_reader(&self) -> Box<dyn FnOnce() -> Result<String, String> + Send> {
        match self.remote.clone() {
            Some(r) => Box::new(move || r.info()),
            None => Box::new(camera_info),
        }
    }

    /// A/B over the extension unit isn't possible on Windows itself.
    fn ab_unavailable(&self) -> bool {
        cfg!(windows) && self.remote.is_none()
    }

    fn connect_remote(&mut self) {
        if self.conn.connecting.is_some() || self.busy() {
            return;
        }
        let (host, password) = (self.conn.host.trim().to_string(), self.conn.password.trim().to_string());
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(Remote::connect(&host, &password));
        });
        self.conn.error = None;
        self.conn.connecting = Some(rx);
    }

    /// Switches to the camera on `r`, or (None) to this computer's; whatever
    /// was open on the other side closes.
    fn use_remote(&mut self, r: Option<Arc<Remote>>) {
        if let Some(old) = self.remote.take() {
            std::thread::spawn(move || old.disconnect());
        }
        (self.conn.reconnecting, self.conn.stuck, self.conn.error) = (None, false, None);
        self.tuning.set_remote(r.clone());
        self.console.set_remote(r.clone());
        self.own.set_enabled(r.is_none() && !cfg!(test));
        (self.cam_info, self.info_rx, self.cloner_up, self.camera_up) = (None, None, false, false);
        self.last_poll = Instant::now() - Duration::from_secs(10);
        self.held.clear();
        match &r {
            Some(r) => {
                let h = &r.hello;
                let name = if r.name.is_empty() || r.name == r.host { String::new() } else { format!(" ({})", r.name) };
                self.note(Level::Info, format!("Connected to {}{name}: k1cam-flasher {}, {} {}", r.addr, h.version, h.os, h.arch));
                if self.conn.remember {
                    if let Err(e) = remote::saved::store(&self.conn.host) {
                        self.note(Level::Info, format!("The host was not remembered: {e}"));
                    }
                } else {
                    remote::saved::forget();
                }
                self.remote = Some(r.clone());
                self.refresh_held();
            }
            None => self.note(Level::Info, "Using the USB devices of this computer"),
        }
    }

    /// Asks the host which jobs it keeps: ones still running are followed,
    /// finished ones reported, and backups not yet saved listed.
    fn refresh_held(&mut self) {
        let Some(r) = self.remote.clone() else { return };
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(r.jobs());
        });
        self.held_rx = Some(rx);
    }

    fn poll_remote(&mut self) {
        if let Some(rx) = &self.conn.connecting {
            match rx.try_recv() {
                Ok(Ok(r)) => {
                    self.conn.connecting = None;
                    self.conn.close = true;
                    self.use_remote(Some(r));
                }
                Ok(Err(e)) => {
                    self.conn.connecting = None;
                    self.conn.error = Some(e);
                    self.conn.open = true;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.conn.connecting = None,
            }
        }
        if let Some(rx) = &self.conn.reconnecting {
            match rx.try_recv() {
                Ok(Ok(())) => {
                    self.conn.reconnecting = None;
                    self.conn.close = true;
                    if let Some(r) = &self.remote {
                        self.note(Level::Info, format!("Connected to {} again", r.addr));
                    }
                    // A daemon that restarted may hold other jobs now.
                    self.refresh_held();
                }
                Ok(Err(e)) => {
                    self.conn.reconnecting = None;
                    self.conn.error = Some(e);
                    self.conn.focus_code = true;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.conn.reconnecting = None,
            }
        }
        // A connection that needs the user opens the menu, once: for a new
        // code (the field emptied, the one it had refused), or to take the
        // camera back.
        let link = self.remote.as_ref().map(|r| r.link());
        let stuck = matches!(link, Some(Link::Refused(_) | Link::Taken(_)));
        if stuck != std::mem::replace(&mut self.conn.stuck, stuck) && stuck {
            (self.conn.elsewhere, self.conn.open, self.conn.error) = (true, true, None);
            if matches!(link, Some(Link::Refused(_))) {
                self.conn.password.clear();
                self.conn.focus_code = true;
            }
        }
        let Some(rx) = &self.held_rx else { return };
        let jobs = match rx.try_recv() {
            Ok(Ok(jobs)) => jobs,
            Ok(Err(_)) | Err(TryRecvError::Disconnected) => {
                self.held_rx = None;
                return;
            }
            Err(TryRecvError::Empty) => return,
        };
        self.held_rx = None;
        let Some(r) = self.remote.clone() else { return };
        for j in &jobs {
            match (&j.done, &j.output) {
                // Started by an earlier connection: followed from here on.
                (None, _) if !self.busy() => {
                    let (r, id) = (r.clone(), j.id.clone());
                    self.start(Op::from_name(&j.op), move |job| {
                        job.log(format!("It was started earlier, on {}; following it", r.host));
                        r.follow(job, &id)?.done.unwrap_or_else(|| Err("no outcome".into()))
                    });
                }
                (Some(outcome), None) => {
                    let said = match outcome {
                        Ok(m) => format!("ended: {m}"),
                        Err(e) => format!("failed: {e}"),
                    };
                    self.note(Level::Info, format!("On {}, {} {said}", r.host, Op::from_name(&j.op).title().to_lowercase()));
                    let (r, id) = (r.clone(), j.id.clone());
                    std::thread::spawn(move || r.dismiss(&id));
                }
                _ => {}
            }
        }
        self.held = jobs.into_iter().filter(|j| j.done.is_some() && j.output.is_some()).collect();
    }

    fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// An A/B update runs: the other tabs wait (Help aside).
    fn ab_running(&self) -> bool {
        self.job.as_ref().is_some_and(|j| j.op.ab())
    }

    /// An A/B update keeps its tab open until it ends (one found running on
    /// a host opens it), and leaving Tuning or Console lets the camera go.
    fn interlock_tabs(&mut self) {
        if self.ab_running() && !matches!(self.tab, Tab::Update | Tab::Help) {
            self.tab = Tab::Update;
        }
        if self.tab != self.shown_tab {
            match self.shown_tab {
                Tab::Tuning => self.tuning.leave(),
                Tab::Console => self.console.disconnect(),
                _ => {}
            }
            self.shown_tab = self.tab;
        }
    }

    fn note(&mut self, level: Level, text: impl Into<String>) {
        // A new operation folds the ones before it.
        if level == Level::Start {
            self.log.iter_mut().for_each(|l| l.open = false);
        }
        let (time, at) = (wall_clock(SystemTime::now()), Instant::now());
        self.log.push(Line { level, text: text.into(), time, at, open: level == Level::Start });
    }

    fn log_text(&self) -> String {
        self.log
            .iter()
            .map(|l| match l.level {
                Level::Start => format!("{} --- {} ---\n", l.time, l.text),
                Level::Ok => format!("{} OK: {}\n", l.time, l.text),
                Level::Failed => format!("{} FAILED: {}\n", l.time, l.text),
                Level::Info => format!("{} {}\n", l.time, l.text),
            })
            .collect()
    }

    /// What reached the log since it last showed: operations finished, and
    /// whether one failed.
    fn unread(&self) -> Option<(usize, bool)> {
        let new = &self.log[self.log_seen.min(self.log.len())..];
        let done = new.iter().filter(|l| matches!(l.level, Level::Ok | Level::Failed)).count();
        (!new.is_empty()).then(|| (done, new.iter().any(|l| l.level == Level::Failed)))
    }

    fn start<F>(&mut self, op: Op, f: F)
    where
        F: FnOnce(&Job) -> Result<String, String> + Send + 'static,
    {
        if self.busy() {
            return;
        }
        // Here, this computer's flashers take turns with the camera.
        if let Err(e) = self.own.hold() {
            self.note(Level::Start, op.title());
            self.note(Level::Failed, e.clone());
            self.results.insert(op, Err(e));
            return;
        }
        let mut threads = self.tuning.stop();
        threads.extend(self.console.stop());
        threads.extend(self.info_thread.take());
        self.info_rx = None;
        self.cam_info = None;
        let (tx, rx) = channel();
        self.cancel.store(false, Ordering::Relaxed);
        self.results.remove(&op);
        self.note(Level::Start, op.title());
        // What a job says shows in the log.
        self.log_collapsed = false;
        let now = Instant::now();
        self.job = Some(Running { op, rx, started: now, progress: 0.0, step: now });
        let job = Job::new(tx.clone(), self.cancel.clone());
        std::thread::spawn(move || {
            let r = (|| {
                if !threads.is_empty() {
                    job.log("Waiting for live camera access to stop");
                }
                let mut failed = false;
                for thread in threads {
                    failed |= thread.join().is_err();
                }
                if failed {
                    return Err("a camera thread failed while disconnecting".into());
                }
                job.check_cancel()?;
                f(&job)
            })();
            let _ = tx.send(Event::Done(r));
        });
    }

    fn drain(&mut self) {
        let Some(job) = &mut self.job else { return };
        let mut lines = Vec::new();
        let mut done = None;
        while let Ok(ev) = job.rx.try_recv() {
            match ev {
                Event::Log(s) => lines.push(s),
                Event::Progress(p) => {
                    if p + 0.05 < job.progress {
                        job.step = Instant::now();
                    }
                    job.progress = p;
                }
                Event::Done(r) => done = Some(r),
            }
        }
        let op = job.op;
        for l in lines {
            self.note(Level::Info, l);
        }
        let Some(r) = done else { return };
        // The operation's heading in the log says how long it took.
        match &r {
            Ok(m) => self.note(Level::Ok, m.clone()),
            Err(e) => self.note(Level::Failed, e.clone()),
        }
        // Installing, rebooting into a trial and cancelling it share one line.
        if op.ab() {
            self.results.retain(|o, _| !o.ab());
        }
        self.results.insert(op, r);
        self.job = None;
        // A job may have changed what the camera reports.
        self.cam_info = None;
        // And a remote one may have left a backup there.
        self.refresh_held();
    }

    /// A valid bundle may read the camera for comparison; other reads need Connect.
    fn poll_camera_info(&mut self, read: impl FnOnce() -> Result<String, String> + Send + 'static) {
        if let Some(rx) = &self.info_rx {
            match rx.try_recv() {
                Ok(r) => {
                    self.info_rx = None;
                    if self.camera_up {
                        self.cam_info = Some(r);
                    }
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => self.info_rx = None,
            }
        }
        let compare = self.bundle.as_ref().is_some_and(|l| l.bundle.is_ok());
        if compare && self.cam_info.is_none() {
            self.read_camera_info(read);
        }
    }

    fn reading_info(&self) -> bool {
        self.info_rx.is_some() || self.info_thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    fn read_camera_info(&mut self, read: impl FnOnce() -> Result<String, String> + Send + 'static) {
        if !self.camera_up || self.busy() || self.reading_info() {
            return;
        }
        if let Err(e) = self.own.hold() {
            self.cam_info = Some(Err(e));
            return;
        }
        let (tx, rx) = channel();
        self.cam_info = None;
        self.info_rx = Some(rx);
        self.info_thread = Some(std::thread::spawn(move || {
            let _ = tx.send(read());
        }));
    }

    fn load_bundle(&mut self, path: PathBuf) {
        self.bundle = Some(Loaded::load(path));
        self.cam_info = None;
    }

    /// Beside the button that starts them: one of `ops` running, or its last
    /// outcome, else `idle` dimmed.
    fn result_line(&self, ui: &mut egui::Ui, ops: &[Op], idle: &str) {
        if self.job.as_ref().is_some_and(|j| ops.contains(&j.op)) {
            ui.spinner();
            ui.label(RichText::new("Running…").color(weak_color(ui)));
            return;
        }
        match ops.iter().find_map(|o| self.results.get(o).map(|r| (o, r))) {
            Some((_, Ok(m))) => {
                line(ui, ok_color(ui), m);
            }
            Some((op, Err(e))) => {
                line(ui, err_color(ui), &format!("{} failed: {e}", op.title()));
            }
            None if !idle.is_empty() => {
                ui.label(RichText::new(idle).color(weak_color(ui)));
            }
            None => {}
        }
    }

    fn choose_image(&mut self) {
        if let Some(p) = rfd::FileDialog::new().add_filter("flash image", &["bin"]).pick_file() {
            self.image = Some(Image::load(p));
            self.confirm = false;
        }
    }

    fn choose_bundle(&mut self) {
        if let Some(p) = rfd::FileDialog::new().add_filter("k1cam update", &["upd"]).pick_file() {
            self.load_bundle(p);
        }
    }

    /// Every line on this tab is always there, so nothing moves as results come in.
    fn cloner_tab(&mut self, ui: &mut egui::Ui) {
        let idle = !self.busy() && self.cloner_up;
        card(ui, "Flash chip", |ui| {
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(idle, egui::Button::new("Detect flash chip"))
                    .on_hover_text("Checks the link and identifies the flash, without changing it")
                    .clicked()
                {
                    let here = |j: &Job| {
                        let c = cloner::Cloner::probe(j)?;
                        Ok(format!("{}, {} (JEDEC {:06x})", c.name, bytes(c.size as u64), c.jedec))
                    };
                    self.start_op(Op::Detect, here, |r, j| r.detect(j));
                }
                if !self.cloner_up && !self.busy() {
                    line(ui, weak_color(ui), "Waiting for a camera in USB boot mode.")
                        .on_hover_text("The camera answers here when it powers up with two pins of its flash chip shorted.");
                    if ui.link("How to connect the camera for flashing").clicked() {
                        self.tab = Tab::Help;
                    }
                } else {
                    self.result_line(ui, &[Op::Detect], "Not detected yet");
                }
            });
        });
        card(ui, "Backup", |ui| {
            ui.label(RichText::new("Reads the whole flash into a file and shows its SHA-256.").color(weak_color(ui)));
            ui.horizontal(|ui| {
                if ui.add_enabled(idle, egui::Button::new("Back up flash…")).clicked() {
                    if let Some(path) = rfd::FileDialog::new().set_file_name(backup_name()).save_file() {
                        let p = path.clone();
                        self.start_op(Op::Backup, move |j| ops::backup(j, &p), move |r, j| r.backup(j, &path));
                    }
                }
                self.result_line(ui, &[Op::Backup], "No backup yet");
            });
        });
        card(ui, "Flash", |ui| {
            ui.label(
                RichText::new(
                    "Replaces the whole flash: U-Boot, both slots and the settings. This installs the A/B layout, once; \
                     after that, update with k1cam.upd on the A/B Update tab.",
                )
                .color(weak_color(ui)),
            );
            ui.horizontal(|ui| {
                if ui.button("Choose image…").clicked() {
                    self.choose_image();
                }
                file_row(ui, self.image.as_ref().map(|i| i.path.as_path()), "no image chosen (k1cam.bin)");
            });
            ui.horizontal(|ui| match self.image.as_ref().map(|i| &i.check) {
                Some(Ok(())) => line(ui, ok_color(ui), "8 MiB, a whole-flash image"),
                Some(Err(e)) => line(ui, err_color(ui), e),
                None => line(ui, weak_color(ui), "An 8 MiB whole-flash image"),
            });
            ui.checkbox(&mut self.backup_first, "Back up the current flash first (next to the image)");
            ui.checkbox(&mut self.verify, "Read back and verify after writing");
            ui.checkbox(&mut self.reboot_after, "Reboot the camera when done");
            ui.checkbox(&mut self.confirm, "I understand this erases the whole flash, U-Boot and the A/B layout included");
            let image = self.image.as_ref().filter(|i| i.check.is_ok()).map(|i| i.path.clone());
            let can = idle && image.is_some() && self.confirm;
            let mut button = egui::Button::new(RichText::new("Flash").strong());
            if can {
                button = button.fill(err_color(ui).gamma_multiply(0.55));
            }
            ui.horizontal(|ui| {
                if ui.add_enabled(can, button).clicked() {
                    let image = image.unwrap();
                    let (b, v, r) = (self.backup_first, self.verify, self.reboot_after);
                    self.confirm = false;
                    let i = image.clone();
                    self.start_op(Op::Flash, move |j| ops::flash(j, &i, b, v, r), move |rem, j| rem.flash(j, &image, b, v, r));
                }
                self.result_line(ui, &[Op::Flash], "");
            });
        });
    }

    /// Every line on this tab is always there: a bundle fills in its column of
    /// the table, and the long lists open over the page.
    fn update_tab(&mut self, ui: &mut egui::Ui) {
        if self.ab_unavailable() {
            status(
                ui,
                warn_color(ui),
                "Not available on Windows itself; use the camera through a Linux computer (the connection menu, top right), or the cloner on the Backup & Flash tab.",
            );
            ui.add_space(8.0);
        }
        // Not while the info record is being read: one exchange at a time.
        let idle = !self.busy() && self.info_rx.is_none() && self.camera_up && !self.ab_unavailable();
        card(ui, "Firmware", |ui| self.firmware_card(ui, idle));
        card(ui, "Backup", |ui| {
            ui.label(
                RichText::new(match &self.remote {
                    Some(r) => format!(
                        "Saves the whole flash of the camera on {} (8 MiB; about three minutes) into a file here; the host keeps it until it is saved.",
                        r.host
                    ),
                    None => "Saves the whole flash of the running camera (8 MiB; it takes about three minutes).".into(),
                })
                .color(weak_color(ui)),
            );
            ui.horizontal(|ui| {
                if ui.add_enabled(idle, egui::Button::new("Dump flash…")).clicked() {
                    if let Some(path) = rfd::FileDialog::new().set_file_name(backup_name()).save_file() {
                        let (live, p) = (self.live_dump, path.clone());
                        self.start_op(Op::Dump, move |j| ops::dump(j, &p, live), move |r, j| r.dump(j, &path, live));
                    }
                }
                ui.checkbox(&mut self.live_dump, "Live").on_hover_text(
                    "For a camera without the RAM for a snapshot: reads the flash as it changes, so the copy may be inconsistent (diagnostic)",
                );
                self.result_line(ui, &[Op::Dump], "");
            });
        });
    }

    /// The camera next to the chosen bundle, and installing it.
    fn firmware_card(&mut self, ui: &mut egui::Ui, idle: bool) {
        let info = match &self.cam_info {
            Some(Ok(i)) => Some(i.clone()),
            _ => None,
        };
        let get = |k| info.as_deref().and_then(|i| xu::record_value(i, k)).unwrap_or("");
        egui::Sides::new().shrink_left().show(
            ui,
            |ui| {
                let label = if info.is_some() { "Refresh" } else { "Connect" };
                let can = self.camera_up && !self.busy() && !self.reading_info() && !self.ab_unavailable();
                if ui.add_enabled(can, egui::Button::new(label)).clicked() {
                    let read = self.info_reader();
                    self.read_camera_info(read);
                }
                if !self.camera_up {
                    line(ui, weak_color(ui), "No camera running k1cam firmware is attached");
                } else if self.reading_info() {
                    ui.spinner();
                    ui.label("Reading the camera's firmware information…");
                } else {
                    match &self.cam_info {
                        None => line(ui, weak_color(ui), "Connect, or choose a bundle, to read the camera's firmware information"),
                        Some(Err(e)) => line(ui, warn_color(ui), &format!("Its firmware information could not be read: {e}")),
                        Some(Ok(_)) => line(ui, ok_color(ui), "Connected"),
                    };
                }
            },
            |ui| {
                let r = ui.add_enabled(info.is_some(), egui::Button::new("All fields…"));
                let Some(info) = &info else { return };
                egui::Popup::from_toggle_button_response(&r).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
                    egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                        egui::Grid::new("camera-raw").num_columns(2).spacing([16.0, 2.0]).striped(true).show(ui, |ui| {
                            for line in info.lines() {
                                let (k, v) = line.split_once('=').unwrap_or((line, ""));
                                ui.label(RichText::new(k).color(weak_color(ui)));
                                ui.monospace(v);
                                ui.end_row();
                            }
                        });
                    });
                    if ui.small_button("Copy").clicked() {
                        ui.ctx().copy_text(info.clone());
                    }
                });
            },
        );
        let trial = get("trial_slot");
        let trial_runs = !trial.is_empty() && get("running_slot") == trial;
        // As tall as the slots, before they are known too.
        egui::Sides::new().height(26.0).shrink_left().show(
            ui,
            |ui| {
                ui.label(RichText::new("Slots").color(weak_color(ui)));
                let Some(info) = &info else {
                    ui.label(RichText::new("—").color(weak_color(ui)));
                    return;
                };
                slots(ui, info);
                let mut boot = match get("boot") {
                    "confirmed" => "boots from its confirmed slot".to_string(),
                    "trial" => format!("trial, boot {} of {}", get("bootcount"), get("bootlimit")),
                    "fallback" => "fallback: the confirmed slot's kernel did not load".into(),
                    "single" => "single-slot layout (flash k1cam.bin once for A/B)".into(),
                    b => b.to_string(),
                };
                if !trial.is_empty() && !trial_runs {
                    boot += &format!("; a trial of slot {} starts at the next reboot", trial.to_uppercase());
                }
                let r = ui.add(egui::Label::new(RichText::new(boot).color(weak_color(ui))).truncate());
                if trial_runs {
                    r.on_hover_text(format!(
                        "Slot {} runs as a trial: the camera keeps it once it has streamed video.",
                        trial.to_uppercase()
                    ));
                }
            },
            |ui| {
                if trial.is_empty() {
                    return;
                }
                if ui
                    .add_enabled(idle, egui::Button::new("Cancel trial"))
                    .on_hover_text("Withdraw the trial: the camera stays with its confirmed slot")
                    .clicked()
                {
                    self.start_op(Op::Cancel, ops::cancel_trial, |r, j| r.cancel_trial(j));
                }
                if !trial_runs && ui.add_enabled(idle, egui::Button::new("Reboot into trial")).clicked() {
                    self.start_op(Op::Reboot, ops::reboot_trial, |r, j| r.reboot_trial(j));
                }
            },
        );
        ui.horizontal(|ui| {
            if ui.button("Choose bundle…").clicked() {
                self.choose_bundle();
            }
            file_row(ui, self.bundle.as_ref().map(|l| l.path.as_path()), "no bundle chosen (k1cam.upd)");
        });
        // What is shown is what gets installed.
        let b = match self.bundle.as_ref().map(|l| &l.bundle) {
            Some(Ok(b)) => Some(b.clone()),
            _ => None,
        };
        compare(ui, info.as_deref(), b.as_deref());
        ui.horizontal(|ui| {
            let release = b.as_ref().and_then(|b| b.release.clone());
            let mut r = ui.add_enabled(release.is_some(), egui::Button::new("os-release…"));
            if b.is_some() && release.is_none() {
                r = r.on_disabled_hover_text("This file has no release information (packed by an older k1cam-mkupd).");
            }
            let Some(text) = release else { return };
            egui::Popup::from_toggle_button_response(&r).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
                egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| ui.monospace(text.trim_end()));
            });
        });
        let verdict = info.as_deref().zip(b.as_deref()).map(|(i, b)| xu::install_slot(i, b));
        ui.horizontal(|ui| {
            match (self.bundle.as_ref().map(|l| &l.bundle), &verdict) {
                (Some(Err(e)), _) => line(ui, err_color(ui), e),
                (_, Some(Ok(slot))) => line(
                    ui,
                    ok_color(ui),
                    &format!("The camera can install this into slot {}, then boots it as a trial.", slot.to_uppercase()),
                ),
                (_, Some(Err(e))) => line(ui, warn_color(ui), e),
                (None, _) => line(ui, weak_color(ui), "Choose a k1cam.upd to compare it with the camera"),
                (Some(Ok(_)), None) => line(ui, weak_color(ui), "Connect to compare it with the camera"),
            };
        });
        ui.checkbox(&mut self.reboot_trial, "Reboot into the new slot when it is staged");
        ui.horizontal(|ui| {
            let label = match &verdict {
                Some(Ok(slot)) => format!("Install to slot {}", slot.to_uppercase()),
                _ => "Install to the inactive slot".into(),
            };
            let can = idle && b.is_some() && !matches!(verdict, Some(Err(_)));
            let mut button = egui::Button::new(RichText::new(label).strong());
            if can {
                button = button.fill(ui.visuals().selection.bg_fill);
            }
            if ui.add_enabled(can, button).clicked() {
                let (b, reboot) = (b.clone().unwrap(), self.reboot_trial);
                let b2 = b.clone();
                self.start_op(Op::Update, move |j| ops::update(j, &b, reboot), move |r, j| r.update(j, &b2, reboot));
            }
            self.result_line(ui, &[Op::Update, Op::Reboot, Op::Cancel], "");
        });
    }

    fn help_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Getting the camera into USB boot mode");
        ui.label("The camera only boots from USB, where the cloner can reach it, when its SoC cannot boot from the flash. Shorting two of the flash chip's pins while it powers up does that.");
        ui.add_space(8.0);
        let steps = [
            "Unplug the camera's USB cable and any power.",
            "Connect the camera to this computer with the USB cable, but leave it unpowered if you can (or unplug it again just before the next step).",
            "With tweezers, hold the two pins marked in the picture together: the two right-most pins of the top-right row beside the XMC 25QH64DH flash chip.",
            "While holding the short, plug in USB so the camera powers up.",
            "After about a second, release the short. Backup & Flash shows the camera as connected (USB a108:c309).",
        ];
        let picture = egui::Image::new(egui::include_image!("../docs/short-nor-pins.jpg")).corner_radius(4.0);
        let text = |ui: &mut egui::Ui| {
            for (i, s) in steps.iter().enumerate() {
                ui.horizontal_top(|ui| {
                    ui.label(RichText::new(format!("{}.", i + 1)).strong());
                    ui.add(egui::Label::new(*s).wrap());
                });
                ui.add_space(2.0);
            }
            ui.add_space(4.0);
            ui.label(
                "If it does not show up, power-cycle and try again: the short only matters at power-up. Don't hold it during the transfer.",
            );
        };
        // Side by side where there is room for both.
        let width = ui.available_width();
        if width >= 760.0 {
            ui.horizontal_top(|ui| {
                ui.add(picture.max_width((width * 0.55).min(560.0)));
                ui.add_space(8.0);
                ui.vertical(text);
            });
        } else {
            ui.add(picture.max_width(width.min(560.0)));
            ui.add_space(6.0);
            text(ui);
        }
        ui.add_space(14.0);
        ui.heading("Drivers and permissions");
        ui.add_space(4.0);
        ui.label(RichText::new("Linux").strong());
        ui.label("Install the udev rule from the repository (udev/99-k1cam-flasher.rules):");
        let cmd = "sudo cp udev/99-k1cam-flasher.rules /etc/udev/rules.d/ && sudo udevadm control --reload && sudo udevadm trigger";
        ui.horizontal_top(|ui| {
            if ui.small_button("Copy").clicked() {
                ui.ctx().copy_text(cmd.into());
            }
            ui.add(egui::Label::new(RichText::new(cmd).monospace()).wrap());
        });
        ui.label("A/B Update and Tuning need the camera's /dev/video node (the video group); the console needs /dev/ttyACM* (dialout, or uucp on Arch).");
        ui.add_space(6.0);
        ui.label(RichText::new("Windows").strong());
        ui.label("Bind WinUSB to a108:c309 with Zadig while the camera is in USB boot mode.");
        ui.add_space(6.0);
        ui.label(RichText::new("macOS").strong());
        ui.label("Nothing to install.");
        ui.add_space(14.0);
        ui.heading("A camera on another computer");
        ui.add_space(4.0);
        ui.label(
            "The camera can be plugged into another computer running Linux, a Raspberry Pi or a router for example, and used from here over the network. \
             On that computer, run the daemon (the static command-line builds have it), as a user who can open the camera:",
        );
        let daemon = "k1cam-flasher daemon";
        ui.horizontal_top(|ui| {
            if ui.small_button("Copy").clicked() {
                ui.ctx().copy_text(daemon.into());
            }
            ui.add(egui::Label::new(RichText::new(daemon).monospace()).wrap());
        });
        ui.label(
            "It takes no options, and prints the addresses to connect to and a code (a new one each time it starts, and after 5 wrong attempts). \
             In the connection menu at the top right, choose Another computer, and enter one of the addresses and the code.",
        );
        ui.label(
            "Every tab then works on that camera, from any system, Windows included. Files stay here: an image or bundle goes there when \
             the job starts, and a backup comes back when it ends (the other computer keeps it until it is saved here). A job carries on \
             there if the connection drops. Tuning shows snapshots instead of live video, and one window controls the camera at a time.",
        );
        ui.label(
            RichText::new(format!(
                "The connection is plain HTTP on port {}, the code included: use it on a network you trust.",
                api::PORT
            ))
            .color(weak_color(ui)),
        );
        ui.add_space(14.0);
        ui.heading("About");
        ui.add_space(4.0);
        ui.label(format!("k1cam-flasher {}, GPL-2.0-only.", env!("CARGO_PKG_VERSION")));
        ui.hyperlink_to("github.com/itzexor/k1cam-flasher", "https://github.com/itzexor/k1cam-flasher");
    }

    /// The log beside the tabs that run jobs: each operation under a heading
    /// that folds, with when it started, how long it took and how it ended.
    fn log_panel(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Log").strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let fold = ui.add(egui::Button::new("").min_size(egui::vec2(20.0, 20.0)).frame_when_inactive(false));
                chevron(ui.painter(), fold.rect.center(), egui::vec2(1.0, 0.0), ui.visuals().text_color());
                if fold.on_hover_text("Fold the log away").clicked() {
                    self.log_collapsed = true;
                }
                let any = !self.log.is_empty();
                if ui.add_enabled(any && !self.busy(), egui::Button::new("Clear").small()).clicked() {
                    self.log.clear();
                }
                if ui.add_enabled(any, egui::Button::new("Save…").small()).clicked() {
                    if let Some(path) = rfd::FileDialog::new().set_file_name(format!("k1cam-log-{}.txt", ops::stamp())).save_file() {
                        if let Err(e) = std::fs::write(&path, self.log_text()) {
                            self.note(Level::Failed, format!("could not save the log to {}: {e}", path.display()));
                        }
                    }
                }
                if ui.add_enabled(any, egui::Button::new("Copy").small()).clicked() {
                    ui.ctx().copy_text(self.log_text());
                }
            });
        });
        ui.separator();
        let mut toggle = None;
        egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            if self.log.is_empty() {
                ui.label(RichText::new("Operations and their results appear here.").color(weak_color(ui)));
            }
            let mut i = 0;
            while i < self.log.len() {
                let start = &self.log[i];
                if start.level != Level::Start {
                    // Said outside any operation.
                    log_line(ui, start);
                    i += 1;
                    continue;
                }
                // An operation runs to its outcome, or to the end while it runs.
                let rest = &self.log[i + 1..];
                let end = rest.iter().position(|l| matches!(l.level, Level::Ok | Level::Failed | Level::Start));
                let end = match end.map(|e| (e, rest[e].level)) {
                    Some((e, Level::Start)) => i + e,
                    Some((e, _)) => i + 1 + e,
                    None => self.log.len() - 1,
                };
                let last = &self.log[end];
                let running = end == self.log.len() - 1 && self.busy() && !matches!(last.level, Level::Ok | Level::Failed);
                let took = if running { start.at.elapsed() } else { last.at.duration_since(start.at) };
                if log_heading(ui, start, last.level, running, took).clicked() {
                    toggle = Some(i);
                }
                if start.open {
                    ui.indent(("log", i), |ui| self.log[i + 1..=end].iter().for_each(|l| log_line(ui, l)));
                    ui.add_space(4.0);
                }
                i = end + 1;
            }
        });
        if let Some(i) = toggle {
            self.log[i].open = !self.log[i].open;
        }
    }

    /// The log folded to a strip: unfolds on a click, and marks what it has not shown.
    fn log_rail(&mut self, ui: &mut egui::Ui) {
        let (rect, r) = ui.allocate_exact_size(ui.available_size(), egui::Sense::click());
        let color = if r.hovered() { ui.visuals().strong_text_color() } else { ui.visuals().text_color() };
        let top = rect.center_top();
        chevron(ui.painter(), top + egui::vec2(0.0, 14.0), egui::vec2(-1.0, 0.0), color);
        let mut y = top.y + 30.0;
        if let Some(unread) = self.unread() {
            badge(ui, egui::pos2(top.x, y + 8.0), unread);
            y += 26.0;
        }
        // "Log" reading upwards.
        let galley = ui.painter().layout_no_wrap("Log".into(), egui::TextStyle::Body.resolve(ui.style()), color);
        let pos = egui::pos2(top.x - galley.size().y / 2.0, y + galley.size().x);
        ui.painter().add(egui::epaint::TextShape::new(pos, galley, color).with_angle(-std::f32::consts::FRAC_PI_2));
        if r.on_hover_text("Show the log").clicked() {
            self.log_collapsed = false;
        }
    }

    /// The open tab's device: whether it is there, what it is, and a detail.
    fn device_status(&self) -> Option<(bool, &'static str, String)> {
        Some(match self.tab {
            Tab::Cloner if self.cloner_up => (true, "Camera in USB boot mode", "USB a108:c309".to_string()),
            Tab::Cloner => (false, "No camera in USB boot mode", String::new()),
            Tab::Update | Tab::Tuning if self.camera_up => {
                let slot = match &self.cam_info {
                    Some(Ok(i)) => xu::record_value(i, "running_slot").map(|s| format!("connected · slot {}", s.to_uppercase())),
                    _ => None,
                };
                (true, "Camera running k1cam", slot.unwrap_or_else(|| "attached".into()))
            }
            Tab::Update | Tab::Tuning => (false, "No camera running k1cam", String::new()),
            Tab::Console if self.console.connected() => (true, "Console connected", String::new()),
            Tab::Console if self.console.found() => (false, "Console found, not connected", String::new()),
            Tab::Console => (false, "No camera console found", String::new()),
            Tab::Help => return None,
        })
        .map(|(up, text, detail)| match &self.remote {
            Some(r) if detail.is_empty() => (up, text, format!("on {}", r.host)),
            Some(r) => (up, text, format!("{detail} · on {}", r.host)),
            None => (up, text, detail),
        })
    }

    /// The tint behind the status bar: blue while a job runs, green while the
    /// open tab's device is there, grey otherwise.
    fn status_fill(&self, ui: &egui::Ui) -> Color32 {
        let base = ui.visuals().panel_fill;
        match (self.busy(), self.device_status()) {
            (true, _) => base.lerp_to_gamma(ui.visuals().selection.bg_fill, 0.45),
            (false, Some((true, ..))) => base.lerp_to_gamma(ok_color(ui), 0.16),
            _ => base.lerp_to_gamma(ui.visuals().text_color(), 0.08),
        }
    }

    /// The open tab's device on the left; the running job on the right.
    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let (weak, strong) = (weak_color(ui), ui.visuals().strong_text_color());
        let device = self.device_status();
        egui::Sides::new().height(26.0).shrink_right().show(
            ui,
            |ui| {
                let Some((up, text, detail)) = device else { return };
                let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 5.0, if up { ok_color(ui) } else { weak });
                ui.label(RichText::new(text).size(16.0).color(strong));
                ui.label(RichText::new(detail).color(weak));
            },
            |ui| {
                let Some(job) = &self.job else { return };
                let stopping = self.cancel.load(Ordering::Relaxed);
                if ui.add_enabled(!stopping, egui::Button::new("Cancel")).clicked() {
                    self.cancel.store(true, Ordering::Relaxed);
                }
                let mut time = clock(job.started.elapsed());
                let step = job.step.elapsed();
                if job.progress >= 0.03 && job.progress < 1.0 && step > Duration::from_secs(3) {
                    time += &format!(" · about {} left", clock(step.mul_f32((1.0 - job.progress) / job.progress)));
                }
                ui.label(RichText::new(time).color(weak));
                ui.separator();
                if job.progress > 0.0 {
                    ui.label(RichText::new(format!("{:.0}%", job.progress * 100.0)).size(16.0).color(strong));
                }
                let bar = egui::ProgressBar::new(job.progress).desired_width(220.0).desired_height(10.0);
                ui.add(if job.progress == 0.0 { bar.animate(true) } else { bar });
                // The job's name, then what it said last.
                let said = match self.log.last() {
                    _ if stopping => "stopping",
                    Some(l) if l.level == Level::Info => l.text.as_str(),
                    _ => "",
                };
                let mut text = egui::text::LayoutJob::default();
                let font = egui::TextStyle::Body.resolve(ui.style());
                text.append(job.op.title(), 0.0, egui::TextFormat::simple(egui::FontId::proportional(16.0), strong));
                text.append(said, 10.0, egui::TextFormat::simple(font, weak));
                ui.add(egui::Label::new(text).truncate());
            },
        );
    }

    /// The connection menu's button: where the camera is, and a dot for how
    /// the connection is doing.
    fn connection_button(&mut self, ui: &mut egui::Ui) {
        let (text, dot) = match (&self.remote, &self.conn.connecting) {
            (_, Some(_)) => ("Connecting…".to_string(), ui.visuals().hyperlink_color),
            (None, None) => ("This computer".to_string(), ok_color(ui)),
            (Some(r), None) => match r.link() {
                Link::Up => (r.host.clone(), ok_color(ui)),
                Link::Lost { .. } => (format!("{} · reconnecting", r.host), warn_color(ui)),
                Link::Taken(_) => (format!("{} · taken over", r.host), err_color(ui)),
                Link::Refused(_) => (format!("{} · needs the code", r.host), err_color(ui)),
            },
        };
        let icon = egui::IdSalt::new("link-dot");
        let mut atoms = egui::Atoms::new(text);
        atoms.push_left(egui::Atom::custom(icon, egui::vec2(10.0, 10.0)));
        let r = egui::Button::new(atoms).corner_radius(12.0).atom_ui(ui);
        if let Some(rect) = r.rect(icon) {
            ui.painter().circle_filled(rect.center(), 4.0, dot);
        }
        let resp = r.response.on_hover_text("Where the camera is plugged in: this computer, or another one running k1cam-flasher daemon");
        if std::mem::take(&mut self.conn.open) {
            egui::Popup::open_id(ui.ctx(), egui::Popup::default_response_id(&resp));
        }
        if std::mem::take(&mut self.conn.close) {
            egui::Popup::close_id(ui.ctx(), egui::Popup::default_response_id(&resp));
        }
        egui::Popup::from_toggle_button_response(&resp).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
            ui.set_width(430.0);
            self.connection_menu(ui);
        });
    }

    fn connection_menu(&mut self, ui: &mut egui::Ui) {
        let weak = weak_color(ui);
        let busy = self.busy() || self.conn.connecting.is_some() || self.conn.reconnecting.is_some();
        ui.label(RichText::new("Where the camera is plugged in").strong().size(15.0));
        ui.add_space(4.0);
        // What a choice means, under its label.
        let under = |ui: &mut egui::Ui, text: &str| {
            ui.horizontal(|ui| {
                ui.add_space(ui.spacing().icon_width + ui.spacing().icon_spacing);
                ui.label(RichText::new(text).color(weak));
            });
        };
        let r = ui.add_enabled_ui(!busy, |ui| {
            if ui.radio(!self.conn.elsewhere, "This computer").clicked() {
                self.conn.elsewhere = false;
            }
            under(ui, "USB devices attached here");
            if !self.conn.elsewhere && self.remote.is_some() && ui.button("Disconnect and use this computer").clicked() {
                self.use_remote(None);
            }
            if ui.radio(self.conn.elsewhere, "Another computer").clicked() {
                self.conn.elsewhere = true;
            }
            under(ui, "A Linux computer running k1cam-flasher daemon");
        });
        r.response.on_disabled_hover_text("Wait for the operation to finish");
        if self.conn.elsewhere {
            ui.add_space(4.0);
            match self.remote.clone() {
                Some(r) => self.connected_ui(ui, &r),
                None => self.fields_ui(ui, busy),
            }
            ui.add_space(6.0);
            ui.separator();
            ui.label(RichText::new("On the other computer, as a user who can open the camera:").color(weak));
            ui.monospace("# k1cam-flasher daemon");
        }
    }

    /// Host and code, and Connect.
    fn fields_ui(&mut self, ui: &mut egui::Ui, busy: bool) {
        let weak = weak_color(ui);
        let mut go = false;
        egui::Grid::new("remote-fields").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
            ui.label("Host");
            let hint = "name or address";
            let r = ui.add_enabled(!busy, egui::TextEdit::singleline(&mut self.conn.host).hint_text(hint).desired_width(300.0));
            go |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.end_row();
            ui.label("Code");
            let edit = egui::TextEdit::singleline(&mut self.conn.password).hint_text("as the daemon printed it");
            let r = ui.add_enabled(!busy, edit.desired_width(300.0));
            go |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.end_row();
        });
        ui.checkbox(&mut self.conn.remember, "Remember the host on this computer");
        let can = !busy && !self.conn.host.trim().is_empty() && !self.conn.password.trim().is_empty();
        ui.horizontal(|ui| {
            let label = if self.conn.connecting.is_some() { "Connecting…" } else { "Connect" };
            go |= ui.add_enabled(can, egui::Button::new(label).fill(ui.visuals().selection.bg_fill)).clicked();
            if self.conn.connecting.is_some() {
                ui.spinner();
                ui.label(RichText::new(format!("Asking {}…", self.conn.host.trim())).color(weak));
            }
        });
        if go && can {
            self.connect_remote();
        }
        if let Some(e) = self.conn.error.clone() {
            status(ui, warn_color(ui), e);
        }
    }

    /// The host this window controls, and letting it go. Refused, a new
    /// code; taken, taking it back: in place, so that what this window
    /// follows there (an operation, the console) carries on.
    fn connected_ui(&mut self, ui: &mut egui::Ui, r: &Arc<Remote>) {
        let link = r.link();
        let (color, says) = match &link {
            Link::Up => (ok_color(ui), format!("Connected to {}", r.addr)),
            Link::Lost { since, error } => {
                (warn_color(ui), format!("No answer from {} for {} s: {error}", r.addr, since.elapsed().as_secs()))
            }
            Link::Taken(e) | Link::Refused(e) => (err_color(ui), e.clone()),
        };
        status(ui, color, says.clone());
        let h = &r.hello;
        let name = if r.name.is_empty() || r.name == r.host { String::new() } else { format!(" · it calls itself {}", r.name) };
        ui.label(RichText::new(format!("k1cam-flasher {} · {} {}{name}", h.version, h.os, h.arch)).color(weak_color(ui)));
        if matches!(link, Link::Up | Link::Lost { .. }) {
            ui.label(format!(
                "This window controls the camera there until you disconnect. If it stops answering, the host frees the camera {} s later; \
                 an operation that has started keeps running.",
                api::LEASE_MS / 1000
            ));
        }
        let trying = self.conn.reconnecting.is_some();
        let mut go = None;
        match link {
            Link::Refused(_) => {
                ui.horizontal(|ui| {
                    ui.label("Code");
                    let edit = egui::TextEdit::singleline(&mut self.conn.password).hint_text("from its terminal");
                    let field = ui.add_enabled(!trying, edit.desired_width(140.0));
                    if std::mem::take(&mut self.conn.focus_code) {
                        field.request_focus();
                    }
                    let code = self.conn.password.trim().to_string();
                    let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let button = egui::Button::new("Use this code").fill(ui.visuals().selection.bg_fill);
                    if (ui.add_enabled(!trying && !code.is_empty(), button).clicked() || enter) && !code.is_empty() {
                        go = Some(Some(code));
                    }
                });
            }
            Link::Taken(_) => {
                if ui.add_enabled(!trying, egui::Button::new("Connect again")).clicked() {
                    go = Some(None);
                }
            }
            Link::Up | Link::Lost { .. } => {}
        }
        if trying {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new(format!("Asking {}…", r.host)).color(weak_color(ui)));
            });
        } else if let Some(e) = self.conn.error.clone().filter(|e| *e != says) {
            // A code refused again says so above; the field keeps it, to check.
            status(ui, warn_color(ui), e);
        }
        let idle = !self.busy() && !trying;
        if ui.add_enabled(idle, egui::Button::new("Disconnect")).on_disabled_hover_text("Wait for the operation to finish").clicked() {
            self.use_remote(None);
        }
        if let Some(code) = go {
            let (tx, rx) = channel();
            let r = r.clone();
            std::thread::spawn(move || {
                let _ = tx.send(r.reconnect(code.as_deref()));
            });
            self.conn.error = None;
            self.conn.reconnecting = Some(rx);
        }
    }

    /// `--remote HOST`: opens the connection menu with that host, for the
    /// code its daemon printed.
    fn open_remote(&mut self, host: &str) {
        self.conn.host = host.trim().to_string();
        self.conn.elsewhere = true;
        self.conn.error = Some(format!("Enter the code the daemon on {} printed.", host.trim()));
        self.conn.open = true;
    }

    /// Under the status bar in remote mode: a connection that stopped
    /// answering, and backups the host keeps that are not saved here.
    fn remote_banners(&mut self, ui: &mut egui::Ui) {
        let Some(r) = self.remote.clone() else { return };
        let fill = ui.visuals().panel_fill.lerp_to_gamma(warn_color(ui), 0.18);
        let frame = egui::Frame::new().fill(fill).inner_margin(egui::Margin::symmetric(10, 6));
        let link = r.link();
        match &link {
            Link::Up => {}
            Link::Lost { since, .. } => {
                let job = if self.busy() {
                    " The operation carries on there; this window catches up with it once the connection is back."
                } else {
                    ""
                };
                let text =
                    format!("Lost the connection to {} {} s ago.{job} Trying again every second.", r.host, since.elapsed().as_secs());
                egui::Panel::top("link").frame(frame).show(ui, |ui| status(ui, warn_color(ui), text));
            }
            Link::Taken(e) | Link::Refused(e) => {
                let label = if matches!(link, Link::Refused(_)) { "Enter the code…" } else { "Connect again…" };
                let job = if self.busy() { " The operation carries on there meanwhile." } else { "" };
                egui::Panel::top("link").frame(frame).show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        if ui.small_button(label).clicked() {
                            self.conn.open = true;
                        }
                        ui.label(format!("{e}.{job}"));
                    });
                });
            }
        }
        let Some(j) = self.held.first().cloned() else { return };
        egui::Panel::top("held").frame(frame).show(ui, |ui| {
            ui.horizontal(|ui| {
                let more = if self.held.len() > 1 { format!(" (and {} more)", self.held.len() - 1) } else { String::new() };
                let size = j.output.as_ref().map_or(0, |o| o.size);
                let when = wall_clock(std::time::UNIX_EPOCH + Duration::from_secs(j.started));
                ui.label(format!(
                    "{} keeps a backup that isn't saved here: {}, {} from {when}{more}.",
                    r.host,
                    Op::from_name(&j.op).title(),
                    bytes(size)
                ));
                let idle = !self.busy();
                if ui.add_enabled(idle, egui::Button::new("Save…")).clicked() {
                    if let Some(path) = rfd::FileDialog::new().set_file_name(backup_name()).save_file() {
                        let (r, id) = (r.clone(), j.id.clone());
                        self.held.retain(|h| h.id != id);
                        self.start(Op::Fetch, move |_| r.save_output(&id, &path).map(|sha| format!("{} sha256 {sha}", path.display())));
                    }
                }
                let d = ui.add_enabled(idle, egui::Button::new("Discard…"));
                egui::Popup::from_toggle_button_response(&d).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
                    ui.label(format!("Delete this backup on {}? It isn't saved anywhere else.", r.host));
                    if ui.button("Delete it").clicked() {
                        let (r, id) = (r.clone(), j.id.clone());
                        self.held.retain(|h| h.id != id);
                        self.note(Level::Info, format!("Discarded the backup {} kept from {when}", r.host));
                        std::thread::spawn(move || r.dismiss(&id));
                    }
                });
            });
        });
    }

    /// Files dropped on the window: a .upd for the A/B tab, a .bin to flash.
    fn take_dropped(&mut self, ctx: &egui::Context) {
        if self.busy() {
            return;
        }
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        for p in dropped {
            match p.extension().and_then(|e| e.to_str()) {
                Some("upd") => {
                    self.load_bundle(p);
                    self.tab = Tab::Update;
                }
                Some("bin") => {
                    self.image = Some(Image::load(p));
                    self.confirm = false;
                    self.tab = Tab::Cloner;
                }
                _ => self.note(Level::Info, format!("{} is neither a k1cam.upd nor a .bin image; ignored", p.display())),
            }
            self.tab_chosen = true;
        }
    }

    /// Asks before quitting in the middle of a job.
    fn guard_close(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested()) && self.busy() && !self.quit_after && !self.quit_anyway {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.quit_prompt = true;
        }
        if self.quit_after && !self.busy() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        let Some(op) = self.job.as_ref().map(|j| j.op).filter(|_| self.quit_prompt) else {
            self.quit_prompt = false;
            return;
        };
        let m = egui::Modal::new(egui::Id::unique("quit")).show(ctx, |ui| {
            ui.set_max_width(400.0);
            ui.heading(format!("{} is running", op.title()));
            ui.add_space(4.0);
            match &self.remote {
                Some(r) => ui.label(format!(
                    "It runs on {}, and carries on there if this window closes; connecting again later shows how it ended. {}",
                    r.host,
                    op.risk()
                )),
                None => ui.label(op.risk()),
            };
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Keep it running").clicked() {
                    self.quit_prompt = false;
                }
                if self.remote.is_some() && ui.button("Quit; it carries on there").clicked() {
                    self.quit_anyway = true;
                    self.quit_prompt = false;
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if ui.button("Stop it, then quit").clicked() {
                    self.cancel.store(true, Ordering::Relaxed);
                    self.quit_after = true;
                    self.quit_prompt = false;
                }
            });
        });
        if m.should_close() {
            self.quit_prompt = false;
        }
    }
}

/// The two slots and what each is to the camera, in the caller's row.
fn slots(ui: &mut egui::Ui, info: &str) {
    let get = |k| xu::record_value(info, k).unwrap_or("");
    for s in ["a", "b"] {
        let mut tags = Vec::new();
        if get("running_slot") == s {
            tags.push("running");
        }
        if get("active_slot") == s {
            tags.push("confirmed");
        }
        if get("trial_slot") == s {
            tags.push("trial");
        }
        let running = get("running_slot") == s;
        let stroke = if running { egui::Stroke::new(1.5, ok_color(ui)) } else { ui.visuals().widgets.noninteractive.bg_stroke };
        egui::Frame::new().stroke(stroke).corner_radius(4.0).inner_margin(egui::Margin::symmetric(8, 2)).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("Slot {}", s.to_uppercase())).strong());
                let tags = if tags.is_empty() { "inactive".to_string() } else { tags.join(", ") };
                ui.label(RichText::new(tags).color(weak_color(ui)));
            });
        });
    }
}

/// The camera next to the bundle, with what would stop the camera taking it
/// marked (xu::install_slot has the final word). Every row is there before
/// either is known, so that neither moves what follows.
fn compare(ui: &mut egui::Ui, info: Option<&str>, b: Option<&bundle::Bundle>) {
    let (ok, bad, weak) = (ok_color(ui), err_color(ui), weak_color(ui));
    let cam = |k: &str| info.and_then(|i| xu::record_value(i, k)).unwrap_or("");
    let release = |k: &str| b.and_then(|b| b.release_value(k)).unwrap_or("");
    let mono = |s: &str| if s.is_empty() { RichText::new("—").color(weak) } else { RichText::new(s).monospace() };
    let mark = |t: RichText, good: bool| if info.is_none() { t } else { t.color(if good { ok } else { bad }) };
    egui::Grid::new("compare").num_columns(3).spacing([16.0, 4.0]).striped(true).show(ui, |ui| {
        let row = |ui: &mut egui::Ui, label: &str, cam: RichText, bundle: RichText| {
            ui.label(label);
            ui.label(cam);
            ui.label(bundle);
            ui.end_row();
        };
        row(ui, "", RichText::new("Camera").color(weak), RichText::new("This bundle").color(weak));
        let build = match b {
            Some(b) if b.build == cam("build") => mono(&b.build).color(weak),
            Some(b) => mono(&b.build),
            None => RichText::new("choose a bundle…").color(weak),
        };
        row(ui, "Build", mono(cam("build")), build);
        row(ui, "Commit", mono(""), mono(release("COMMIT_ID")));
        let kernel: String = release("KERNEL_COMMIT").chars().take(12).collect();
        row(ui, "Kernel", mono(cam("kernel")), mono(&kernel));
        row(ui, "Buildroot", mono(""), mono(release("BUILDROOT_VERSION")));
        row(ui, "Board", mono(cam("board")), b.map_or(mono(""), |b| mark(mono(&b.board), cam("board") == b.board)));
        row(ui, "Layout", mono(cam("layout")), b.map_or(mono(""), |b| mark(mono(&b.layout), cam("layout") == b.layout)));
        let compat = cam("boot_compat").parse::<u32>().unwrap_or(0);
        let needs = b.map_or(mono(""), |b| mark(RichText::new(format!("needs {}", b.min_boot)), b.min_boot <= compat));
        row(ui, "Boot compat", mono(cam("boot_compat")), needs);
        let sizes =
            [("Kernel", b.map(|b| b.kernel_len as u64), "kernel_capacity"), ("Rootfs", b.map(|b| b.rootfs_len as u64), "rootfs_capacity")];
        for (label, len, key) in sizes {
            let cap = cam(key).parse::<u64>().unwrap_or(0);
            let slot = if cap == 0 { mono("") } else { RichText::new(format!("{} slot", bytes(cap))) };
            let size = match (len, cap) {
                (None, _) => mono(""),
                (Some(len), 0) => RichText::new(bytes(len)),
                (Some(len), cap) => mark(RichText::new(format!("{} ({}%)", bytes(len), len * 100 / cap)), len <= cap),
            };
            row(ui, &format!("{label} size"), slot, size);
        }
    });
}

/// Help, made easy to find: a filled circle with a question mark, painted.
fn help_button(ui: &mut egui::Ui, selected: bool) -> bool {
    let icon = egui::IdSalt::new("help-icon");
    let mut atoms = egui::Atoms::new("Help");
    atoms.push_left(egui::Atom::custom(icon, egui::vec2(16.0, 16.0)));
    let accent = ui.visuals().hyperlink_color;
    // The ring is painted over the button: a stroke given to the button would
    // undo the style's padding compensation and resize it on hover.
    let r = egui::Button::new(atoms).selected(selected).corner_radius(12.0).atom_ui(ui);
    let painter = ui.painter();
    if !selected && !r.response.hovered() {
        painter.rect_stroke(r.response.rect, 12.0, egui::Stroke::new(1.0, accent.gamma_multiply(0.6)), egui::StrokeKind::Inside);
    }
    if let Some(rect) = r.rect(icon) {
        painter.circle_filled(rect.center(), 8.0, accent);
        let font = egui::FontId::proportional(13.0);
        painter.text(rect.center() + egui::vec2(0.0, 0.5), egui::Align2::CENTER_CENTER, "?", font, ui.visuals().panel_fill);
    }
    r.response.on_hover_text("Help: USB boot mode, drivers, about").clicked()
}

/// One line of the log, timed.
fn log_line(ui: &mut egui::Ui, l: &Line) {
    ui.horizontal_top(|ui| {
        ui.label(RichText::new(&l.time).monospace().color(weak_color(ui)));
        let t = RichText::new(&l.text).monospace();
        let t = match l.level {
            Level::Ok => t.color(ok_color(ui)),
            Level::Failed => t.color(err_color(ui)),
            _ => t,
        };
        ui.add(egui::Label::new(t).wrap());
    });
}

/// An operation's heading in the log: folds its lines, and says how it went.
fn log_heading(ui: &mut egui::Ui, start: &Line, end: Level, running: bool, took: Duration) -> egui::Response {
    let height = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
    let (rect, r) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::click());
    let painter = ui.painter();
    if r.hovered() {
        painter.rect_filled(rect, 3.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let y = rect.center().y;
    let dir = if start.open { egui::vec2(0.0, 1.0) } else { egui::vec2(1.0, 0.0) };
    chevron(painter, egui::pos2(rect.left() + 8.0, y), dir, weak_color(ui));
    let mark = egui::Rect::from_center_size(egui::pos2(rect.left() + 24.0, y), egui::vec2(10.0, 10.0));
    if running {
        egui::Spinner::new().size(10.0).paint_at(ui, mark);
    } else {
        let color = match end {
            Level::Ok => ok_color(ui),
            Level::Failed => err_color(ui),
            _ => weak_color(ui),
        };
        painter.circle_filled(mark.center(), 4.0, color);
    }
    let meta = format!("{} · {}", start.time, clock(took));
    painter.text(egui::pos2(rect.right() - 6.0, y), egui::Align2::RIGHT_CENTER, meta, egui::FontId::proportional(12.0), weak_color(ui));
    let font = egui::TextStyle::Body.resolve(ui.style());
    painter.text(egui::pos2(rect.left() + 36.0, y), egui::Align2::LEFT_CENTER, &start.text, font, ui.visuals().strong_text_color());
    r
}

impl eframe::App for App {
    /// What goes on whether or not the window shows: jobs, devices, the
    /// console and the preview.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain();
        self.poll_remote();
        self.interlock_tabs();
        if !self.busy() && self.last_poll.elapsed() > Duration::from_secs(1) {
            match &self.remote {
                Some(r) => {
                    let s = r.status();
                    (self.cloner_up, self.camera_up) = (s.cloner, s.camera);
                }
                None => {
                    self.cloner_up = cloner::device_present();
                    self.camera_up = xu::Camera::present();
                }
            }
            if !self.camera_up {
                self.cam_info = None;
                self.info_rx = None;
            }
            if !self.tab_chosen {
                if self.cloner_up && !self.camera_up {
                    self.tab = Tab::Cloner;
                }
                self.tab_chosen = true;
            }
            if let Some(path) = self.bundle.as_ref().filter(|l| l.changed()).map(|l| l.path.clone()) {
                self.note(Level::Info, format!("{} changed on disk, reloaded", path.display()));
                self.load_bundle(path);
            }
            self.last_poll = Instant::now();
        }
        let read = self.info_reader();
        self.poll_camera_info(read);
        self.console.poll();
        let shown = self.tab == Tab::Tuning && !ctx.input(|i| i.viewport().minimized.unwrap_or(false));
        if !self.busy() {
            self.tuning.poll(ctx, shown, self.camera_up);
        }
        for l in self.tuning.take_log() {
            match l.strip_prefix("FAILED: ") {
                Some(e) => self.note(Level::Failed, e),
                None => self.note(Level::Info, l),
            }
        }
        // This computer's camera is let go once nothing here uses it.
        if !(self.busy() || self.reading_info() || self.tuning.in_use() || self.console.in_use()) {
            self.own.release();
        }
        ctx.request_repaint_after(Duration::from_millis(250));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.take_dropped(&ctx);
        self.guard_close(&ctx);

        // The tabs stay open during a job, but for the A/B update: the status
        // bar follows it from any of them.
        egui::Panel::top("tabs").show(ui, |ui| {
            ui.add_space(6.0);
            let (tab, locked) = (self.tab, self.ab_running());
            let (left, right) = egui::Sides::new().show(
                ui,
                |ui| {
                    let mut pick = None;
                    let mut item = |ui: &mut egui::Ui, t: Tab, label: &str, tip: &str| {
                        let b = ui.add_enabled(!locked || t == Tab::Update, egui::Button::selectable(tab == t, label));
                        if b.on_hover_text(tip).on_disabled_hover_text("Open once the A/B update ends").clicked() {
                            pick = Some(t);
                        }
                    };
                    item(
                        ui,
                        Tab::Cloner,
                        "Backup & Flash",
                        "The whole 8 MiB flash, with Ingenic's cloner, through the boot ROM's USB boot mode: for installing k1cam once, \
                         and for a camera that no longer boots.",
                    );
                    // USB boot mode on the left, a camera running k1cam on the right.
                    ui.separator();
                    item(
                        ui,
                        Tab::Update,
                        "A/B Update",
                        "Updates a camera running k1cam firmware over USB. The update goes into the slot it isn't running and \
                         boots as a trial: the camera keeps it once it works, and goes back to the old one by itself otherwise.",
                    );
                    item(ui, Tab::Tuning, "Tuning", "The live picture and the camera's image controls");
                    item(ui, Tab::Console, "Console", "The camera's serial console");
                    pick
                },
                |ui| {
                    let help = help_button(ui, tab == Tab::Help);
                    self.connection_button(ui);
                    help.then_some(Tab::Help)
                },
            );
            if let Some(t) = left.or(right) {
                self.tab = t;
            }
            ui.add_space(4.0);
        });
        // Right under the tabs, where it is seen; Help has no device.
        if self.busy() || self.tab != Tab::Help {
            let frame = egui::Frame::new().fill(self.status_fill(ui)).inner_margin(egui::Margin::symmetric(10, 6));
            egui::Panel::top("status").frame(frame).show(ui, |ui| self.status_bar(ui));
        }
        self.remote_banners(ui);
        // The log sits beside the tabs that run jobs.
        if matches!(self.tab, Tab::Cloner | Tab::Update) {
            if self.log_collapsed {
                egui::Panel::right("log-rail").resizable(false).exact_size(30.0).show(ui, |ui| self.log_rail(ui));
            } else {
                egui::Panel::right("log")
                    .resizable(true)
                    .default_size(380.0)
                    .min_size(260.0)
                    .max_size(720.0)
                    .show(ui, |ui| self.log_panel(ui));
                self.log_seen = self.log.len();
            }
        }
        egui::CentralPanel::default().show(ui, |ui| {
            if self.busy() {
                ui.disable();
            }
            match self.tab {
                Tab::Cloner => {
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| self.cloner_tab(ui));
                }
                Tab::Update => {
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| self.update_tab(ui));
                }
                Tab::Tuning => self.tuning.ui(ui, self.camera_up),
                Tab::Console => self.console.ui(ui),
                Tab::Help => {
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| self.help_tab(ui));
                }
            }
        });
        // Dragging files over the window says what dropping them does.
        if !self.busy() && ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("drop")));
            let rect = ctx.content_rect();
            painter.rect_filled(rect, 0.0, Color32::from_black_alpha(170));
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Drop a k1cam.upd to update, or a .bin image to flash",
                egui::FontId::proportional(20.0),
                Color32::WHITE,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_valid_bundle_allows_an_automatic_info_read() {
        let mut app = App::new();
        app.camera_up = true;
        let read = || Ok("build=test\n".into());
        app.poll_camera_info(read);
        assert!(app.info_rx.is_none(), "an attachment alone must not query the camera");

        app.load_bundle("tests/data/missing.upd".into());
        app.poll_camera_info(read);
        assert!(app.info_rx.is_none(), "a bad bundle must not query the camera");

        app.load_bundle("tests/data/golden.upd".into());
        assert!(app.bundle.as_ref().unwrap().bundle.is_ok());
        app.camera_up = false;
        app.poll_camera_info(read);
        assert!(app.info_rx.is_none());
        app.camera_up = true;
        app.poll_camera_info(read);
        app.info_thread.take().expect("a valid bundle needs a comparison").join().unwrap();
        app.poll_camera_info(read);
        assert_eq!(app.cam_info.as_ref().unwrap().as_deref(), Ok("build=test\n"));
        assert!(app.info_rx.is_none(), "a completed comparison must not repeat");

        app.cam_info = Some(Err("camera did not answer".into()));
        app.poll_camera_info(read);
        assert!(app.info_rx.is_none(), "failed comparisons wait for an explicit retry");
    }

    #[test]
    fn an_ab_update_keeps_its_tab_and_other_jobs_do_not() {
        let running = |op| Running { op, rx: channel().1, started: Instant::now(), progress: 0.0, step: Instant::now() };
        let mut app = App::new();
        for op in [Op::Update, Op::Reboot, Op::Cancel] {
            app.tab = Tab::Console;
            app.job = Some(running(op));
            app.interlock_tabs();
            assert!(app.tab == Tab::Update && app.shown_tab == Tab::Update);
            app.tab = Tab::Help;
            app.interlock_tabs();
            assert!(app.tab == Tab::Help, "Help stays open to it");
        }
        app.job = Some(running(Op::Dump));
        app.tab = Tab::Tuning;
        app.interlock_tabs();
        assert!(app.tab == Tab::Tuning && app.shown_tab == Tab::Tuning);
    }

    #[test]
    fn jobs_wait_for_camera_access_and_cancel_before_starting() {
        for cancel in [false, true] {
            let mut app = App::new();
            let (release, held) = channel();
            app.info_thread = Some(std::thread::spawn(move || held.recv().unwrap()));
            // A valid bundle must not launch a comparison during the job either.
            app.load_bundle("tests/data/golden.upd".into());
            app.camera_up = true;
            let (ran, started) = channel();
            app.start(Op::Flash, move |_| {
                ran.send(()).unwrap();
                Ok("done".into())
            });
            assert!(app.busy());
            app.poll_camera_info(|| panic!("comparison during a firmware job"));
            assert!(app.info_rx.is_none());
            let events = &app.job.as_ref().unwrap().rx;
            assert!(matches!(events.recv_timeout(Duration::from_secs(5)), Ok(Event::Log(_))));
            assert!(started.try_recv().is_err(), "the operation started before the device closed");
            app.cancel.store(cancel, Ordering::Relaxed);
            release.send(()).unwrap();
            let Ok(Event::Done(result)) = events.recv_timeout(Duration::from_secs(5)) else { panic!("no job result") };
            if cancel {
                assert_eq!(result, Err("cancelled".into()));
                assert!(started.try_recv().is_err());
            } else {
                assert_eq!(result, Ok("done".into()));
                assert!(started.try_recv().is_ok());
            }
        }
    }
}

/// The GUI; `args` may name a tab (--tab=update, --tab=tuning, --tab=help, …),
/// a .upd to open, or a host to connect to (--remote HOST[:PORT]).
pub fn run(args: &[String]) -> eframe::Result {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([800.0, 560.0])
            .with_title("k1cam flasher"),
        ..Default::default()
    };
    let args = args.to_vec();
    eframe::run_native(
        "k1cam-flasher",
        opts,
        Box::new(move |cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            let mut app = App::new();
            let mut it = args.iter();
            while let Some(arg) = it.next() {
                let host = match arg.strip_prefix("--remote") {
                    Some("") => it.next().cloned(),
                    Some(h) => h.strip_prefix('=').map(str::to_string),
                    None => None,
                };
                if let Some(host) = host {
                    app.open_remote(&host);
                    continue;
                }
                let tab = match arg.as_str() {
                    "--tab=cloner" => Tab::Cloner,
                    "--tab=update" => Tab::Update,
                    "--tab=tuning" => Tab::Tuning,
                    "--tab=console" => Tab::Console,
                    "--tab=help" => Tab::Help,
                    path if path.ends_with(".upd") => {
                        app.load_bundle(path.into());
                        Tab::Update
                    }
                    _ => continue,
                };
                app.tab = tab;
                app.tab_chosen = true;
            }
            Ok(Box::new(app))
        }),
    )
}
