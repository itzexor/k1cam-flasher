// SPDX-License-Identifier: MIT
//! The GUI: cloner backup and flash, A/B update, tuning with a live preview,
//! the camera's console, and help on forcing cloner mode.

use crate::job::{Event, Job};
use crate::{bundle, cloner, console, ops, tuning, xu};
use eframe::egui::{self, Color32, RichText};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;
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
}

impl Op {
    fn title(self) -> &'static str {
        match self {
            Op::Detect => "Detect flash chip",
            Op::Backup => "Backup",
            Op::Flash => "Flash",
            Op::Dump => "Dump flash",
            Op::Update => "A/B update",
            Op::Reboot => "Reboot into trial",
            Op::Cancel => "Cancel trial",
        }
    }

    /// What quitting halfway leaves behind.
    fn risk(self) -> &'static str {
        match self {
            Op::Flash => "Stopping a flash halfway leaves the camera unable to boot until it is flashed again in cloner mode.",
            Op::Update => "The camera keeps booting what it runs now; an install it has already started still finishes.",
            Op::Backup | Op::Dump => "Stopping now leaves no backup.",
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
}

struct App {
    tab: Tab,
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
    /// The log shows (it also does while a job runs), and how many of its
    /// lines have been seen.
    log_open: bool,
    log_seen: usize,
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

/// Sizes as the firmware's documents give them: whole MiB, else KiB.
fn bytes(n: u64) -> String {
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

/// A tab's title and what it is for.
fn intro(ui: &mut egui::Ui, title: &str, text: &str) {
    ui.heading(title);
    ui.label(RichText::new(text).color(weak_color(ui)));
    ui.add_space(8.0);
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
        Self {
            tab: Tab::Cloner,
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
            console: console::Console::new(),
            tuning: tuning::Tuning::new(),
            quit_prompt: false,
            quit_after: false,
            log_open: false,
            log_seen: 0,
        }
    }

    fn busy(&self) -> bool {
        self.job.is_some()
    }

    fn note(&mut self, level: Level, text: impl Into<String>) {
        self.log.push(Line { level, text: text.into() });
    }

    fn start<F>(&mut self, op: Op, f: F)
    where
        F: FnOnce(&Job) -> Result<String, String> + Send + 'static,
    {
        if self.busy() {
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
        // A job's progress and what it says show in the log.
        self.log_open = true;
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
        let (op, took) = (job.op, job.started.elapsed());
        for l in lines {
            self.note(Level::Info, l);
        }
        let Some(r) = done else { return };
        let took = if took > Duration::from_secs(2) { format!(" ({})", clock(took)) } else { String::new() };
        match &r {
            Ok(m) => self.note(Level::Ok, format!("{m}{took}")),
            Err(e) => self.note(Level::Failed, format!("{e}{took}")),
        }
        self.results.insert(op, r);
        self.job = None;
        // A job may have changed what the camera reports.
        self.cam_info = None;
    }

    /// A valid bundle may read the camera for comparison; other reads need Connect.
    fn poll_camera_info(&mut self, read: fn() -> Result<String, String>) {
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

    fn read_camera_info(&mut self, read: fn() -> Result<String, String>) {
        if !self.camera_up || self.busy() || self.reading_info() {
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

    /// The last outcome of `op`, if it ran since this tab was set up.
    fn result(&self, ui: &mut egui::Ui, op: Op) {
        match self.results.get(&op) {
            Some(Ok(m)) => {
                status(ui, ok_color(ui), RichText::new(m).color(ok_color(ui)));
            }
            Some(Err(e)) => {
                status(ui, err_color(ui), RichText::new(format!("{} failed: {e}", op.title())).color(err_color(ui)));
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

    fn cloner_tab(&mut self, ui: &mut egui::Ui) {
        intro(
            ui,
            "Backup & Flash",
            "The whole 8 MiB flash, through the Ingenic boot ROM's USB mode (cloner): for installing k1cam once, \
             and for a camera that no longer boots.",
        );
        let idle = !self.busy() && self.cloner_up;
        card(ui, "Camera in cloner mode", |ui| {
            if self.cloner_up {
                status(ui, ok_color(ui), "Attached (USB a108:c309)");
                if ui
                    .add_enabled(idle, egui::Button::new("Detect flash chip"))
                    .on_hover_text("Checks the link and identifies the flash, without changing it")
                    .clicked()
                {
                    self.start(Op::Detect, |j| {
                        let c = cloner::Cloner::probe(j)?;
                        Ok(format!("{}, {} (JEDEC {:06x})", c.name, bytes(c.size as u64), c.jedec))
                    });
                }
            } else {
                status(ui, weak_color(ui), "Not connected");
                ui.label("The camera answers here when it powers up with two pins of its flash chip shorted.");
                if ui.link("How to short the pins").clicked() {
                    self.tab = Tab::Help;
                }
            }
            self.result(ui, Op::Detect);
        });
        card(ui, "Backup", |ui| {
            ui.label("Reads the whole flash into a file and shows its SHA-256.");
            if ui.add_enabled(idle, egui::Button::new("Back up flash…")).clicked() {
                if let Some(path) = rfd::FileDialog::new().set_file_name(backup_name()).save_file() {
                    self.start(Op::Backup, move |j| ops::backup(j, &path));
                }
            }
            self.result(ui, Op::Backup);
        });
        card(ui, "Flash", |ui| {
            ui.label("Replaces the whole flash: U-Boot, both slots and the settings. This installs the A/B layout, once; after that, update with k1cam.upd on the A/B Update tab.");
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("Choose image…").clicked() {
                    self.choose_image();
                }
                file_row(ui, self.image.as_ref().map(|i| i.path.as_path()), "no image chosen (k1cam.bin)");
            });
            match self.image.as_ref().map(|i| &i.check) {
                Some(Ok(())) => {
                    status(ui, ok_color(ui), "8 MiB, a whole-flash image");
                }
                Some(Err(e)) => {
                    status(ui, err_color(ui), RichText::new(e).color(err_color(ui)));
                }
                None => {}
            }
            ui.add_space(4.0);
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
            ui.add_space(2.0);
            if ui.add_enabled(can, button).clicked() {
                let image = image.unwrap();
                let (b, v, r) = (self.backup_first, self.verify, self.reboot_after);
                self.confirm = false;
                self.start(Op::Flash, move |j| ops::flash(j, &image, b, v, r));
            }
            self.result(ui, Op::Flash);
        });
    }

    fn update_tab(&mut self, ui: &mut egui::Ui) {
        intro(
            ui,
            "A/B Update",
            "Updates a camera running k1cam firmware over USB. The update goes into the slot it isn't running and \
             boots as a trial: the camera keeps it once it works, and goes back to the old one by itself otherwise.",
        );
        if cfg!(windows) {
            status(ui, warn_color(ui), "Not available on Windows yet; use Linux or macOS, or the cloner on the Backup & Flash tab.");
            ui.add_space(8.0);
        }
        // Not while the info record is being read: one exchange at a time.
        let idle = !self.busy() && self.info_rx.is_none() && self.camera_up && !cfg!(windows);
        card(ui, "Camera", |ui| self.camera_card(ui, idle));
        card(ui, "Update", |ui| self.bundle_card(ui, idle));
        card(ui, "Backup", |ui| {
            ui.label("Saves the whole flash of the running camera (8 MiB; it takes about three minutes).");
            ui.horizontal(|ui| {
                if ui.add_enabled(idle, egui::Button::new("Dump flash…")).clicked() {
                    if let Some(path) = rfd::FileDialog::new().set_file_name(backup_name()).save_file() {
                        let live = self.live_dump;
                        self.start(Op::Dump, move |j| ops::dump(j, &path, live));
                    }
                }
                ui.checkbox(&mut self.live_dump, "Live").on_hover_text(
                    "For a camera without the RAM for a snapshot: reads the flash as it changes, so the copy may be inconsistent (diagnostic)",
                );
            });
            self.result(ui, Op::Dump);
        });
    }

    fn camera_card(&mut self, ui: &mut egui::Ui, idle: bool) {
        if !self.camera_up {
            status(ui, weak_color(ui), "No camera running k1cam firmware is attached");
            return;
        }
        let label = if matches!(self.cam_info, Some(Ok(_))) { "Refresh" } else { "Connect" };
        if ui.add_enabled(!self.busy() && !self.reading_info(), egui::Button::new(label)).clicked() {
            self.read_camera_info(camera_info);
        }
        let info = match &self.cam_info {
            None => {
                if self.reading_info() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Reading the camera's firmware information…");
                    });
                } else {
                    ui.label("Choose a bundle or connect to read the camera's firmware information.");
                }
                return;
            }
            Some(Err(e)) => {
                status(
                    ui,
                    warn_color(ui),
                    RichText::new(format!("Attached, but its firmware information could not be read: {e}")).color(warn_color(ui)),
                );
                ui.label(RichText::new("Press Connect to try again.").color(weak_color(ui)));
                return;
            }
            Some(Ok(info)) => info.clone(),
        };
        let get = |k| xu::record_value(&info, k).unwrap_or("");
        status(ui, ok_color(ui), "Connected");
        ui.add_space(4.0);
        egui::Grid::new("camera").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
            ui.label("Build");
            ui.monospace(get("build"));
            ui.end_row();
            ui.label("Kernel");
            ui.monospace(get("kernel"));
            ui.end_row();
            ui.label("Slots");
            slots(ui, &info);
            ui.end_row();
            ui.label("Boot");
            ui.label(match get("boot") {
                "confirmed" => "from its confirmed slot".to_string(),
                "trial" => format!("trial, boot {} of {}", get("bootcount"), get("bootlimit")),
                "fallback" => "fallback: the confirmed slot's kernel did not load".into(),
                "single" => "single-slot layout (flash k1cam.bin once for A/B)".into(),
                b => b.to_string(),
            });
            ui.end_row();
        });
        let trial = get("trial_slot").to_string();
        if !trial.is_empty() {
            ui.add_space(4.0);
            let running = get("running_slot") == trial;
            ui.label(if running {
                format!("Slot {} runs as a trial: the camera keeps it once it has streamed video.", trial.to_uppercase())
            } else {
                format!("A trial of slot {} is staged: it starts at the next reboot.", trial.to_uppercase())
            });
            ui.horizontal(|ui| {
                if !running && ui.add_enabled(idle, egui::Button::new("Reboot into trial")).clicked() {
                    self.start(Op::Reboot, |_| xu::Camera::open()?.reboot().map(|_| "rebooting into the trial".into()));
                }
                if ui
                    .add_enabled(idle, egui::Button::new("Cancel trial"))
                    .on_hover_text("Withdraw the trial: the camera stays with its confirmed slot")
                    .clicked()
                {
                    self.start(Op::Cancel, |_| xu::Camera::open()?.cancel_trial().map(|_| "trial cancelled".into()));
                }
            });
        }
        self.result(ui, Op::Reboot);
        self.result(ui, Op::Cancel);
        egui::CollapsingHeader::new("All fields").id_salt("camera-fields").show(ui, |ui| {
            egui::Grid::new("camera-raw").num_columns(2).spacing([16.0, 2.0]).striped(true).show(ui, |ui| {
                for line in info.lines() {
                    let (k, v) = line.split_once('=').unwrap_or((line, ""));
                    ui.label(RichText::new(k).color(weak_color(ui)));
                    ui.monospace(v);
                    ui.end_row();
                }
            });
            if ui.small_button("Copy").clicked() {
                ui.ctx().copy_text(info.clone());
            }
        });
    }

    fn bundle_card(&mut self, ui: &mut egui::Ui, idle: bool) {
        ui.horizontal(|ui| {
            if ui.button("Choose bundle…").clicked() {
                self.choose_bundle();
            }
            file_row(ui, self.bundle.as_ref().map(|l| l.path.as_path()), "no bundle chosen (k1cam.upd)");
        });
        let info = match &self.cam_info {
            Some(Ok(i)) => Some(i.clone()),
            _ => None,
        };
        // What is shown is what gets installed.
        let b = match self.bundle.as_ref().map(|l| &l.bundle) {
            Some(Ok(b)) => b.clone(),
            Some(Err(e)) => {
                status(ui, err_color(ui), RichText::new(e).color(err_color(ui)));
                return;
            }
            None => return,
        };
        ui.add_space(4.0);
        compare(ui, info.as_deref(), &b);
        match &b.release {
            Some(text) => {
                egui::CollapsingHeader::new("os-release").id_salt("os-release").show(ui, |ui| ui.monospace(text.trim_end()));
            }
            None => {
                ui.label(RichText::new("This file has no release information (packed by an older k1cam-mkupd).").color(weak_color(ui)));
            }
        }
        ui.add_space(4.0);
        let verdict = info.as_deref().map(|i| xu::install_slot(i, &b));
        match &verdict {
            Some(Ok(slot)) => {
                status(
                    ui,
                    ok_color(ui),
                    format!("The camera can install this into slot {}, then boots it as a trial.", slot.to_uppercase()),
                );
            }
            Some(Err(e)) => {
                status(ui, warn_color(ui), RichText::new(e).color(warn_color(ui)));
            }
            None => {}
        }
        ui.checkbox(&mut self.reboot_trial, "Reboot into the new slot when it is staged");
        let label = match &verdict {
            Some(Ok(slot)) => format!("Install to slot {}", slot.to_uppercase()),
            _ => "Install to the inactive slot".into(),
        };
        let can = idle && !matches!(verdict, Some(Err(_)));
        let mut button = egui::Button::new(RichText::new(label).strong());
        if can {
            button = button.fill(ui.visuals().selection.bg_fill);
        }
        if ui.add_enabled(can, button).clicked() {
            let reboot = self.reboot_trial;
            self.start(Op::Update, move |j| ops::update(j, &b, reboot));
        }
        self.result(ui, Op::Update);
    }

    fn help_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Getting the camera into cloner mode");
        ui.label("The camera only talks the cloner protocol when its SoC cannot boot from the flash. Shorting two of the flash chip's pins while it powers up does that.");
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
        ui.label("Bind WinUSB to a108:c309 with Zadig while the camera is in cloner mode.");
        ui.add_space(6.0);
        ui.label(RichText::new("macOS").strong());
        ui.label("Nothing to install.");
        ui.add_space(14.0);
        ui.heading("About");
        ui.add_space(4.0);
        ui.label(format!("k1cam-flasher {}, GPL-2.0-only.", env!("CARGO_PKG_VERSION")));
        ui.hyperlink_to("github.com/itzexor/k1cam-flasher", "https://github.com/itzexor/k1cam-flasher");
    }

    fn log_panel(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        if let Some(job) = &self.job {
            let stopping = self.cancel.load(Ordering::Relaxed);
            ui.horizontal(|ui| {
                let mut text = format!("{}  ·  {}", job.op.title(), clock(job.started.elapsed()));
                if job.progress > 0.0 {
                    text += &format!("  ·  {:.0}%", job.progress * 100.0);
                }
                let step = job.step.elapsed();
                if job.progress >= 0.03 && job.progress < 1.0 && step > Duration::from_secs(3) {
                    let left = step.mul_f32((1.0 - job.progress) / job.progress);
                    text += &format!("  ·  about {} left", clock(left));
                }
                if stopping {
                    text += "  ·  stopping";
                }
                let bar = egui::ProgressBar::new(job.progress).text(text).desired_width(ui.available_width() - 90.0);
                ui.add(if job.progress == 0.0 { bar.animate(true) } else { bar });
                if ui.add_enabled(!stopping, egui::Button::new("Cancel")).clicked() {
                    self.cancel.store(true, Ordering::Relaxed);
                }
            });
            ui.add_space(4.0);
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new("Log").strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(!self.busy(), egui::Button::new("Hide").small()).clicked() {
                    self.log_open = false;
                }
                if ui.add_enabled(!self.log.is_empty(), egui::Button::new("Clear").small()).clicked() {
                    self.log.clear();
                }
                if ui.add_enabled(!self.log.is_empty(), egui::Button::new("Copy").small()).clicked() {
                    let text: String = self
                        .log
                        .iter()
                        .map(|l| match l.level {
                            Level::Start => format!("--- {} ---\n", l.text),
                            Level::Ok => format!("OK: {}\n", l.text),
                            Level::Failed => format!("FAILED: {}\n", l.text),
                            Level::Info => format!("{}\n", l.text),
                        })
                        .collect();
                    ui.ctx().copy_text(text);
                }
            });
        });
        egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
            if self.log.is_empty() {
                ui.label(RichText::new("Operations and their results appear here.").color(weak_color(ui)));
            }
            for l in &self.log {
                let t = RichText::new(&l.text).monospace();
                match l.level {
                    Level::Info => ui.label(t),
                    Level::Start => {
                        ui.add_space(2.0);
                        ui.label(t.strong())
                    }
                    Level::Ok => ui.label(t.color(ok_color(ui))),
                    Level::Failed => ui.label(t.color(err_color(ui))),
                };
            }
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
        if ctx.input(|i| i.viewport().close_requested()) && self.busy() && !self.quit_after {
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
            ui.label(op.risk());
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Keep it running").clicked() {
                    self.quit_prompt = false;
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

/// The two slots and what each is to the camera.
fn slots(ui: &mut egui::Ui, info: &str) {
    let get = |k| xu::record_value(info, k).unwrap_or("");
    ui.horizontal(|ui| {
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
    });
}

/// The camera next to the bundle, with what would stop the camera taking it
/// marked (xu::install_slot has the final word).
fn compare(ui: &mut egui::Ui, info: Option<&str>, b: &bundle::Bundle) {
    let (ok, bad, weak) = (ok_color(ui), err_color(ui), weak_color(ui));
    let cam = |k: &str| info.and_then(|i| xu::record_value(i, k)).unwrap_or("");
    let mono = |s: &str| if s.is_empty() { RichText::new("—").color(weak) } else { RichText::new(s).monospace() };
    let mark = |t: RichText, good: bool| if info.is_none() { t } else { t.color(if good { ok } else { bad }) };
    egui::Grid::new("compare").num_columns(3).spacing([16.0, 4.0]).striped(true).show(ui, |ui| {
        let row = |ui: &mut egui::Ui, label: &str, cam: RichText, bundle: RichText| {
            ui.label(label);
            if info.is_some() {
                ui.label(cam);
            }
            ui.label(bundle);
            ui.end_row();
        };
        row(ui, "", RichText::new("Camera").color(weak), RichText::new("This bundle").color(weak));
        let build = mono(&b.build);
        row(ui, "Build", mono(cam("build")), if cam("build") == b.build { build.color(weak) } else { build });
        if let Some(c) = b.release_value("COMMIT_ID") {
            row(ui, "Commit", mono(""), mono(c));
        }
        let kernel: String = b.release_value("KERNEL_COMMIT").unwrap_or("").chars().take(12).collect();
        row(ui, "Kernel", mono(cam("kernel")), mono(&kernel));
        if let Some(v) = b.release_value("BUILDROOT_VERSION") {
            row(ui, "Buildroot", mono(""), mono(v));
        }
        row(ui, "Board", mono(cam("board")), mark(mono(&b.board), cam("board") == b.board));
        row(ui, "Layout", mono(cam("layout")), mark(mono(&b.layout), cam("layout") == b.layout));
        let compat = cam("boot_compat").parse::<u32>().unwrap_or(0);
        row(
            ui,
            "Boot compat",
            RichText::new(cam("boot_compat")),
            mark(RichText::new(format!("needs {}", b.min_boot)), b.min_boot <= compat),
        );
        for (label, len, key) in [("Kernel", b.kernel_len as u64, "kernel_capacity"), ("Rootfs", b.rootfs_len as u64, "rootfs_capacity")] {
            let cap = cam(key).parse::<u64>().unwrap_or(0);
            let (slot, size) = match cap {
                0 => (RichText::new("—").color(weak), RichText::new(bytes(len))),
                _ => (
                    RichText::new(format!("{} slot", bytes(cap))),
                    mark(RichText::new(format!("{} ({}%)", bytes(len), len * 100 / cap)), len <= cap),
                ),
            };
            row(ui, &format!("{label} size"), slot, size);
        }
    });
}

impl eframe::App for App {
    /// What goes on whether or not the window shows: jobs, devices, the
    /// console and the preview.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain();
        if !self.busy() && self.last_poll.elapsed() > Duration::from_secs(1) {
            self.cloner_up = cloner::device_present();
            self.camera_up = xu::Camera::present();
            if !self.camera_up {
                self.cam_info = None;
                self.info_rx = None;
            }
            if !self.tab_chosen {
                if self.camera_up {
                    self.tab = Tab::Update;
                }
                self.tab_chosen = true;
            }
            if let Some(path) = self.bundle.as_ref().filter(|l| l.changed()).map(|l| l.path.clone()) {
                self.note(Level::Info, format!("{} changed on disk, reloaded", path.display()));
                self.load_bundle(path);
            }
            self.last_poll = Instant::now();
        }
        self.poll_camera_info(camera_info);
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
        ctx.request_repaint_after(Duration::from_millis(250));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.take_dropped(&ctx);
        self.guard_close(&ctx);

        egui::Panel::top("tabs").show(ui, |ui| {
            if self.busy() {
                ui.disable();
            }
            ui.add_space(6.0);
            let (cloner, camera, console) = (self.cloner_up, self.camera_up, self.console.connected());
            let tab = self.tab;
            // The log's button counts the lines not seen yet, in red if one failed.
            let unseen = &self.log[self.log_seen.min(self.log.len())..];
            let mut log = RichText::new(if unseen.is_empty() { "Log".to_string() } else { format!("Log ({})", unseen.len()) });
            if unseen.iter().any(|l| l.level == Level::Failed) {
                log = log.color(err_color(ui));
            }
            let log_open = self.log_open;
            let (left, (right, toggle_log)) = egui::Sides::new().show(
                ui,
                |ui| {
                    let mut pick = None;
                    let mut item = |ui: &mut egui::Ui, t: Tab, label: &str, up: bool, tip: &str| {
                        let dot = egui::IdSalt::new(("tab-dot", label));
                        let mut atoms = egui::Atoms::new(label);
                        atoms.push_left(egui::Atom::custom(dot, egui::vec2(8.0, 8.0)));
                        let r = egui::Button::selectable(tab == t, atoms).atom_ui(ui);
                        if let Some(rect) = r.rect(dot) {
                            let color = if up { ok_color(ui) } else { weak_color(ui) };
                            ui.painter().circle_filled(rect.center(), 3.5, color);
                        }
                        if r.response.on_hover_text(tip).clicked() {
                            pick = Some(t);
                        }
                    };
                    item(
                        ui,
                        Tab::Cloner,
                        "Backup & Flash",
                        cloner,
                        if cloner { "A camera in cloner mode is attached" } else { "No camera in cloner mode" },
                    );
                    item(
                        ui,
                        Tab::Update,
                        "A/B Update",
                        camera,
                        if camera { "A camera running k1cam is attached" } else { "No camera running k1cam" },
                    );
                    item(ui, Tab::Tuning, "Tuning", camera, "The live picture and the camera's image controls");
                    // Port discovery makes the tab available without opening the port.
                    if self.console.seen() {
                        item(
                            ui,
                            Tab::Console,
                            "Console",
                            console,
                            if console { "Connected to the camera's console" } else { "Not connected" },
                        );
                    }
                    pick
                },
                |ui| {
                    let help = egui::Button::selectable(tab == Tab::Help, RichText::new(" ? ").strong());
                    let help = ui.add(help).on_hover_text("Help: cloner mode, drivers, about").clicked().then_some(Tab::Help);
                    let log = ui.add(egui::Button::selectable(log_open, log)).on_hover_text("Operations and their results");
                    (help, log.clicked())
                },
            );
            if let Some(t) = left.or(right) {
                self.tab = t;
            }
            if toggle_log {
                self.log_open = !self.log_open;
            }
            ui.add_space(4.0);
        });
        if self.log_open || self.busy() {
            egui::Panel::bottom("log").resizable(true).default_size(120.0).min_size(70.0).show(ui, |ui| self.log_panel(ui));
            self.log_seen = self.log.len();
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

/// The GUI; `args` may name a tab (--tab=update, --tab=tuning, --tab=help, …)
/// or a .upd to open.
pub fn run(args: &[String]) -> eframe::Result {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([960.0, 680.0])
            .with_min_inner_size([640.0, 480.0])
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
            for arg in &args {
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
