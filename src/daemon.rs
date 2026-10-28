// SPDX-License-Identifier: MIT
//! `k1cam-flasher daemon`: the camera attached to this Linux computer, for a
//! GUI elsewhere on the network. Plain HTTP with a password it makes up and
//! prints (a new one after `MAX_WRONG` wrong attempts), for a network you trust. One GUI controls it at a time, through a session it renews; while
//! one does, or a job runs, the daemon holds the lock other flashers here
//! respect (lock.rs). Firmware jobs keep running when the GUI goes away, and
//! the backups they make wait on disk until a GUI has saved them; uploads stay
//! in memory. Snapshots are taken on request, never stored. It runs in a
//! terminal while it is needed: Ctrl-C waits for a running job (see `watch_stops`).
//!
//! POST session needs the password, and every other route but /api/hello the
//! session it gives: a new password (after wrong ones) refuses new GUIs only.
//!
//! ```text
//! GET    /api/hello                       version, without the password
//! POST   /api/v1/session                  take control with the password; 409 while another GUI has it
//! DELETE /api/v1/session                  let go
//! GET    /api/v1/status                   what is attached, the running job
//! GET    /api/v1/info                     the camera's firmware record
//! GET    /api/v1/tuning/sizes             MJPEG frame sizes
//! GET    /api/v1/tuning/controls          every control
//! GET    /api/v1/tuning/get?i=1,2         some controls' values
//! POST   /api/v1/tuning/set?i=&value=     set one; answers what applies
//! POST   /api/v1/tuning/reset             factory defaults
//! POST   /api/v1/tuning/close             stop capturing, close the controls
//! GET    /api/v1/snapshot.jpg?width=&height=&auto=1&after=SEQ
//! POST   /api/v1/snapshot/stop            stop automatic capture
//! POST   /api/v1/console/open             open the camera's console
//! GET    /api/v1/console?after=N&wait=MS  its output from byte N
//! POST   /api/v1/console/input            bytes for it
//! POST   /api/v1/console/close
//! GET    /api/v1/jobs                     jobs not yet dismissed
//! POST   /api/v1/jobs?id=&op=&…           start one; the body is its upload
//! GET    /api/v1/jobs/ID?after=N&wait=MS  its log from line N, progress, outcome
//! POST   /api/v1/jobs/ID/cancel
//! GET    /api/v1/jobs/ID/output           the backup it made
//! DELETE /api/v1/jobs/ID                  dismiss it, deleting its backup
//! ```

use crate::api::{self, Hello, InUse, NewSession, Session, Status};
use crate::controls::Tuner;
use crate::http::{self, Request, Response};
use crate::job::{Event, Job};
use crate::lock::{self, Lock};
use crate::{bundle, cloner, controls, ops, serial, v4l2, xu};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type R<T> = Result<T, String>;
/// A response, or the status and message of a refusal.
type Reply = Result<Response, (u16, String)>;

const USAGE: &str = "usage: k1cam-flasher daemon\n";
/// Passwords are this long, from these letters and digits (no 0/O or 1/I).
const PASSWORD_LEN: usize = 6;
const PASSWORD_CHARS: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
/// Different wrong attempts before the password is replaced.
const MAX_WRONG: u32 = 5;
/// Replaced passwords remembered, so that GUIs still sending them use up no attempts.
const MAX_RETIRED: usize = 16;
const MAX_CONNECTIONS: usize = 16;
/// Backups kept for GUIs that haven't saved them; at this many, jobs that
/// would make another are refused rather than an old one deleted.
const MAX_OUTPUTS: usize = 4;
/// The rate snapshots are captured at; the GUI asks for at most two a second.
const CAPTURE_FPS: u32 = 5;
const MAX_WAIT: Duration = Duration::from_secs(10);
const CONSOLE_KEEP: usize = 64 * 1024;

/// What a job does.
pub enum Task {
    Detect,
    Backup,
    Flash { data: Vec<u8>, backup: bool, verify: bool, reboot: bool },
    Dump { live: bool },
    Update { data: Vec<u8>, reboot: bool },
    Reboot,
    Cancel,
}

impl Task {
    /// Whether it leaves a backup behind.
    fn backs_up(&self) -> bool {
        matches!(self, Task::Backup | Task::Dump { .. } | Task::Flash { backup: true, .. })
    }
}

/// MJPEG frames from the camera's video.
pub trait Frames: Send {
    /// The next frame, or None if none came within `wait`.
    fn frame(&mut self, wait: Duration) -> R<Option<Vec<u8>>>;
    fn size(&self) -> (u32, u32);
}

/// The camera's console port.
pub trait Port: Send {
    /// What arrives within `wait`; Some(0) when the port went away.
    fn read(&mut self, buf: &mut [u8], wait: Duration) -> std::io::Result<Option<usize>>;
    fn writer(&self) -> std::io::Result<Box<dyn Write + Send>>;
}

/// The camera and its devices; the tests pretend.
pub trait Devices: Send + Sync {
    fn status(&self) -> Status;
    fn info(&self) -> R<String>;
    fn tuner(&self) -> R<Box<dyn Tuner>>;
    fn frames(&self, width: u32, height: u32, fps: u32) -> R<Box<dyn Frames>>;
    fn console(&self) -> R<Box<dyn Port>>;
    /// Runs a task; a backup it makes goes to `out`.
    fn run(&self, j: &Job, task: Task, out: &Path) -> R<String>;
}

/// The devices attached here.
struct Here;

impl Frames for v4l2::Stream {
    fn frame(&mut self, wait: Duration) -> R<Option<Vec<u8>>> {
        v4l2::Stream::frame(self, wait)
    }

    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

impl Port for serial::Port {
    fn read(&mut self, buf: &mut [u8], wait: Duration) -> std::io::Result<Option<usize>> {
        serial::Port::read(self, buf, wait)
    }

    fn writer(&self) -> std::io::Result<Box<dyn Write + Send>> {
        Ok(Box::new(serial::Port::writer(self)?))
    }
}

fn video_node() -> R<PathBuf> {
    v4l2::node().ok_or_else(|| "the camera's video device was not found".into())
}

impl Devices for Here {
    fn status(&self) -> Status {
        Status {
            cloner: cloner::device_present(),
            camera: xu::Camera::present(),
            video: v4l2::node().is_some(),
            console: serial::find(&xu::CAMERA_IDS).is_some(),
            job: None,
        }
    }

    fn info(&self) -> R<String> {
        xu::Camera::open()?.info()
    }

    fn tuner(&self) -> R<Box<dyn Tuner>> {
        Ok(Box::new(controls::Local::open(&video_node()?)?))
    }

    fn frames(&self, width: u32, height: u32, fps: u32) -> R<Box<dyn Frames>> {
        Ok(Box::new(v4l2::Stream::start(&video_node()?, width, height, fps)?))
    }

    fn console(&self) -> R<Box<dyn Port>> {
        let path = serial::find(&xu::CAMERA_IDS).ok_or("no camera console was found")?;
        let users = crate::linux::device_users(&path.to_string_lossy());
        if !users.is_empty() {
            return Err(format!("{} is open in {}; not opening it, so that it keeps its output", path.display(), users.join(", ")));
        }
        Ok(Box::new(serial::Port::open(&path)?))
    }

    fn run(&self, j: &Job, task: Task, out: &Path) -> R<String> {
        // What a backup's message says about its file is said by the GUI that saves it.
        let mine = |r: R<String>| r.map(|m| m.strip_prefix(&*out.to_string_lossy()).unwrap_or(&m).trim_start().to_string());
        match task {
            Task::Detect => {
                let c = cloner::Cloner::probe(j)?;
                Ok(format!("{} ({} KiB, JEDEC {:06x})", c.name, c.size / 1024, c.jedec))
            }
            Task::Backup => mine(ops::backup(j, out)),
            Task::Flash { data, backup, verify, reboot } => ops::flash_data(j, &data, backup.then_some(out), verify, reboot),
            Task::Dump { live } => mine(ops::dump(j, out, live)),
            Task::Update { data, reboot } => ops::update(j, &bundle::parse(data)?, reboot),
            Task::Reboot => ops::reboot_trial(j),
            Task::Cancel => ops::cancel_trial(j),
        }
    }
}

struct Sess {
    id: String,
    client: String,
    peer: String,
    seen: Instant,
}

/// The newest frame of an automatic capture.
#[derive(Default)]
struct Shot {
    seq: u64,
    jpeg: Arc<Vec<u8>>,
    at: Option<Instant>,
    size: (u32, u32),
    error: Option<String>,
    ended: bool,
}

type Shared = Arc<(Mutex<Shot>, Condvar)>;

/// The controls, opened once for the session and shared by its requests.
type SharedTuner = Arc<Mutex<Box<dyn Tuner>>>;

/// Automatic capture: a stream kept running while the GUI keeps asking.
struct Capture {
    size: (u32, u32),
    stop: Arc<AtomicBool>,
    shot: Shared,
    thread: Option<JoinHandle<()>>,
    asked: Instant,
}

impl Capture {
    fn end(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The open console: its output so far (the last CONSOLE_KEEP bytes) and its writer.
struct Con {
    gen: u64,
    start: u64,
    data: VecDeque<u8>,
    open: bool,
    writer: Box<dyn Write + Send>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Con {
    fn end(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Rec {
    job: api::Job,
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
struct State {
    session: Option<Sess>,
    lock: Option<Lock>,
    tuner: Option<SharedTuner>,
    capture: Option<Capture>,
    console: Option<Con>,
    console_gen: u64,
    jobs: Vec<Rec>,
    running: Option<String>,
    seq: u64,
}

/// The password, and what has been tried against it.
struct Auth {
    password: String,
    /// The different wrong ones tried since it was made.
    wrong: Vec<String>,
    /// Replaced passwords: refused, but not counted, so that a GUI still
    /// sending one doesn't keep the password changing.
    retired: Vec<String>,
}

pub struct Daemon {
    dev: Arc<dyn Devices>,
    auth: Mutex<Auth>,
    /// Ctrl-C: no new jobs.
    stopping: AtomicBool,
    jobs_dir: PathBuf,
    lock_name: String,
    host: String,
    lease: Duration,
    idle: Duration,
    /// How long a wrong password waits for its answer.
    slow: Duration,
    st: Mutex<State>,
    cv: Condvar,
    /// Serializes camera requests and the transition into firmware work.
    /// Status, lease renewals and job polling remain available during I/O.
    camera: Mutex<()>,
    /// One snapshot capture at a time.
    snapping: Mutex<()>,
}

fn bad(e: String) -> (u16, String) {
    (400, e)
}

fn failed(e: String) -> (u16, String) {
    (500, e)
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn random_bytes(n: usize) -> R<Vec<u8>> {
    let mut b = vec![0u8; n];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).map_err(|e| format!("/dev/urandom: {e}"))?;
    Ok(b)
}

fn random_hex(n: usize) -> R<String> {
    Ok(xu::hex(&random_bytes(n)?))
}

fn new_password() -> R<String> {
    Ok(random_bytes(PASSWORD_LEN)?.iter().map(|b| PASSWORD_CHARS[(b & 31) as usize] as char).collect())
}

fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// A camera frame worth showing: a whole JPEG (UVC may pad after its end).
fn whole_jpeg(j: &[u8]) -> bool {
    j.len() > 4 && j[..2] == [0xff, 0xd8] && j[j.len().saturating_sub(1024)..].windows(2).any(|w| w == [0xff, 0xd9])
}

fn hello() -> Hello {
    Hello { api: api::API, version: env!("CARGO_PKG_VERSION").into(), os: std::env::consts::OS.into(), arch: std::env::consts::ARCH.into() }
}

/// Constant time, so that the answer's timing says nothing about the password.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn jpeg_reply(jpeg: &[u8], seq: u64, age: Duration, (w, h): (u32, u32)) -> Response {
    Response::new(200, "image/jpeg", jpeg.to_vec())
        .header("Cache-Control", "no-store")
        .header(api::SEQ, seq.to_string())
        .header(api::AGE, age.as_millis().to_string())
        .header(api::WIDTH, w.to_string())
        .header(api::HEIGHT, h.to_string())
}

impl Daemon {
    pub fn new(dev: Arc<dyn Devices>, password: String, state_dir: &Path, lock_name: &str) -> R<Self> {
        let jobs_dir = state_dir.join("jobs");
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&jobs_dir).map_err(|e| format!("{}: {e}", jobs_dir.display()))?;
        let host = std::fs::read_to_string("/proc/sys/kernel/hostname").map(|h| h.trim().to_string()).unwrap_or_default();
        let d = Self {
            dev,
            auth: Mutex::new(Auth { password, wrong: Vec::new(), retired: Vec::new() }),
            stopping: AtomicBool::new(false),
            jobs_dir,
            lock_name: lock_name.into(),
            host,
            lease: Duration::from_millis(api::LEASE_MS),
            idle: Duration::from_secs(3),
            slow: Duration::from_secs(1),
            st: Mutex::new(State::default()),
            cv: Condvar::new(),
            camera: Mutex::new(()),
            snapping: Mutex::new(()),
        };
        d.load_jobs()?;
        Ok(d)
    }

    fn st(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap()
    }

    fn who(&self, st: &State) -> String {
        let mut who = format!("k1cam-flasher daemon (pid {})", std::process::id());
        if let Some(s) = &st.session {
            who += &format!(", controlled from {} ({})", s.client, s.peer);
        } else if let Some(op) = st.running.as_ref().and_then(|id| st.jobs.iter().find(|r| &r.job.id == id)).map(|r| r.job.op.clone()) {
            who += &format!(", running {op}");
        }
        who
    }

    /// Holds the lock while a session or a job needs it, and only then.
    fn hold(&self, st: &mut State) -> Result<(), (u16, String)> {
        if st.session.is_none() && st.running.is_none() {
            st.lock = None;
            return Ok(());
        }
        let who = self.who(st);
        match &st.lock {
            Some(l) => l.set_who(who),
            None => st.lock = Some(lock::acquire_named(&self.lock_name, who).map_err(|e| (409, e))?),
        }
        Ok(())
    }

    // ---- jobs on disk ----

    fn record(&self, id: &str) -> PathBuf {
        self.jobs_dir.join(format!("{id}.json"))
    }

    fn output(&self, id: &str) -> PathBuf {
        self.jobs_dir.join(format!("{id}.bin"))
    }

    fn save(&self, job: &api::Job) -> R<()> {
        let save = || -> Result<(), Box<dyn std::error::Error>> {
            let text = serde_json::to_vec(job)?;
            let tmp = self.jobs_dir.join(format!("{}.tmp", job.id));
            let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
            f.write_all(&text)?;
            f.sync_all()?;
            std::fs::rename(&tmp, self.record(&job.id))?;
            std::fs::File::open(&self.jobs_dir)?.sync_all()?;
            Ok(())
        };
        save().map_err(|e| format!("saving job {} in {}: {e}", job.id, self.jobs_dir.display()))
    }

    /// Jobs from before a restart: their backups wait still, and one that was
    /// running was cut off.
    fn load_jobs(&self) -> R<()> {
        let mut jobs = Vec::new();
        let read = || -> Result<_, Box<dyn std::error::Error>> {
            Ok(std::fs::read_dir(&self.jobs_dir)?.collect::<Result<Vec<_>, _>>()?)
        };
        for e in read().map_err(|e| format!("reading {}: {e}", self.jobs_dir.display()))? {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "json") {
                let data = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
                let mut job: api::Job = serde_json::from_slice(&data).map_err(|e| format!("{}: {e}", p.display()))?;
                if !valid_id(&job.id) || p != self.record(&job.id) {
                    return Err(format!("{}: invalid job id", p.display()));
                }
                if job.done.is_none() {
                    job.done = Some(Err("interrupted: the daemon stopped while it ran; check the camera before trusting it".into()));
                    job.output = self.describe_output(&job.id);
                    self.save(&job)?;
                }
                jobs.push(Rec { job, cancel: Arc::new(AtomicBool::new(false)) });
            }
        }
        jobs.sort_by_key(|r| r.job.started);
        self.st().jobs = jobs;
        Ok(())
    }

    fn describe_output(&self, id: &str) -> Option<api::Output> {
        let data = std::fs::read(self.output(id)).ok()?;
        Some(api::Output { size: data.len() as u64, sha256: xu::hex(&Sha256::digest(&data)) })
    }

    // ---- requests ----

    /// The password GUIs connect with now.
    pub fn password(&self) -> String {
        self.auth.lock().unwrap().password.clone()
    }

    /// Checks the password; the `MAX_WRONG`th different wrong one replaces it.
    /// One sent again (a GUI still using it) counts once.
    fn authorized(&self, req: &Request, peer: &str) -> bool {
        let given = req.header("Authorization").and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("").trim().to_ascii_uppercase();
        let mut a = self.auth.lock().unwrap();
        if same(given.as_bytes(), a.password.as_bytes()) {
            return true;
        }
        if a.retired.iter().chain(&a.wrong).any(|old| same(given.as_bytes(), old.as_bytes())) {
            return false;
        }
        a.wrong.push(given);
        if a.wrong.len() >= MAX_WRONG as usize {
            match new_password() {
                Ok(p) => {
                    let old = std::mem::replace(&mut a.password, p);
                    if a.retired.len() >= MAX_RETIRED {
                        a.retired.remove(0);
                    }
                    a.retired.push(old);
                    a.wrong.clear();
                    eprintln!(
                        "k1cam-flasher daemon: {MAX_WRONG} wrong codes, the last from {peer}; the code is now {}",
                        a.password
                    );
                }
                Err(e) => eprintln!("k1cam-flasher daemon: {MAX_WRONG} wrong codes, but no new one: {e}"),
            }
        }
        false
    }

    pub fn handle(self: &Arc<Self>, req: &Request, peer: &str) -> Response {
        if req.path == "/api/hello" {
            return Response::json(200, &hello());
        }
        let Some(route) = req.path.strip_prefix(&format!("/api/v{}/", api::API)) else {
            return Response::error(404, format!("no such route: {}", req.path));
        };
        // The password takes a session; the session is what the rest need, so
        // that a new password leaves the GUI that has the camera connected.
        let take = (req.method.as_str(), route) == ("POST", "session");
        if take && !self.authorized(req, peer) {
            // Slows guessing down.
            std::thread::sleep(self.slow);
            return Response::error(401, "wrong password");
        }
        let camera = route == "info" || route == "session" || route.starts_with("tuning/")
            || route.starts_with("snapshot") || route.starts_with("console/")
            || (req.method == "POST" && route == "jobs");
        let _camera = camera.then(|| self.camera.lock().unwrap());
        let r = if take { self.new_session(req, peer) } else { self.renew(req).and_then(|_| self.route(req, route)) };
        r.unwrap_or_else(|(status, e)| Response::error(status, e))
    }

    fn route(self: &Arc<Self>, req: &Request, route: &str) -> Reply {
        let done = || Ok(Response::json(200, &serde_json::json!({})));
        match (req.method.as_str(), route) {
            ("DELETE", "session") => {
                self.end_session();
                done()
            }
            ("GET", "status") => Ok(Response::json(200, &self.status())),
            ("GET", "info") => {
                self.idle()?;
                let info = self.dev.info().map_err(failed)?;
                Ok(Response::new(200, "text/plain; charset=utf-8", info.into_bytes()))
            }
            ("GET", "tuning/diagnostic") => {
                let record = self.tuner()?.lock().unwrap().diagnostic().map_err(failed)?;
                Ok(match record {
                    Some(r) => Response::new(200, "application/octet-stream", r),
                    None => Response::new(204, "application/octet-stream", Vec::new()),
                })
            }
            ("GET", "tuning/sizes") => Ok(Response::json(200, &self.tuner()?.lock().unwrap().sizes().map_err(failed)?)),
            ("GET", "tuning/controls") => Ok(Response::json(200, &self.tuner()?.lock().unwrap().load().map_err(failed)?)),
            ("GET", "tuning/get") => {
                let which: Vec<usize> = req.param("i").unwrap_or("").split(',').filter_map(|s| s.parse().ok()).collect();
                Ok(Response::json(200, &self.tuner()?.lock().unwrap().get(&which)))
            }
            ("POST", "tuning/set") => {
                let (i, v) = (req.num("i", usize::MAX).map_err(bad)?, req.num("value", 0i32).map_err(bad)?);
                Ok(Response::json(200, &self.tuner()?.lock().unwrap().set(i, v).map_err(failed)?))
            }
            ("POST", "tuning/reset") => {
                self.tuner()?.lock().unwrap().reset().map_err(failed)?;
                done()
            }
            ("POST", "tuning/close") => {
                self.stop_capture();
                self.st().tuner = None;
                done()
            }
            ("GET", "snapshot.jpg") => self.snapshot(req),
            ("POST", "snapshot/stop") => {
                self.stop_capture();
                done()
            }
            ("POST", "console/open") => {
                self.console_open()?;
                done()
            }
            ("GET", "console") => self.console_read(req),
            ("POST", "console/input") => {
                let mut st = self.st();
                let c = st.console.as_mut().filter(|c| c.open).ok_or((409, "the console is not open".to_string()))?;
                c.writer.write_all(&req.body).map_err(|e| failed(e.to_string()))?;
                done()
            }
            ("POST", "console/close") => {
                let c = self.st().console.take();
                if let Some(c) = c {
                    c.end();
                }
                done()
            }
            ("GET", "jobs") => {
                Ok(Response::json(200, &self.st().jobs.iter().map(|r| api::Job { log: Vec::new(), ..r.job.clone() }).collect::<Vec<_>>()))
            }
            ("POST", "jobs") => self.start_job(req),
            (m, r) => match r.strip_prefix("jobs/") {
                Some(rest) => self.job_route(m, rest, req),
                None => Err((404, format!("no such route: {m} {}", req.path))),
            },
        }
    }

    // ---- sessions ----

    fn new_session(&self, req: &Request, peer: &str) -> Reply {
        let wanted: NewSession = serde_json::from_slice(&req.body).map_err(|e| bad(e.to_string()))?;
        let client: String = wanted.client.chars().take(64).collect();
        let mut st = self.st();
        if let Some(s) = st.session.as_ref().filter(|s| s.seen.elapsed() <= self.lease) {
            if !wanted.resume.as_ref().is_some_and(|id| same(id.as_bytes(), s.id.as_bytes())) {
                let holder = format!("{} ({})", s.client, s.peer);
                let age = s.seen.elapsed();
                let error = format!(
                    "in use by another flasher, {holder}, last heard from {} s ago; it lets go {} s after that flasher stops answering",
                    age.as_secs(),
                    self.lease.as_secs()
                );
                return Ok(Response::json(409, &InUse { error, holder, age_ms: age.as_millis() as u64 }));
            }
            let id = s.id.clone();
            let s = st.session.as_mut().unwrap();
            (s.client, s.peer, s.seen) = (client, peer.into(), Instant::now());
            self.hold(&mut st)?;
            return Ok(Response::json(200, &Session { id, host: self.host.clone() }));
        }
        if st.session.is_some() {
            drop(st);
            self.end_session();
            st = self.st();
        }
        let old = st.session.take();
        let id = random_hex(16).map_err(failed)?;
        st.session = Some(Sess { id: id.clone(), client, peer: peer.into(), seen: Instant::now() });
        if let Err(e) = self.hold(&mut st) {
            st.session = old;
            return Ok(Response::json(409, &InUse { error: e.1.clone(), holder: e.1, age_ms: 0 }));
        }
        Ok(Response::json(200, &Session { id, host: self.host.clone() }))
    }

    fn renew(&self, req: &Request) -> Result<(), (u16, String)> {
        let id = req.header(api::SESSION).unwrap_or("");
        let mut st = self.st();
        match st.session.as_mut().filter(|s| same(s.id.as_bytes(), id.as_bytes()) && s.seen.elapsed() <= self.lease) {
            Some(s) => {
                s.seen = Instant::now();
                Ok(())
            }
            None => Err((403, "no session: connect again".into())),
        }
    }

    /// Lets the camera go: capture, controls and console close; a running job carries on.
    fn end_session(&self) {
        let (cap, con) = {
            let mut st = self.st();
            st.session = None;
            st.tuner = None;
            (st.capture.take(), st.console.take())
        };
        if let Some(c) = cap {
            c.end();
        }
        if let Some(c) = con {
            c.end();
        }
        let _ = self.hold(&mut self.st());
        self.cv.notify_all();
    }

    /// Ends a session that stopped renewing, and a capture nobody asks of.
    pub fn tick(&self) {
        let _camera = self.camera.lock().unwrap();
        let (expired, idle) = {
            let st = self.st();
            (
                st.session.as_ref().is_some_and(|s| s.seen.elapsed() > self.lease),
                st.capture.as_ref().is_some_and(|c| c.asked.elapsed() > self.idle),
            )
        };
        if expired {
            self.end_session();
        } else if idle {
            self.stop_capture();
        }
    }

    fn status(&self) -> Status {
        let job = self.st().running.clone();
        Status { job, ..self.dev.status() }
    }

    /// Refuses while a job has the camera.
    fn idle(&self) -> Result<(), (u16, String)> {
        let st = self.st();
        match st.running.as_ref().and_then(|id| st.jobs.iter().find(|r| &r.job.id == id)) {
            Some(r) => Err((409, format!("busy: {} is running", r.job.op))),
            None => Ok(()),
        }
    }

    // ---- tuning and snapshots ----

    fn tuner(&self) -> Result<SharedTuner, (u16, String)> {
        self.idle()?;
        if let Some(t) = self.st().tuner.clone() {
            return Ok(t);
        }
        let t = Arc::new(Mutex::new(self.dev.tuner().map_err(failed)?));
        Ok(self.st().tuner.get_or_insert(t).clone())
    }

    fn stop_capture(&self) {
        let c = self.st().capture.take();
        if let Some(c) = c {
            c.end();
        }
    }

    fn next_seq(&self) -> u64 {
        let mut st = self.st();
        st.seq += 1;
        st.seq
    }

    fn snapshot(self: &Arc<Self>, req: &Request) -> Reply {
        self.idle()?;
        let size = (req.num("width", 1280u32).map_err(bad)?, req.num("height", 720u32).map_err(bad)?);
        let after: u64 = req.num("after", 0).map_err(bad)?;
        let _one = self.snapping.lock().unwrap();
        if req.flag("auto") {
            let shot = self.capture(size);
            return self.next_shot(&shot, after).map(|r| self.snapshot_diagnostic(req, r));
        }
        // A capture already running gives its next frame.
        let running = self.st().capture.as_ref().filter(|c| c.size == size).map(|c| c.shot.clone());
        if let Some(shot) = running {
            let seq = shot.0.lock().unwrap().seq;
            return self.next_shot(&shot, seq).map(|r| self.snapshot_diagnostic(req, r));
        }
        self.stop_capture();
        // Otherwise the video starts for this one picture and stops again. The
        // first frame after a start is passed over: it can predate the start.
        let mut f = self.dev.frames(size.0, size.1, CAPTURE_FPS).map_err(failed)?;
        let end = Instant::now() + Duration::from_secs(5);
        let mut passed = false;
        let jpeg = loop {
            match f.frame(Duration::from_millis(250)).map_err(failed)? {
                Some(j) if whole_jpeg(&j) && passed => break j,
                Some(j) if whole_jpeg(&j) => passed = true,
                _ if Instant::now() > end => return Err((504, "no picture came from the camera".into())),
                _ => {}
            }
        };
        let got = f.size();
        // Read before closing: the camera returns to its idle crop at STREAMOFF.
        let reply = self.snapshot_diagnostic(req, jpeg_reply(&jpeg, self.next_seq(), Duration::ZERO, got));
        drop(f);
        Ok(reply)
    }

    fn snapshot_diagnostic(&self, req: &Request, reply: Response) -> Response {
        if !req.flag("diagnostic") { return reply; }
        match self.tuner().and_then(|t| t.lock().unwrap().diagnostic().map_err(failed)) {
            Ok(Some(record)) => reply.header(api::DIAGNOSTIC, xu::hex(&record)),
            Ok(None) => reply,
            Err(_) => reply.header(api::DIAGNOSTIC, "error"),
        }
    }

    /// The automatic capture at `size`, started if it isn't running.
    fn capture(self: &Arc<Self>, size: (u32, u32)) -> Shared {
        let mut st = self.st();
        if let Some(c) = st.capture.as_mut().filter(|c| c.size == size && !c.shot.0.lock().unwrap().ended) {
            c.asked = Instant::now();
            return c.shot.clone();
        }
        let old = st.capture.take();
        drop(st);
        if let Some(c) = old {
            c.end();
        }
        let (stop, shot): (_, Shared) = (Arc::new(AtomicBool::new(false)), Arc::default());
        let (d, stop2, shot2) = (self.clone(), stop.clone(), shot.clone());
        let thread = std::thread::spawn(move || d.capturing(size, stop2, shot2));
        self.st().capture = Some(Capture { size, stop, shot: shot.clone(), thread: Some(thread), asked: Instant::now() });
        shot
    }

    fn capturing(&self, size: (u32, u32), stop: Arc<AtomicBool>, shot: Shared) {
        let (lock, cv) = &*shot;
        let end = |e: Option<String>| {
            let mut s = lock.lock().unwrap();
            s.error = e;
            s.ended = true;
            cv.notify_all();
        };
        let mut f = match self.dev.frames(size.0, size.1, CAPTURE_FPS) {
            Ok(f) => f,
            Err(e) => return end(Some(e)),
        };
        let mut passed = false;
        while !stop.load(Ordering::Relaxed) {
            match f.frame(Duration::from_millis(250)) {
                Ok(Some(j)) if whole_jpeg(&j) && passed => {
                    let seq = self.next_seq();
                    let mut s = lock.lock().unwrap();
                    (s.seq, s.jpeg, s.at, s.size) = (seq, Arc::new(j), Some(Instant::now()), f.size());
                    cv.notify_all();
                }
                Ok(Some(j)) if whole_jpeg(&j) => passed = true,
                Ok(_) => {}
                Err(e) => return end(Some(e)),
            }
        }
        end(None);
    }

    /// The capture's first frame after `after`.
    fn next_shot(&self, shot: &Shared, after: u64) -> Reply {
        let (lock, cv) = &**shot;
        let end = Instant::now() + Duration::from_secs(3);
        let mut s = lock.lock().unwrap();
        loop {
            if s.seq > after {
                return Ok(jpeg_reply(&s.jpeg, s.seq, s.at.map_or(Duration::ZERO, |t| t.elapsed()), s.size));
            }
            if s.ended {
                return Err((500, s.error.clone().unwrap_or_else(|| "capture stopped".into())));
            }
            let now = Instant::now();
            if now >= end {
                return Err((504, "no picture came from the camera".into()));
            }
            s = cv.wait_timeout(s, end - now).unwrap().0;
        }
    }

    // ---- console ----

    fn console_open(self: &Arc<Self>) -> Result<(), (u16, String)> {
        self.idle()?;
        if self.st().console.as_ref().is_some_and(|c| c.open) {
            return Ok(());
        }
        let old = self.st().console.take();
        if let Some(c) = old {
            c.end();
        }
        let mut port = self.dev.console().map_err(failed)?;
        let writer = port.writer().map_err(|e| failed(e.to_string()))?;
        let stop = Arc::new(AtomicBool::new(false));
        let mut st = self.st();
        st.console_gen += 1;
        let gen = st.console_gen;
        let (d, stop2) = (self.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while !stop2.load(Ordering::Relaxed) {
                let got = port.read(&mut buf, Duration::from_millis(200));
                let mut st = d.st();
                let Some(c) = st.console.as_mut().filter(|c| c.gen == gen) else { break };
                match got {
                    Ok(None) => continue,
                    Ok(Some(n)) if n > 0 => {
                        c.data.extend(&buf[..n]);
                        while c.data.len() > CONSOLE_KEEP {
                            c.data.pop_front();
                            c.start += 1;
                        }
                    }
                    _ => {
                        c.open = false;
                        drop(st);
                        d.cv.notify_all();
                        break;
                    }
                }
                drop(st);
                d.cv.notify_all();
            }
        });
        st.console = Some(Con { gen, start: 0, data: VecDeque::new(), open: true, writer, stop, thread: Some(thread) });
        Ok(())
    }

    fn console_read(&self, req: &Request) -> Reply {
        let after: u64 = req.num("after", 0).map_err(bad)?;
        let wait = Duration::from_millis(req.num("wait", 1000).map_err(bad)?).min(MAX_WAIT);
        let end = Instant::now() + wait;
        let mut st = self.st();
        loop {
            let c = st.console.as_ref().ok_or((409, "the console is not open".to_string()))?;
            let top = c.start + c.data.len() as u64;
            let now = Instant::now();
            if top > after || !c.open || now >= end {
                let from = after.clamp(c.start, top);
                let bytes: Vec<u8> = c.data.iter().skip((from - c.start) as usize).take(CONSOLE_KEEP).copied().collect();
                let next = from + bytes.len() as u64;
                return Ok(Response::new(200, "application/octet-stream", bytes)
                    .header(api::NEXT, next.to_string())
                    .header(api::OPEN, (c.open as u8).to_string()));
            }
            st = self.cv.wait_timeout(st, end - now).unwrap().0;
        }
    }

    // ---- jobs ----

    fn task(&self, op: &str, req: &Request) -> Result<Task, (u16, String)> {
        let data = || req.body.clone();
        Ok(match op {
            "detect" => Task::Detect,
            "backup" => Task::Backup,
            "flash" => {
                if req.body.len() != ops::FLASH_SIZE {
                    return Err(bad(format!("a whole-flash image is exactly 8 MiB (got {} bytes)", req.body.len())));
                }
                Task::Flash { data: data(), backup: req.flag("backup"), verify: req.flag("verify"), reboot: req.flag("reboot") }
            }
            "dump" => Task::Dump { live: req.flag("live") },
            "update" => {
                // Checked now, so that a damaged upload is refused before it is a job.
                bundle::parse(data()).map_err(|e| bad(format!("the bundle: {e}")))?;
                Task::Update { data: data(), reboot: req.flag("reboot") }
            }
            "reboot" => Task::Reboot,
            "cancel" => Task::Cancel,
            _ => return Err(bad(format!("no such operation: {op}"))),
        })
    }

    fn start_job(self: &Arc<Self>, req: &Request) -> Reply {
        let id = req.param("id").filter(|i| valid_id(i)).ok_or(bad("a job needs an id of letters, digits and dashes".into()))?.to_string();
        let op = req.param("op").unwrap_or("").to_string();
        // A request repeated after a lost answer gets the job it started.
        if let Some(r) = self.st().jobs.iter().find(|r| r.job.id == id) {
            return Ok(Response::json(200, &r.job));
        }
        let task = self.task(&op, req)?;
        let (cap, con) = {
            let mut st = self.st();
            if let Some(r) = st.jobs.iter().find(|r| r.job.id == id) {
                return Ok(Response::json(200, &r.job));
            }
            if let Some(r) = st.running.as_ref().and_then(|id| st.jobs.iter().find(|r| &r.job.id == id)) {
                return Err((409, format!("busy: {} is running", r.job.op)));
            }
            if self.stopping.load(Ordering::Relaxed) {
                return Err((503, format!("the daemon on {} is stopping", self.host)));
            }
            let kept = st.jobs.iter().filter(|r| r.job.output.is_some()).count();
            if task.backs_up() && kept >= MAX_OUTPUTS {
                return Err((409, format!("{} holds {kept} backups that no GUI has saved yet; save or discard them first", self.host)));
            }
            let job =
                api::Job { id: id.clone(), op, started: now_secs(), log: Vec::new(), next: 0, progress: 0.0, done: None, output: None };
            st.running = Some(id.clone());
            if let Err(e) = self.hold(&mut st) {
                st.running = None;
                return Err(e);
            }
            if let Err(e) = self.save(&job) {
                st.running = None;
                let _ = self.hold(&mut st);
                return Err(failed(e));
            }
            st.jobs.push(Rec { job: job.clone(), cancel: Arc::new(AtomicBool::new(false)) });
            // Live camera access stops before firmware work.
            st.tuner = None;
            (st.capture.take(), st.console.take())
        };
        let cancel = self.st().jobs.last().unwrap().cancel.clone();
        let d = self.clone();
        let id2 = id.clone();
        // The request gate also waits for manual snapshots, info and control
        // requests. Close the background devices before letting work begin.
        if let Some(c) = cap {
            c.end();
        }
        if let Some(c) = con {
            c.end();
        }
        std::thread::spawn(move || d.run_job(&id2, task, cancel));
        let st = self.st();
        Ok(Response::json(201, &st.jobs.iter().find(|r| r.job.id == id).unwrap().job))
    }

    fn run_job(self: &Arc<Self>, id: &str, task: Task, cancel: Arc<AtomicBool>) {
        let (tx, rx) = channel();
        let job = Job::new(tx, cancel);
        let d = self.clone();
        let id2 = id.to_string();
        let collect = std::thread::spawn(move || {
            for ev in rx {
                let mut st = d.st();
                if let Some(r) = st.jobs.iter_mut().find(|r| r.job.id == id2) {
                    match ev {
                        Event::Log(s) => r.job.log.push(s),
                        Event::Progress(p) => r.job.progress = p,
                        Event::Done(_) => {}
                    }
                    r.job.next = r.job.log.len();
                }
                drop(st);
                d.cv.notify_all();
            }
        });
        let out = self.output(id);
        let r = job.check_cancel().and_then(|_| self.dev.run(&job, task, &out));
        drop(job);
        let _ = collect.join();
        let output = self.describe_output(id);
        let mut st = self.st();
        if let Some(rec) = st.jobs.iter_mut().find(|r| r.job.id == id) {
            rec.job.done = Some(r);
            rec.job.output = output;
            if let Err(e) = self.save(&rec.job) {
                let outcome = match rec.job.done.as_ref().unwrap() {
                    Ok(m) => format!("operation completed: {m}"),
                    Err(e) => format!("operation failed: {e}"),
                };
                let outcome = format!("{outcome}; {e}; after a restart the job may be reported as interrupted");
                eprintln!("k1cam-flasher daemon: {outcome}");
                rec.job.done = Some(Err(outcome));
            }
        }
        st.running = None;
        let _ = self.hold(&mut st);
        drop(st);
        self.cv.notify_all();
    }

    fn job_route(&self, method: &str, rest: &str, req: &Request) -> Reply {
        let (id, what) = rest.split_once('/').unwrap_or((rest, ""));
        let missing = || (404, format!("no job {id}"));
        match (method, what) {
            ("GET", "") => {
                let after: usize = req.num("after", 0).map_err(bad)?;
                let wait = Duration::from_millis(req.num("wait", 1000).map_err(bad)?).min(MAX_WAIT);
                let end = Instant::now() + wait;
                let mut st = self.st();
                loop {
                    let r = st.jobs.iter().find(|r| r.job.id == id).ok_or_else(missing)?;
                    let now = Instant::now();
                    if r.job.log.len() > after || r.job.done.is_some() || now >= end {
                        let log = r.job.log.get(after..).unwrap_or(&[]).to_vec();
                        return Ok(Response::json(200, &api::Job { log, ..r.job.clone() }));
                    }
                    st = self.cv.wait_timeout(st, end - now).unwrap().0;
                }
            }
            ("POST", "cancel") => {
                self.st().jobs.iter().find(|r| r.job.id == id).ok_or_else(missing)?.cancel.store(true, Ordering::Relaxed);
                Ok(Response::json(200, &serde_json::json!({})))
            }
            ("GET", "output") => {
                let o = self.st().jobs.iter().find(|r| r.job.id == id).ok_or_else(missing)?.job.output.clone();
                let o = o.ok_or((404, format!("job {id} has no backup")))?;
                let data = std::fs::read(self.output(id)).map_err(|e| failed(e.to_string()))?;
                Ok(Response::new(200, "application/octet-stream", data).header(api::SHA256, o.sha256))
            }
            ("DELETE", "") => {
                let mut st = self.st();
                let i = st.jobs.iter().position(|r| r.job.id == id).ok_or_else(missing)?;
                if st.jobs[i].job.done.is_none() {
                    return Err((409, "the job is still running".into()));
                }
                st.jobs.remove(i);
                let _ = std::fs::remove_file(self.output(id));
                let _ = std::fs::remove_file(self.record(id));
                Ok(Response::json(200, &serde_json::json!({})))
            }
            _ => Err((404, format!("no such route: {method} {}", req.path))),
        }
    }

    fn connection(self: &Arc<Self>, mut s: TcpStream) {
        let peer = s.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
        let _ = s.set_read_timeout(Some(Duration::from_secs(30)));
        let _ = s.set_write_timeout(Some(Duration::from_secs(60)));
        let max = |m: &str, p: &str| if (m, p) == ("POST", "/api/v1/jobs") { ops::FLASH_SIZE + (1 << 20) } else { 64 * 1024 };
        let resp = match http::read_request(&mut s, max) {
            Ok(req) => self.handle(&req, &peer),
            Err(e) if e.kind() == std::io::ErrorKind::OutOfMemory => Response::error(413, "request too large"),
            Err(_) => return,
        };
        let _ = http::write_response(&mut s, &resp);
    }
}

/// Answers each connection on a thread of its own, at most MAX_CONNECTIONS at once.
pub fn serve(listener: TcpListener, d: Arc<Daemon>) {
    let active = Arc::new(Mutex::new(0usize));
    for conn in listener.incoming() {
        let Ok(mut s) = conn else { continue };
        {
            let mut n = active.lock().unwrap();
            if *n >= MAX_CONNECTIONS {
                let _ = http::write_response(&mut s, &Response::error(503, "too many connections"));
                continue;
            }
            *n += 1;
        }
        let (d, active) = (d.clone(), active.clone());
        std::thread::spawn(move || {
            d.connection(s);
            *active.lock().unwrap() -= 1;
        });
    }
}

/// Ctrl-C (or SIGTERM) presses so far; `watch_stops` acts on them.
static STOPS: AtomicUsize = AtomicUsize::new(0);

extern "C" fn on_stop(_: libc::c_int) {
    if STOPS.fetch_add(1, Ordering::Relaxed) >= 2 {
        unsafe { libc::_exit(130) };
    }
}

impl Daemon {
    fn running_op(&self) -> Option<String> {
        let st = self.st();
        st.running.as_ref().and_then(|id| st.jobs.iter().find(|r| &r.job.id == id)).map(|r| r.job.op.clone())
    }

    fn cancel_running(&self) {
        let st = self.st();
        if let Some(r) = st.running.as_ref().and_then(|id| st.jobs.iter().find(|r| &r.job.id == id)) {
            r.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Lets the GUI go and says which backups no GUI has saved yet.
    fn stop(&self) {
        self.end_session();
        let st = self.st();
        for r in st.jobs.iter().filter(|r| r.job.output.is_some()) {
            eprintln!(
                "k1cam-flasher daemon: kept {} ({}): no GUI has saved it; one connecting to a daemon with this state directory can",
                self.output(&r.job.id).display(),
                r.job.op
            );
        }
    }
}

/// Ctrl-C with no job running stops the daemon. During a job, the first waits
/// for it to end, the second cancels it (as the GUI's Cancel does), and the
/// third quits at once (`on_stop`).
fn watch_stops(d: Arc<Daemon>) {
    let mut seen = 0;
    loop {
        std::thread::sleep(Duration::from_millis(100));
        if d.on_stops(STOPS.load(Ordering::Relaxed), &mut seen) {
            eprintln!("\nk1cam-flasher daemon: stopping");
            d.stop();
            std::process::exit(0);
        }
    }
}

impl Daemon {
    /// After `n` presses (`seen` of them already acted on): whether to stop now.
    fn on_stops(&self, n: usize, seen: &mut usize) -> bool {
        if n == 0 {
            return false;
        }
        self.stopping.store(true, Ordering::Relaxed);
        let Some(op) = self.running_op() else { return true };
        if n == *seen {
            return false;
        }
        *seen = n;
        if n == 1 {
            eprintln!(
                "\nk1cam-flasher daemon: {op} is running; stopping once it ends. Ctrl-C again cancels it, \
                 which can leave the camera half-written."
            );
        } else {
            self.cancel_running();
            eprintln!("\nk1cam-flasher daemon: cancelling {op}; Ctrl-C again quits at once.");
        }
        false
    }
}

/// This computer's IPv4 addresses, but loopback.
fn addresses() -> Vec<String> {
    let mut out = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return out;
    }
    let mut p = list;
    while let Some(ifa) = unsafe { p.as_ref() } {
        let up = ifa.ifa_flags & (libc::IFF_UP as libc::c_uint) != 0 && ifa.ifa_flags & (libc::IFF_LOOPBACK as libc::c_uint) == 0;
        if let Some(a) = unsafe { ifa.ifa_addr.as_ref() }.filter(|a| up && a.sa_family == libc::AF_INET as libc::sa_family_t) {
            let a = unsafe { &*(a as *const libc::sockaddr as *const libc::sockaddr_in) };
            out.push(std::net::Ipv4Addr::from(u32::from_be(a.sin_addr.s_addr)).to_string());
        }
        p = ifa.ifa_next;
    }
    unsafe { libc::freeifaddrs(list) };
    out
}

/// Where jobs and backups are kept: /var/lib for root, the XDG state directory otherwise.
fn default_state_dir() -> PathBuf {
    if unsafe { libc::geteuid() } == 0 {
        return "/var/lib/k1cam-flasher".into();
    }
    match std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d).join("k1cam-flasher"),
        None => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state/k1cam-flasher"),
    }
}

/// `k1cam-flasher daemon …`; runs until killed.
pub fn run(args: &[String]) -> i32 {
    if !args.is_empty() {
        eprint!("{USAGE}");
        return 2;
    }
    let fail = |e: String| {
        eprintln!("k1cam-flasher daemon: {e}");
        1
    };
    let password = match new_password() {
        Ok(p) => p,
        Err(e) => return fail(e),
    };
    let dir = default_state_dir();
    let d = match Daemon::new(Arc::new(Here), password, &dir, "k1cam-flasher") {
        Ok(d) => Arc::new(d),
        Err(e) => return fail(e),
    };
    let listen = format!("0.0.0.0:{}", api::PORT);
    let listener = match TcpListener::bind(&listen) {
        Ok(l) => l,
        Err(e) => return fail(format!("{listen}: {e}")),
    };
    let hosts: Vec<String> = std::iter::once(d.host.clone()).filter(|h| !h.is_empty()).chain(addresses()).collect();
    eprintln!("k1cam-flasher daemon: listening on {listen} (plain HTTP: use it on a network you trust)");
    eprintln!("  connect to:  {}", hosts.join(", "));
    eprintln!("  code:        {}  (a new one after {MAX_WRONG} wrong attempts)", d.password());
    eprintln!("  backups in:  {}", dir.display());
    eprintln!("  Ctrl-C stops it, after a running job ends.");
    for sig in [libc::SIGINT, libc::SIGTERM] {
        unsafe { libc::signal(sig, on_stop as extern "C" fn(libc::c_int) as libc::sighandler_t) };
    }
    let watcher = d.clone();
    std::thread::spawn(move || watch_stops(watcher));
    let ticker = d.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(500));
        ticker.tick();
    });
    serve(listener, d);
    0
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::controls::Value;
    use std::sync::atomic::AtomicUsize;

    /// Pretend devices: counts what is opened, and blocks jobs until told.
    #[derive(Default)]
    pub struct Fake {
        pub diagnostic: Arc<Mutex<Option<Vec<u8>>>>,
        pub diagnostic_streams: Arc<Mutex<Vec<usize>>>,
        pub frames_open: Arc<AtomicUsize>,
        pub frames_opened: AtomicUsize,
        pub console_in: Arc<Mutex<Vec<u8>>>,
        pub console_out: Arc<Mutex<VecDeque<u8>>>,
        pub gate: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
        pub ran: Mutex<Vec<String>>,
        /// A real JPEG to send as every frame, for tests that decode them.
        pub picture: Mutex<Option<Vec<u8>>>,
        /// Hold a frame request open to exercise firmware handoff.
        pub frame_gate: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    }

    struct FakeFrames {
        n: u8,
        open: Arc<AtomicUsize>,
        size: (u32, u32),
        picture: Option<Vec<u8>>,
        gate: Option<std::sync::mpsc::Receiver<()>>,
    }

    impl Frames for FakeFrames {
        fn frame(&mut self, _: Duration) -> R<Option<Vec<u8>>> {
            if let Some(gate) = self.gate.take() {
                gate.recv_timeout(Duration::from_secs(5)).map_err(|e| e.to_string())?;
            }
            std::thread::sleep(Duration::from_millis(20));
            self.n = self.n.wrapping_add(1);
            Ok(Some(self.picture.clone().unwrap_or_else(|| vec![0xff, 0xd8, self.n, 0xff, 0xd9])))
        }
        fn size(&self) -> (u32, u32) {
            self.size
        }
    }

    impl Drop for FakeFrames {
        fn drop(&mut self) {
            self.open.fetch_sub(1, Ordering::SeqCst);
        }
    }

    struct FakeTuner {
        diagnostic: Arc<Mutex<Option<Vec<u8>>>>,
        streams: Arc<AtomicUsize>,
        readings: Arc<Mutex<Vec<usize>>>,
    }

    impl Tuner for FakeTuner {
        fn diagnostic(&self) -> R<Option<Vec<u8>>> {
            self.readings.lock().unwrap().push(self.streams.load(Ordering::SeqCst));
            Ok(self.diagnostic.lock().unwrap().clone())
        }
        fn sizes(&self) -> R<Vec<(u32, u32)>> {
            Ok(vec![(1920, 1080), (1280, 720)])
        }
        fn load(&self) -> R<Vec<Option<Value>>> {
            Ok(vec![Some(Value { value: 128, min: 0, max: 255, step: 1, default: 128, inactive: false }), None])
        }
        fn set(&self, _: usize, v: i32) -> R<i32> {
            Ok(v.clamp(0, 255))
        }
        fn get(&self, which: &[usize]) -> Vec<(usize, i32)> {
            which.iter().map(|&i| (i, 7)).collect()
        }
        fn reset(&self) -> R<()> {
            Ok(())
        }
    }

    struct FakePort {
        out: Arc<Mutex<VecDeque<u8>>>,
        input: Arc<Mutex<Vec<u8>>>,
    }

    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Port for FakePort {
        fn read(&mut self, buf: &mut [u8], wait: Duration) -> std::io::Result<Option<usize>> {
            let mut out = self.out.lock().unwrap();
            if out.is_empty() {
                drop(out);
                std::thread::sleep(wait.min(Duration::from_millis(20)));
                return Ok(None);
            }
            let n = out.len().min(buf.len());
            for b in buf.iter_mut().take(n) {
                *b = out.pop_front().unwrap();
            }
            Ok(Some(n))
        }
        fn writer(&self) -> std::io::Result<Box<dyn Write + Send>> {
            Ok(Box::new(Sink(self.input.clone())))
        }
    }

    impl Devices for Fake {
        fn status(&self) -> Status {
            Status { camera: true, video: true, console: true, ..Default::default() }
        }
        fn info(&self) -> R<String> {
            Ok("build=k1cam-test\nrunning_slot=b\n".into())
        }
        fn tuner(&self) -> R<Box<dyn Tuner>> {
            Ok(Box::new(FakeTuner { diagnostic: self.diagnostic.clone(), streams: self.frames_open.clone(),
                readings: self.diagnostic_streams.clone() }))
        }
        fn frames(&self, w: u32, h: u32, _: u32) -> R<Box<dyn Frames>> {
            self.frames_open.fetch_add(1, Ordering::SeqCst);
            self.frames_opened.fetch_add(1, Ordering::SeqCst);
            let picture = self.picture.lock().unwrap().clone();
            let gate = self.frame_gate.lock().unwrap().take();
            Ok(Box::new(FakeFrames { n: 0, open: self.frames_open.clone(), size: (w, h), picture, gate }))
        }
        fn console(&self) -> R<Box<dyn Port>> {
            Ok(Box::new(FakePort { out: self.console_out.clone(), input: self.console_in.clone() }))
        }
        fn run(&self, j: &Job, task: Task, out: &Path) -> R<String> {
            let name = match &task {
                Task::Flash { data, .. } => format!("flash {}", data.len()),
                Task::Update { data, .. } => format!("update {}", data.len()),
                Task::Backup => "backup".into(),
                Task::Dump { .. } => "dump".into(),
                _ => "other".into(),
            };
            self.ran.lock().unwrap().push(name);
            j.log("working");
            j.progress(0.5);
            if let Some(gate) = self.gate.lock().unwrap().take() {
                let _ = gate.recv();
            }
            j.check_cancel()?;
            if task.backs_up() {
                std::fs::write(out, b"backup bytes").map_err(|e| e.to_string())?;
            }
            Ok("done".into())
        }
    }

    pub const PASSWORD: &str = "TEST42";

    pub fn daemon(dir: &Path, fake: Arc<Fake>) -> Arc<Daemon> {
        let name = format!("k1cam-flasher-test-{}-{}", std::process::id(), dir.file_name().unwrap().to_string_lossy());
        let mut d = Daemon::new(fake, PASSWORD.into(), dir, &name).unwrap();
        d.slow = Duration::ZERO;
        Arc::new(d)
    }

    /// What a restart does to a GUI: its session gone, and a password it never had.
    pub fn restart(d: &Daemon, password: &str) {
        d.end_session();
        *d.auth.lock().unwrap() = Auth { password: password.into(), wrong: Vec::new(), retired: Vec::new() };
    }

    /// A GUI's session that lapsed.
    pub fn lapse(d: &Daemon) {
        d.end_session();
    }

    /// The different wrong passwords counted so far.
    pub fn wrong(d: &Daemon) -> usize {
        d.auth.lock().unwrap().wrong.len()
    }

    pub fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("k1cam-daemon-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn req(method: &str, target: &str, session: Option<&str>, body: &[u8]) -> Request {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let query =
            query.split('&').filter(|p| !p.is_empty()).map(|p| p.split_once('=').map(|(k, v)| (k.into(), v.into())).unwrap()).collect();
        let mut headers = vec![("Authorization".to_string(), format!("Bearer {PASSWORD}"))];
        if let Some(s) = session {
            headers.push((api::SESSION.into(), s.into()));
        }
        Request { method: method.into(), path: path.into(), query, headers, body: body.to_vec() }
    }

    fn session(d: &Arc<Daemon>, client: &str, peer: &str) -> Response {
        d.handle(&req("POST", "/api/v1/session", None, format!("{{\"client\":\"{client}\"}}").as_bytes()), peer)
    }

    fn connect(d: &Arc<Daemon>) -> String {
        session(d, "studio-pc", "10.0.0.2").parse::<Session>().unwrap().id
    }

    #[test]
    fn the_password_takes_a_session_and_the_session_does_the_rest() {
        let dir = scratch("auth");
        let d = daemon(&dir, Arc::default());
        assert_eq!(
            d.handle(&Request { headers: vec![], ..req("GET", "/api/hello", None, b"") }, "x").parse::<Hello>().unwrap().api,
            api::API
        );
        let mut wrong = req("POST", "/api/v1/session", None, br#"{"client":"t"}"#);
        wrong.headers[0].1 = "Bearer nope".into();
        assert_eq!(d.handle(&wrong, "x").status, 401);
        assert_eq!(d.handle(&req("GET", "/api/v1/status", None, b""), "x").status, 403);
        let id = connect(&d);
        assert!(d.handle(&req("GET", "/api/v1/status", Some(&id), b""), "x").parse::<Status>().unwrap().camera);
        // The session alone is enough; the password isn't asked again.
        let mut no_password = req("GET", "/api/v1/status", Some(&id), b"");
        no_password.headers.remove(0);
        assert_eq!(d.handle(&no_password, "x").status, 200);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn different_wrong_passwords_replace_it_and_repeated_or_old_ones_are_not_counted() {
        let dir = scratch("wrong");
        let d = daemon(&dir, Arc::default());
        let id = connect(&d);
        let with = |p: &str| {
            let mut r = req("POST", "/api/v1/session", None, br#"{"client":"other"}"#);
            r.headers[0].1 = format!("Bearer {p}");
            d.handle(&r, "10.0.0.9").status
        };
        assert_eq!(with("test42"), 409, "any case, then the camera is taken");
        // A GUI still sending one wrong password uses up one attempt.
        for _ in 0..MAX_WRONG * 2 {
            assert_eq!(with("STALE1"), 401);
        }
        for n in 2..MAX_WRONG {
            assert_eq!(with(&format!("nope{n}")), 401);
        }
        assert_eq!(d.password(), PASSWORD);
        assert_eq!(with("last"), 401);
        let new = d.password();
        assert_ne!(new, PASSWORD);
        assert!(new.len() == PASSWORD_LEN && new.bytes().all(|b| PASSWORD_CHARS.contains(&b)), "{new}");
        assert_eq!(with(&new), 409);
        // The GUI that has the camera keeps it.
        assert_eq!(d.handle(&req("GET", "/api/v1/status", Some(&id), b""), "x").status, 200);
        // A GUI still using the old one is refused without using up attempts.
        for _ in 0..MAX_WRONG * 2 {
            assert_eq!(with(PASSWORD), 401);
        }
        assert_eq!(d.password(), new);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_stopping_daemon_starts_no_jobs() {
        let dir = scratch("stopping");
        let d = daemon(&dir, Arc::default());
        let sid = connect(&d);
        d.stopping.store(true, Ordering::Relaxed);
        let r = d.handle(&req("POST", "/api/v1/jobs?id=b-1&op=backup", Some(&sid), b""), "x");
        assert_eq!(r.status, 503);
        assert!(r.message().contains("stopping"), "{}", r.message());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ctrl_c_waits_for_a_job_then_cancels_it() {
        let dir = scratch("ctrl-c");
        let fake = Arc::new(Fake::default());
        let (release, gate) = channel();
        *fake.gate.lock().unwrap() = Some(gate);
        let d = daemon(&dir, fake);
        let sid = connect(&d);
        let mut seen = 0;
        assert!(!d.on_stops(0, &mut seen));
        assert_eq!(d.handle(&req("POST", "/api/v1/jobs?id=b-1&op=backup", Some(&sid), b""), "x").status, 201);
        // The first waits, and no other job starts meanwhile.
        assert!(!d.on_stops(1, &mut seen));
        assert!(!d.on_stops(1, &mut seen));
        assert_eq!(d.handle(&req("POST", "/api/v1/jobs?id=d-1&op=detect", Some(&sid), b""), "x").status, 409);
        assert!(!d.st().jobs[0].cancel.load(Ordering::Relaxed));
        // The second cancels; the daemon stops once the job has.
        assert!(!d.on_stops(2, &mut seen));
        assert!(d.st().jobs[0].cancel.load(Ordering::Relaxed));
        release.send(()).unwrap();
        let job = wait_done(&d, &sid, "b-1");
        assert_eq!(job.done.unwrap().unwrap_err(), "cancelled");
        assert!(d.on_stops(2, &mut seen));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn one_gui_controls_until_its_lease_runs_out() {
        let dir = scratch("lease");
        let mut d = daemon(&dir, Arc::default());
        Arc::get_mut(&mut d).unwrap().lease = Duration::from_millis(300);
        let id = connect(&d);
        let other = session(&d, "laptop", "10.0.0.3");
        assert_eq!(other.status, 409);
        assert!(other.message().contains("studio-pc (10.0.0.2)"), "{}", other.message());
        // A second window on the same computer cannot take over by name.
        assert_eq!(session(&d, "studio-pc", "10.0.0.2").status, 409);
        // Only the window with the credential can resume, even after its IP changes.
        let body = serde_json::to_vec(&NewSession { client: "studio-pc".into(), resume: Some(id.clone()) }).unwrap();
        let again = d.handle(&req("POST", "/api/v1/session", None, &body), "10.0.0.4").parse::<Session>().unwrap().id;
        assert_eq!(again, id, "resuming must not invalidate concurrent requests");
        // Renewing keeps it; silence lets it go.
        std::thread::sleep(Duration::from_millis(200));
        assert!(d.handle(&req("GET", "/api/v1/status", Some(&again), b""), "x").ok());
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(session(&d, "laptop", "10.0.0.3").status, 409);
        std::thread::sleep(Duration::from_millis(400));
        d.tick();
        assert!(d.st().lock.is_none(), "an expired session lets go of the lock");
        assert!(session(&d, "laptop", "10.0.0.3").ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn diagnostics_need_a_session_without_capturing_video() {
        let dir = scratch("diagnostic");
        let fake = Arc::new(Fake::default());
        let d = daemon(&dir, fake.clone());
        let route = "/api/v1/tuning/diagnostic";
        assert_eq!(d.handle(&req("GET", route, None, b""), "x").status, 403);
        let id = connect(&d);
        assert_eq!(d.handle(&req("GET", route, Some(&id), b""), "x").status, 204);
        *fake.diagnostic.lock().unwrap() = Some(crate::diag::tests::record());
        let response = d.handle(&req("GET", route, Some(&id), b""), "x");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, crate::diag::tests::record());
        assert_eq!(fake.frames_opened.load(Ordering::SeqCst), 0);
        d.end_session();
        assert_eq!(d.handle(&req("GET", route, Some(&id), b""), "x").status, 403);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn manual_snapshots_start_and_stop_the_video() {
        let dir = scratch("manual");
        let fake = Arc::new(Fake::default());
        let d = daemon(&dir, fake.clone());
        let id = connect(&d);
        // Connecting and controls capture nothing.
        assert!(d.handle(&req("GET", "/api/v1/tuning/controls", Some(&id), b""), "x").ok());
        assert!(d.handle(&req("POST", "/api/v1/tuning/set?i=0&value=300", Some(&id), b""), "x").parse::<i32>().unwrap() == 255);
        assert_eq!(fake.frames_opened.load(Ordering::SeqCst), 0);
        let r = d.handle(&req("GET", "/api/v1/snapshot.jpg?width=640&height=360", Some(&id), b""), "x");
        assert_eq!((r.status, r.get(api::WIDTH), r.get("Cache-Control")), (200, Some("640"), Some("no-store")));
        // The first frame after the start is passed over.
        assert_eq!(r.body[2], 2);
        assert_eq!(fake.frames_open.load(Ordering::SeqCst), 0, "a manual snapshot stops the video again");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn snapshot_diagnostics_are_read_before_the_stream_closes_and_only_when_asked() {
        let dir = scratch("snapshot-diagnostic");
        let fake = Arc::new(Fake::default());
        *fake.diagnostic.lock().unwrap() = Some(crate::diag::tests::record());
        let d = daemon(&dir, fake.clone());
        let id = connect(&d);
        let plain = d.handle(&req("GET", "/api/v1/snapshot.jpg", Some(&id), b""), "x");
        assert!(plain.get(api::DIAGNOSTIC).is_none());
        assert!(fake.diagnostic_streams.lock().unwrap().is_empty());
        let paired = d.handle(&req("GET", "/api/v1/snapshot.jpg?diagnostic=1", Some(&id), b""), "x");
        assert_eq!(paired.get(api::DIAGNOSTIC), Some(xu::hex(&crate::diag::tests::record()).as_str()));
        assert_eq!(*fake.diagnostic_streams.lock().unwrap(), vec![1]);
        assert_eq!(fake.frames_open.load(Ordering::SeqCst), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn automatic_snapshots_keep_one_stream_and_stop_when_nobody_asks() {
        let dir = scratch("auto");
        let fake = Arc::new(Fake::default());
        let mut d = daemon(&dir, fake.clone());
        Arc::get_mut(&mut d).unwrap().idle = Duration::from_millis(300);
        let id = connect(&d);
        let shot = |after: u64| {
            d.handle(&req("GET", &format!("/api/v1/snapshot.jpg?width=640&height=360&auto=1&after={after}"), Some(&id), b""), "x")
        };
        let a = shot(0);
        let seq: u64 = a.get(api::SEQ).unwrap().parse().unwrap();
        let b = shot(seq);
        assert!(b.get(api::SEQ).unwrap().parse::<u64>().unwrap() > seq, "after= gives a newer frame");
        assert!(a.get(api::AGE).is_some());
        assert_eq!((fake.frames_opened.load(Ordering::SeqCst), fake.frames_open.load(Ordering::SeqCst)), (1, 1));
        // A manual snapshot meanwhile takes the running stream's next frame.
        assert!(d.handle(&req("GET", "/api/v1/snapshot.jpg?width=640&height=360", Some(&id), b""), "x").ok());
        assert_eq!(fake.frames_opened.load(Ordering::SeqCst), 1);
        std::thread::sleep(Duration::from_millis(400));
        d.tick();
        assert_eq!(fake.frames_open.load(Ordering::SeqCst), 0, "an unasked capture stops");
        let _ = std::fs::remove_dir_all(dir);
    }

    fn wait_done(d: &Arc<Daemon>, sid: &str, id: &str) -> api::Job {
        for _ in 0..100 {
            let j: api::Job = d.handle(&req("GET", &format!("/api/v1/jobs/{id}?wait=100"), Some(sid), b""), "x").parse().unwrap();
            if j.done.is_some() {
                return j;
            }
        }
        panic!("job {id} did not finish");
    }

    #[test]
    fn recovery_write_failures_refuse_jobs_and_preserve_retry_safety() {
        for rename in [false, true] {
            let dir = scratch(if rename { "record-rename" } else { "record-write" });
            let fake = Arc::new(Fake::default());
            let d = daemon(&dir, fake.clone());
            let sid = connect(&d);
            let blocked = dir.join(if rename { "jobs/u-1.json" } else { "jobs/u-1.tmp" });
            std::fs::create_dir(&blocked).unwrap();
            let body = include_bytes!("../tests/data/golden.upd");
            let start = req("POST", "/api/v1/jobs?id=u-1&op=update", Some(&sid), body);
            let r = d.handle(&start, "x");
            assert_eq!(r.status, 500);
            assert!(r.message().contains("saving job u-1"));
            assert!(fake.ran.lock().unwrap().is_empty());
            assert!(d.st().running.is_none() && d.st().jobs.is_empty());
            std::fs::remove_dir(blocked).unwrap();
            assert_eq!(d.handle(&start, "x").status, 201);
            assert!(wait_done(&d, &sid, "u-1").done.unwrap().is_ok());
            d.end_session();
            drop(d);
            let restarted = daemon(&dir, fake.clone());
            let sid = connect(&restarted);
            let again = req("POST", "/api/v1/jobs?id=u-1&op=update", Some(&sid), body);
            assert_eq!(restarted.handle(&again, "x").status, 200);
            assert_eq!(fake.ran.lock().unwrap().len(), 1, "a persisted update is never started twice");
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn an_unreadable_recovery_record_prevents_startup() {
        let dir = scratch("bad-record");
        std::fs::create_dir_all(dir.join("jobs")).unwrap();
        std::fs::write(dir.join("jobs/u-1.json"), b"damaged").unwrap();
        let result = Daemon::new(Arc::new(Fake::default()), PASSWORD.into(), &dir, "unused");
        assert!(result.err().unwrap().contains("u-1.json"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn outcome_write_failures_are_reported_and_recovered_as_interrupted() {
        let dir = scratch("outcome-write");
        let fake = Arc::new(Fake::default());
        let (release, gate) = channel();
        *fake.gate.lock().unwrap() = Some(gate);
        let d = daemon(&dir, fake.clone());
        let sid = connect(&d);
        assert_eq!(d.handle(&req("POST", "/api/v1/jobs?id=b-1&op=backup", Some(&sid), b""), "x").status, 201);
        std::fs::create_dir(dir.join("jobs/b-1.tmp")).unwrap();
        release.send(()).unwrap();
        let job = wait_done(&d, &sid, "b-1");
        assert!(job.done.unwrap().unwrap_err().contains("saving job b-1"));
        assert!(job.output.is_some());
        d.end_session();
        drop(d);
        std::fs::remove_dir(dir.join("jobs/b-1.tmp")).unwrap();
        let restarted = daemon(&dir, fake);
        let sid = connect(&restarted);
        let job = wait_done(&restarted, &sid, "b-1");
        assert!(job.done.unwrap().unwrap_err().starts_with("interrupted"));
        assert!(job.output.is_some(), "the backup survives a failed outcome write");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn firmware_waits_for_manual_snapshots_without_blocking_status() {
        let dir = scratch("snapshot-handoff");
        let fake = Arc::new(Fake::default());
        let (release, gate) = channel();
        *fake.frame_gate.lock().unwrap() = Some(gate);
        let d = daemon(&dir, fake.clone());
        let sid = connect(&d);
        let (d2, sid2) = (d.clone(), sid.clone());
        let shot = std::thread::spawn(move || d2.handle(&req("GET", "/api/v1/snapshot.jpg", Some(&sid2), b""), "x"));
        let end = Instant::now() + Duration::from_secs(5);
        while fake.frames_open.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < end, "snapshot did not start");
            std::thread::sleep(Duration::from_millis(5));
        }
        let (d2, sid2) = (d.clone(), sid.clone());
        let (started, waiting) = channel();
        let update = std::thread::spawn(move || {
            started.send(()).unwrap();
            d2.handle(&req("POST", "/api/v1/jobs?id=u-1&op=update", Some(&sid2), include_bytes!("../tests/data/golden.upd")), "x")
        });
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(d.handle(&req("GET", "/api/v1/status", Some(&sid), b""), "x").ok());
        std::thread::sleep(Duration::from_millis(50));
        assert!(fake.ran.lock().unwrap().is_empty(), "the firmware job overlapped the snapshot");
        release.send(()).unwrap();
        assert_eq!(shot.join().unwrap().status, 200);
        assert_eq!(update.join().unwrap().status, 201);
        assert!(wait_done(&d, &sid, "u-1").done.unwrap().is_ok());
        assert_eq!(fake.frames_open.load(Ordering::SeqCst), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn jobs_run_once_and_keep_their_backups_until_dismissed() {
        let dir = scratch("jobs");
        let fake = Arc::new(Fake::default());
        let d = daemon(&dir, fake.clone());
        let sid = connect(&d);
        let (open, gate) = channel();
        *fake.gate.lock().unwrap() = Some(gate);
        // Tuning stops for the job, and refuses while it runs.
        assert!(d.handle(&req("GET", "/api/v1/tuning/sizes", Some(&sid), b""), "x").ok());
        let r = d.handle(&req("POST", "/api/v1/jobs?id=b-1&op=backup", Some(&sid), b""), "x");
        assert_eq!(r.status, 201);
        assert!(d.st().tuner.is_none());
        assert_eq!(d.handle(&req("GET", "/api/v1/tuning/sizes", Some(&sid), b""), "x").status, 409);
        assert_eq!(d.handle(&req("POST", "/api/v1/jobs?id=b-2&op=dump", Some(&sid), b""), "x").status, 409);
        // Asking again for the same job gets it, not a second run.
        assert_eq!(d.handle(&req("POST", "/api/v1/jobs?id=b-1&op=backup", Some(&sid), b""), "x").status, 200);
        let first: api::Job = d.handle(&req("GET", "/api/v1/jobs/b-1?wait=2000", Some(&sid), b""), "x").parse().unwrap();
        assert_eq!(first.log, ["working"]);
        open.send(()).unwrap();
        let j = wait_done(&d, &sid, "b-1");
        assert_eq!(j.done, Some(Ok("done".into())));
        assert_eq!(j.output.as_ref().unwrap().size, 12);
        assert_eq!(fake.ran.lock().unwrap().len(), 1);
        // The backup comes back with its digest, and stays until dismissed.
        let o = d.handle(&req("GET", "/api/v1/jobs/b-1/output", Some(&sid), b""), "x");
        assert_eq!((o.body.as_slice(), o.get(api::SHA256)), (&b"backup bytes"[..], Some(j.output.unwrap().sha256.as_str())));
        assert!(dir.join("jobs/b-1.bin").exists());
        assert!(d.handle(&req("DELETE", "/api/v1/jobs/b-1", Some(&sid), b""), "x").ok());
        assert!(!dir.join("jobs/b-1.bin").exists() && !dir.join("jobs/b-1.json").exists());
        // Uploads are checked and held in memory.
        assert_eq!(d.handle(&req("POST", "/api/v1/jobs?id=f-1&op=flash", Some(&sid), &[0; 100]), "x").status, 400);
        assert_eq!(d.handle(&req("POST", "/api/v1/jobs?id=u-1&op=update", Some(&sid), b"not a bundle"), "x").status, 400);
        assert!(d.handle(&req("POST", "/api/v1/jobs?id=f-2&op=flash", Some(&sid), &vec![0; ops::FLASH_SIZE]), "x").ok());
        wait_done(&d, &sid, "f-2");
        assert_eq!(fake.ran.lock().unwrap()[1], format!("flash {}", ops::FLASH_SIZE));
        let kept: Vec<_> = std::fs::read_dir(dir.join("jobs")).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(kept, ["f-2.json"], "the upload was never written");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn backups_are_capped_rather_than_deleted() {
        let dir = scratch("cap");
        let d = daemon(&dir, Arc::default());
        let sid = connect(&d);
        for i in 0..MAX_OUTPUTS {
            assert!(d.handle(&req("POST", &format!("/api/v1/jobs?id=d-{i}&op=dump"), Some(&sid), b""), "x").ok());
            wait_done(&d, &sid, &format!("d-{i}"));
        }
        let r = d.handle(&req("POST", "/api/v1/jobs?id=d-x&op=backup", Some(&sid), b""), "x");
        assert_eq!(r.status, 409);
        assert!(r.message().contains("save or discard"));
        // Jobs that make no backup still run.
        assert!(d.handle(&req("POST", "/api/v1/jobs?id=r-1&op=reboot", Some(&sid), b""), "x").ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_restart_marks_a_running_job_interrupted_and_keeps_backups() {
        let dir = scratch("restart");
        std::fs::create_dir_all(dir.join("jobs")).unwrap();
        let job =
            api::Job { id: "f-9".into(), op: "flash".into(), started: 1, log: vec![], next: 0, progress: 0.4, done: None, output: None };
        std::fs::write(dir.join("jobs/f-9.json"), serde_json::to_vec(&job).unwrap()).unwrap();
        std::fs::write(dir.join("jobs/f-9.bin"), b"pre-flash backup").unwrap();
        let d = daemon(&dir, Arc::default());
        let sid = connect(&d);
        let jobs: Vec<api::Job> = d.handle(&req("GET", "/api/v1/jobs", Some(&sid), b""), "x").parse().unwrap();
        assert!(matches!(&jobs[0].done, Some(Err(e)) if e.starts_with("interrupted")));
        assert_eq!(jobs[0].output.as_ref().unwrap().size, 16);
        assert!(d.st().running.is_none(), "nothing resumes by itself");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_console_passes_bytes_both_ways() {
        let dir = scratch("console");
        let fake = Arc::new(Fake::default());
        let d = daemon(&dir, fake.clone());
        let sid = connect(&d);
        assert!(d.handle(&req("POST", "/api/v1/console/open", Some(&sid), b""), "x").ok());
        fake.console_out.lock().unwrap().extend(b"# ");
        let r = d.handle(&req("GET", "/api/v1/console?after=0&wait=2000", Some(&sid), b""), "x");
        assert_eq!((r.body.as_slice(), r.get(api::NEXT), r.get(api::OPEN)), (&b"# "[..], Some("2"), Some("1")));
        assert!(d.handle(&req("POST", "/api/v1/console/input", Some(&sid), b"ls\r"), "x").ok());
        assert_eq!(fake.console_in.lock().unwrap().as_slice(), b"ls\r");
        // A long poll with nothing new waits, then answers empty.
        let r = d.handle(&req("GET", "/api/v1/console?after=2&wait=50", Some(&sid), b""), "x");
        assert_eq!((r.body.len(), r.get(api::NEXT)), (0, Some("2")));
        d.end_session();
        assert!(d.st().console.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn over_tcp() {
        let dir = scratch("tcp");
        let d = daemon(&dir, Arc::default());
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        std::thread::spawn(move || serve(l, d));
        let auth = format!("Bearer {PASSWORD}");
        let r = http::call(&addr, "GET", "/api/hello", &[], b"", Duration::from_secs(5)).unwrap();
        assert_eq!(r.parse::<Hello>().unwrap().api, api::API);
        let r = http::call(&addr, "POST", "/api/v1/session", &[("Authorization", &auth)], br#"{"client":"t"}"#, Duration::from_secs(5))
            .unwrap();
        let s: Session = r.parse().unwrap();
        let big = vec![0u8; 128 * 1024];
        let r = http::call(
            &addr,
            "POST",
            "/api/v1/console/input",
            &[("Authorization", &auth), (api::SESSION, &s.id)],
            &big,
            Duration::from_secs(5),
        );
        assert!(r.map(|r| r.status == 413).unwrap_or(true), "bodies are bounded");
        let _ = std::fs::remove_dir_all(dir);
    }
}
