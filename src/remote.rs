// SPDX-License-Identifier: MIT
//! The GUI's remote mode: `k1cam-flasher daemon` on another computer (see
//! daemon.rs) does there what the GUI does here. A thread keeps the session
//! alive and the host's status fresh. Jobs run there and are followed from
//! here; a backup comes back once the job is done, and the host keeps it until
//! it is saved here. Uploads go in the request that starts the job.

use crate::api::{self, Hello, InUse, Session, Status};
use crate::bundle::Bundle;
use crate::controls::{Tuner, Value};
use crate::http::{self, Response};
use crate::job::Job;
use sha2::{Digest, Sha256};
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type R<T> = Result<T, String>;

/// How the connection is doing.
#[derive(Clone, Debug, PartialEq)]
pub enum Link {
    Up,
    /// Unanswered since then; the keeper keeps trying.
    Lost {
        since: Instant,
        error: String,
    },
    /// Another GUI took the camera once this one's session lapsed. Nothing is
    /// asked of the host until `reconnect`.
    Taken(String),
    /// This window's session is gone (the daemon restarted, or it lapsed) and
    /// the host has a new password. Nothing is asked of the host until
    /// `reconnect` brings it, so that the old one uses up no attempts there.
    Refused(String),
}

/// Why a request in this session failed.
#[derive(Clone, Debug)]
enum Fail {
    /// 401: the host has another password.
    Refused,
    /// 409 to a new session: another flasher has the camera.
    Taken(String),
    /// No answer, or none that makes sense.
    Other(String),
}

pub struct Remote {
    /// As typed, for showing: "camera-host.lan".
    pub host: String,
    /// With the port: "camera-host.lan:19249".
    pub addr: String,
    pub hello: Hello,
    /// The host's own name for itself.
    pub name: String,
    password: Mutex<String>,
    client: String,
    session: Mutex<String>,
    status: Mutex<Status>,
    link: Mutex<Link>,
    stop: AtomicBool,
}

/// `host` with the daemon's port unless it names one.
pub fn address(host: &str) -> String {
    let h = host.trim();
    let has_port = match h.rsplit_once(':') {
        Some((a, p)) => !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) && (!a.contains(':') || a.ends_with(']')),
        None => false,
    };
    if has_port {
        h.to_string()
    } else if h.contains(':') && !h.starts_with('[') {
        format!("[{h}]:{}", api::PORT)
    } else {
        format!("{h}:{}", api::PORT)
    }
}

/// Who this GUI is, for the host to name it to others: user@computer.
pub fn client_name() -> String {
    let user = std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_default();
    #[cfg(unix)]
    let host = {
        let mut buf = [0u8; 256];
        match unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } {
            0 => String::from_utf8_lossy(buf.split(|&b| b == 0).next().unwrap_or(&[])).into_owned(),
            _ => String::new(),
        }
    };
    #[cfg(not(unix))]
    let host = std::env::var("COMPUTERNAME").unwrap_or_default();
    match (user.is_empty(), host.is_empty()) {
        (false, false) => format!("{user}@{host}"),
        (true, false) => host,
        (false, true) => user,
        (true, true) => "k1cam-flasher".into(),
    }
}

/// An id for a job, so that a request repeated after a lost answer starts nothing new.
pub fn new_id() -> String {
    static COUNT: AtomicU32 = AtomicU32::new(0);
    let mut h = RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    h.write_u32(COUNT.fetch_add(1, Ordering::Relaxed));
    format!("{:016x}-{}", h.finish(), std::process::id())
}

const QUICK: Duration = Duration::from_secs(10);

impl Remote {
    /// Says hello, checks the protocol, takes a session and starts keeping it.
    pub fn connect(host: &str, password: &str) -> R<Arc<Remote>> {
        let addr = address(host);
        let hello: Hello = http::call(&addr, "GET", "/api/hello", &[], b"", QUICK)?.parse().map_err(|e| format!("{addr}: {e}"))?;
        if hello.api != api::API {
            return Err(format!(
                "{} runs a k1cam-flasher ({}) whose remote protocol is v{}; this one speaks v{}. Update the older one.",
                host.trim(),
                hello.version,
                hello.api,
                api::API
            ));
        }
        let r = Remote {
            host: host.trim().to_string(),
            addr,
            hello,
            name: String::new(),
            password: Mutex::new(password.trim().to_string()),
            client: client_name(),
            session: Mutex::new(String::new()),
            status: Mutex::new(Status::default()),
            link: Mutex::new(Link::Up),
            stop: AtomicBool::new(false),
        };
        let s = r.new_session().map_err(|f| r.said(&f))?;
        let r = Arc::new(Remote { name: s.host, ..r });
        r.refresh();
        let keeper = Arc::downgrade(&r);
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(1000));
            match keeper.upgrade() {
                Some(r) if !r.stop.load(Ordering::Relaxed) => r.refresh(),
                _ => break,
            }
        });
        Ok(r)
    }

    /// Takes a session, or this window's again, and says how the link is
    /// doing; under the session's lock, so that what it says is the latest.
    fn new_session(&self) -> Result<Session, Fail> {
        // Serialize renewals, and keep the credential stable when resuming:
        // simultaneous requests must not invalidate one another's session.
        let mut session = self.session.lock().unwrap();
        let resume = (!session.is_empty()).then(|| session.clone());
        let body = serde_json::to_vec(&api::NewSession { client: self.client.clone(), resume }).unwrap_or_default();
        let auth = format!("Bearer {}", self.password.lock().unwrap());
        let resp = http::call(&self.addr, "POST", &format!("/api/v{}/session", api::API), &[("Authorization", &auth)], &body, QUICK)
            .map_err(Fail::Other)?;
        let got = match resp.status {
            401 => Err(Fail::Refused),
            409 => Err(Fail::Taken(
                serde_json::from_slice::<InUse>(&resp.body)
                    .map(|u| format!("{}: {}", self.host, u.error))
                    .unwrap_or_else(|_| resp.message()),
            )),
            _ => resp.parse::<Session>().map_err(Fail::Other),
        };
        let mut link = self.link.lock().unwrap();
        match &got {
            Ok(s) => {
                *session = s.id.clone();
                *link = Link::Up;
            }
            Err(f @ Fail::Refused) => *link = Link::Refused(self.said(f)),
            Err(Fail::Taken(e)) => *link = Link::Taken(e.clone()),
            Err(Fail::Other(_)) => {}
        }
        got
    }

    /// One status request, which also renews the session.
    fn refresh(&self) {
        match self.request("GET", "status", b"", QUICK).and_then(|r| r.parse::<Status>().map_err(Fail::Other)) {
            Ok(s) => {
                *self.status.lock().unwrap() = s;
                let mut l = self.link.lock().unwrap();
                if matches!(*l, Link::Lost { .. }) {
                    *l = Link::Up;
                }
            }
            Err(Fail::Other(e)) => {
                let mut l = self.link.lock().unwrap();
                match &*l {
                    Link::Up => *l = Link::Lost { since: Instant::now(), error: e },
                    Link::Lost { since, .. } => *l = Link::Lost { since: *since, error: e },
                    Link::Taken(_) | Link::Refused(_) => {}
                }
            }
            // The link says so already.
            Err(_) => {}
        }
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    pub fn link(&self) -> Link {
        self.link.lock().unwrap().clone()
    }

    /// After Refused or Taken: takes this window's session again, with
    /// `password` when one is given. What follows the host carries on.
    pub fn reconnect(&self, password: Option<&str>) -> R<()> {
        if let Some(p) = password {
            *self.password.lock().unwrap() = p.trim().to_string();
        }
        self.new_session().map_err(|f| self.said(&f))?;
        self.refresh();
        Ok(())
    }

    /// Lets the host go: it closes what it had open for this GUI; a job runs on.
    /// Refused or taken, there is nothing there to let go, and nothing is sent.
    pub fn disconnect(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.call("DELETE", "session", b"", Duration::from_secs(3));
    }

    /// A request in this session; one that finds the session lapsed takes a new one and tries again.
    pub fn call(&self, method: &str, route: &str, body: &[u8], wait: Duration) -> R<Response> {
        self.request(method, route, body, wait).map_err(|f| self.said(&f))
    }

    fn request(&self, method: &str, route: &str, body: &[u8], wait: Duration) -> Result<Response, Fail> {
        match &*self.link.lock().unwrap() {
            Link::Refused(_) => return Err(Fail::Refused),
            Link::Taken(e) => return Err(Fail::Taken(e.clone())),
            Link::Up | Link::Lost { .. } => {}
        }
        let target = format!("/api/v{}/{route}", api::API);
        for again in [false, true] {
            // Only a new session needs the password; the first daemons asked
            // for it every time, so it goes along.
            let auth = format!("Bearer {}", self.password.lock().unwrap());
            let session = self.session.lock().unwrap().clone();
            let resp = http::call(&self.addr, method, &target, &[("Authorization", &auth), (api::SESSION, &session)], body, wait)
                .map_err(Fail::Other)?;
            if resp.status == 401 {
                *self.link.lock().unwrap() = Link::Refused(self.said(&Fail::Refused));
                return Err(Fail::Refused);
            }
            if resp.status == 403 && !again && !self.stop.load(Ordering::Relaxed) {
                self.new_session()?;
                continue;
            }
            return Ok(resp);
        }
        unreachable!()
    }

    fn said(&self, f: &Fail) -> String {
        match f {
            Fail::Refused => format!("{} refused the code: enter the one its terminal shows now", self.host),
            Fail::Taken(e) | Fail::Other(e) => e.clone(),
        }
    }

    fn json<T: serde::de::DeserializeOwned>(&self, method: &str, route: &str, body: &[u8], wait: Duration) -> R<T> {
        self.call(method, route, body, wait)?.parse()
    }

    pub fn info(&self) -> R<String> {
        let r = self.call("GET", "info", b"", Duration::from_secs(30))?;
        if !r.ok() {
            return Err(r.message());
        }
        Ok(String::from_utf8_lossy(&r.body).into_owned())
    }

    // ---- jobs ----

    /// Jobs the host still keeps (finished ones wait to be dismissed).
    pub fn jobs(&self) -> R<Vec<api::Job>> {
        self.json("GET", "jobs", b"", QUICK)
    }

    pub fn dismiss(&self, id: &str) -> R<()> {
        let r = self.call("DELETE", &format!("jobs/{id}"), b"", QUICK)?;
        if r.ok() {
            Ok(())
        } else {
            Err(r.message())
        }
    }

    /// Starts a job there and follows it to its end: its log and progress
    /// show here, and Cancel reaches it. A lost connection is waited out, as
    /// the job carries on there; so is a refused or taken one, until
    /// `reconnect` brings it back.
    fn run(&self, j: &Job, op: &str, params: &str, upload: &[u8]) -> R<api::Job> {
        let id = new_id();
        let route = format!("jobs?id={id}&op={op}{params}");
        if !upload.is_empty() {
            j.log(format!("Sending {} to {}", crate::gui::bytes(upload.len() as u64), self.host));
        }
        // Repeating the request is safe: the id makes it the same job.
        let (mut lost, mut stuck) = (None::<Instant>, false);
        loop {
            match self.request("POST", &route, upload, Duration::from_secs(120)) {
                Ok(r) if r.ok() => break,
                Ok(r) => return Err(r.message()),
                Err(_) if self.stop.load(Ordering::Relaxed) => {
                    return Err(format!("disconnected from {}; the job did not start", self.host));
                }
                Err(Fail::Other(e)) => {
                    j.check_cancel()?;
                    if lost.is_none() {
                        j.log(format!("{e}; trying again"));
                    }
                    if lost.get_or_insert_with(Instant::now).elapsed() > Duration::from_secs(60) {
                        return Err(format!("{e}; the job did not start"));
                    }
                    std::thread::sleep(Duration::from_secs(2));
                }
                // Nothing is sent meanwhile: it waits for the connection menu.
                Err(f) => {
                    j.check_cancel()?;
                    if !std::mem::replace(&mut stuck, true) {
                        j.log(format!("{}; it starts once connected again", self.said(&f)));
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        }
        self.follow(j, &id)
    }

    /// Follows a job that runs there until it ends.
    pub fn follow(&self, j: &Job, id: &str) -> R<api::Job> {
        let (mut next, mut cancelled, mut lost) = (0, false, None::<Instant>);
        loop {
            if !cancelled && j.check_cancel().is_err() {
                cancelled = self.call("POST", &format!("jobs/{id}/cancel"), b"", QUICK).is_ok_and(|r| r.ok());
            }
            let route = format!("jobs/{id}?after={next}&wait=1000");
            match self.request("GET", &route, b"", Duration::from_secs(15)).and_then(|r| r.parse::<api::Job>().map_err(Fail::Other)) {
                Ok(job) => {
                    if lost.take().is_some() {
                        j.log(format!("Connected to {} again", self.host));
                    }
                    for l in &job.log {
                        j.log(l.clone());
                    }
                    next = job.next;
                    j.progress(job.progress);
                    if job.done.is_some() {
                        return Ok(job);
                    }
                }
                Err(_) if self.stop.load(Ordering::Relaxed) => {
                    return Err(format!("disconnected from {}; the job carries on there", self.host));
                }
                Err(f) => {
                    let since = *lost.get_or_insert_with(|| {
                        j.log(match &f {
                            Fail::Other(e) => format!("Lost the connection to {}; the job carries on there ({e})", self.host),
                            _ => format!("{}; the job carries on there", self.said(&f)),
                        });
                        Instant::now()
                    });
                    // Cancel can't reach it: say so rather than wait forever.
                    if j.check_cancel().is_err() && !cancelled && since.elapsed() > Duration::from_secs(10) {
                        return Err(format!("could not reach {} to cancel; the job may still be running there", self.host));
                    }
                    std::thread::sleep(Duration::from_secs(if matches!(f, Fail::Other(_)) { 2 } else { 1 }));
                }
            }
        }
    }

    /// A job that makes nothing to keep: its outcome, and it is dismissed there.
    fn simple(&self, j: &Job, op: &str, params: &str, upload: &[u8]) -> R<String> {
        let job = self.run(j, op, params, upload)?;
        let _ = self.dismiss(&job.id);
        job.done.unwrap_or_else(|| Err("no outcome".into()))
    }

    /// Fetches a job's backup into `path` (a new file), checks it, and lets the host delete it.
    pub fn save_output(&self, id: &str, path: &Path) -> R<String> {
        let r = self.call("GET", &format!("jobs/{id}/output"), b"", Duration::from_secs(120))?;
        if !r.ok() {
            return Err(r.message());
        }
        let sha = crate::xu::hex(&Sha256::digest(&r.body));
        if r.get(api::SHA256) != Some(sha.as_str()) {
            return Err("the backup arrived damaged (its SHA-256 differs); it stays on the host".into());
        }
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        f.write_all(&r.body).and_then(|_| f.sync_all()).map_err(|e| format!("{}: {e}", path.display()))?;
        let _ = self.dismiss(id);
        Ok(sha)
    }

    /// A job's backup into `path`; if that fails the host keeps it, and says so.
    fn bring_back(&self, job: &api::Job, path: &Path) -> R<String> {
        self.save_output(&job.id, path).map_err(|e| format!("{e}; {} keeps the backup until it is saved", self.host))
    }

    pub fn detect(&self, j: &Job) -> R<String> {
        self.simple(j, "detect", "", b"")
    }

    pub fn backup(&self, j: &Job, path: &Path) -> R<String> {
        let job = self.run(j, "backup", "", b"")?;
        let msg = job.done.clone().unwrap_or_else(|| Err("no outcome".into()))?;
        self.bring_back(&job, path)?;
        Ok(format!("{} {msg}", path.display()))
    }

    pub fn dump(&self, j: &Job, path: &Path, live: bool) -> R<String> {
        let job = self.run(j, "dump", &format!("&live={}", live as u8), b"")?;
        let msg = job.done.clone().unwrap_or_else(|| Err("no outcome".into()))?;
        self.bring_back(&job, path)?;
        Ok(format!("{} {msg}", path.display()))
    }

    /// Flashes the image there; its backup of the flash comes back next to the image, as here.
    pub fn flash(&self, j: &Job, image: &Path, backup: bool, verify: bool, reboot: bool) -> R<String> {
        let data = std::fs::read(image).map_err(|e| format!("{}: {e}", image.display()))?;
        if data.len() != crate::ops::FLASH_SIZE {
            return Err(format!("{} must be exactly 8 MiB (got {} bytes)", image.display(), data.len()));
        }
        let params = format!("&backup={}&verify={}&reboot={}", backup as u8, verify as u8, reboot as u8);
        let job = self.run(j, "flash", &params, &data)?;
        // The backup is brought back even when the flash failed after it.
        if job.output.is_some() {
            let mut name = image.file_stem().unwrap_or_default().to_os_string();
            name.push(format!("-backup-{}.bin", crate::ops::stamp()));
            let path = image.with_file_name(name);
            match self.bring_back(&job, &path) {
                Ok(sha) => j.log(format!("Backup saved: {} sha256 {sha}", path.display())),
                Err(e) => j.log(format!("The backup was not saved here: {e}")),
            }
        } else {
            let _ = self.dismiss(&job.id);
        }
        job.done.unwrap_or_else(|| Err("no outcome".into()))
    }

    pub fn update(&self, j: &Job, b: &Bundle, reboot: bool) -> R<String> {
        j.log(format!("Bundle {} ({} bytes)", b.build, b.data.len()));
        self.simple(j, "update", &format!("&reboot={}", reboot as u8), &b.data)
    }

    pub fn reboot_trial(&self, j: &Job) -> R<String> {
        self.simple(j, "reboot", "", b"")
    }

    pub fn cancel_trial(&self, j: &Job) -> R<String> {
        self.simple(j, "cancel", "", b"")
    }

    // ---- tuning ----

    /// A snapshot: a JPEG as the camera sent it. `auto` keeps the host's capture
    /// running between requests; `after` asks for a frame newer than that one.
    pub fn snapshot(&self, (w, h): (u32, u32), auto: bool, after: u64, diagnostic: bool) -> R<Snap> {
        let route = format!("snapshot.jpg?width={w}&height={h}&auto={}&after={after}&diagnostic={}", auto as u8, diagnostic as u8);
        let r = self.call("GET", &route, b"", Duration::from_secs(15))?;
        if !r.ok() {
            return Err(r.message());
        }
        let num = |k| r.get(k).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        let diagnostic = diagnostic.then(|| r.get(api::DIAGNOSTIC).map(crate::diag::from_hex).transpose());
        Ok(Snap { seq: num(api::SEQ), age: Duration::from_millis(num(api::AGE)), jpeg: r.body, diagnostic })
    }

    pub fn stop_snapshots(&self) {
        let _ = self.call("POST", "snapshot/stop", b"", Duration::from_secs(5));
    }

    /// Stops capturing and closes the controls there.
    pub fn close_tuning(&self) {
        let _ = self.call("POST", "tuning/close", b"", Duration::from_secs(5));
    }
}

pub struct Snap {
    /// None when not requested; otherwise a paired sample, unavailable, or a read error.
    pub diagnostic: Option<R<Option<crate::diag::Snapshot>>>,
    pub jpeg: Vec<u8>,
    pub seq: u64,
    pub age: Duration,
}

/// The controls there, for the Tuning tab's worker.
pub struct RemoteTuner(pub Arc<Remote>);

impl Tuner for RemoteTuner {
    fn diagnostic(&self) -> R<Option<Vec<u8>>> {
        let r = self.0.call("GET", "tuning/diagnostic", b"", QUICK)?;
        match r.status {
            204 | 404 => Ok(None), // Older firmware or daemon.
            200 => { crate::diag::Snapshot::parse(&r.body)?; Ok(Some(r.body)) },
            _ => Err(r.message()),
        }
    }

    fn sizes(&self) -> R<Vec<(u32, u32)>> {
        self.0.json("GET", "tuning/sizes", b"", QUICK)
    }

    fn load(&self) -> R<Vec<Option<Value>>> {
        self.0.json("GET", "tuning/controls", b"", Duration::from_secs(30))
    }

    fn set(&self, i: usize, value: i32) -> R<i32> {
        self.0.json("POST", &format!("tuning/set?i={i}&value={value}"), b"", QUICK)
    }

    fn get(&self, which: &[usize]) -> Vec<(usize, i32)> {
        let list: Vec<String> = which.iter().map(usize::to_string).collect();
        self.0.json("GET", &format!("tuning/get?i={}", list.join(",")), b"", QUICK).unwrap_or_default()
    }

    fn reset(&self) -> R<()> {
        self.0.call("POST", "tuning/reset", b"", Duration::from_secs(30)).and_then(|r| if r.ok() { Ok(()) } else { Err(r.message()) })
    }
}

/// The console there, as a port the Console tab reads and writes: reads wait
/// a moment for output, writes send at once.
pub struct RemotePort {
    remote: Arc<Remote>,
    next: u64,
    pending: Vec<u8>,
}

impl RemotePort {
    pub fn open(remote: Arc<Remote>) -> R<Self> {
        let r = remote.call("POST", "console/open", b"", QUICK)?;
        if !r.ok() {
            return Err(r.message());
        }
        Ok(Self { remote, next: 0, pending: Vec::new() })
    }
}

impl Read for RemotePort {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pending.is_empty() {
            let r = self
                .remote
                .call("GET", &format!("console?after={}&wait=300", self.next), b"", Duration::from_secs(10))
                .map_err(io::Error::other)?;
            if !r.ok() {
                return Err(io::Error::other(r.message()));
            }
            self.next = r.get(api::NEXT).and_then(|v| v.parse().ok()).unwrap_or(self.next);
            if r.body.is_empty() {
                // Closed there: the end, as for a port that went away.
                return if r.get(api::OPEN) == Some("0") { Ok(0) } else { Err(io::ErrorKind::TimedOut.into()) };
            }
            self.pending = r.body;
        }
        let n = self.pending.len().min(buf.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }
}

impl Write for RemotePort {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let r = self.remote.call("POST", "console/input", data, QUICK).map_err(io::Error::other)?;
        if !r.ok() {
            return Err(io::Error::other(r.message()));
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for RemotePort {
    fn drop(&mut self) {
        let _ = self.remote.call("POST", "console/close", b"", Duration::from_secs(3));
    }
}

/// The host remembered on this computer (the password changes each time the daemon starts).
pub mod saved {
    use std::path::PathBuf;

    fn file() -> Option<PathBuf> {
        let base = if cfg!(windows) {
            std::env::var_os("APPDATA").map(PathBuf::from)
        } else if cfg!(target_os = "macos") {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
        } else {
            std::env::var_os("XDG_CONFIG_HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        }?;
        Some(base.join("k1cam-flasher").join("remote"))
    }

    pub fn load() -> Option<String> {
        let text = std::fs::read_to_string(file()?).ok()?;
        crate::xu::record_value(&text, "host").map(str::to_string)
    }

    pub fn store(host: &str) -> Result<(), String> {
        let path = file().ok_or("no configuration directory")?;
        let dir = path.parent().unwrap();
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        let mut f = o.open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        std::io::Write::write_all(&mut f, format!("host={}\n", host.trim()).as_bytes())
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn forget() {
        if let Some(p) = file() {
            let _ = std::fs::remove_file(p);
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::daemon::tests::{daemon, lapse, restart, scratch, wrong, Fake, PASSWORD};
    use crate::daemon::{serve, Daemon};
    use std::net::TcpListener;
    use std::sync::mpsc::channel;

    fn host(name: &str) -> (String, Arc<Daemon>, std::path::PathBuf) {
        host_with_fake(name, Arc::new(Fake::default()))
    }

    fn host_with_fake(name: &str, fake: Arc<Fake>) -> (String, Arc<Daemon>, std::path::PathBuf) {
        let dir = scratch(name);
        let d = daemon(&dir, fake);
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let d2 = d.clone();
        std::thread::spawn(move || serve(l, d2));
        (addr, d, dir)
    }

    #[test]
    fn addresses_get_the_daemons_port() {
        assert_eq!(address("camera-host.lan"), "camera-host.lan:19249");
        assert_eq!(address(" 10.0.0.5:8000 "), "10.0.0.5:8000");
        assert_eq!(address("fe80::1"), "[fe80::1]:19249");
        assert_eq!(address("[fe80::1]:80"), "[fe80::1]:80");
    }

    #[test]
    fn a_wrong_password_or_a_taken_camera_says_why() {
        let (addr, _d, dir) = host("remote-refuse");
        let e = Remote::connect(&addr, "nope").err().unwrap();
        assert!(e.contains("refused the code"), "{e}");
        let first = Remote::connect(&addr, PASSWORD).unwrap();
        // Another window with the same user, name and address stays separate.
        let other = Remote::connect_parts(&addr);
        let e = other.new_session().map_err(|f| other.said(&f)).err().unwrap();
        assert!(e.contains("in use by another flasher"), "{e}");
        let id = first.session.lock().unwrap().clone();
        assert_eq!(first.new_session().unwrap().id, id);
        assert!(first.info().is_ok(), "resuming keeps the existing window's requests valid");
        first.disconnect();
        assert!(other.new_session().is_ok(), "a GUI that disconnects lets the camera go");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_new_password_on_the_host_leaves_the_connected_gui_alone() {
        let (addr, d, dir) = host("remote-new-password");
        let r = Remote::connect(&addr, "test42").unwrap();
        for n in 0..5 {
            assert!(Remote::connect(&addr, &format!("guess{n}")).err().unwrap().contains("refused the code"));
        }
        assert_ne!(d.password(), PASSWORD);
        r.refresh();
        assert_eq!(r.link(), Link::Up);
        assert!(r.info().is_ok());
        r.disconnect();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_restarted_daemon_is_asked_once_and_its_code_resumes_the_window() {
        let (addr, d, dir) = host("remote-restart");
        let r = Remote::connect(&addr, PASSWORD).unwrap();
        restart(&d, "NEW234");
        r.refresh();
        assert!(matches!(r.link(), Link::Refused(e) if e.contains("its terminal shows")), "{:?}", r.link());
        for _ in 0..10 {
            r.refresh();
            assert!(r.info().unwrap_err().contains("refused the code"));
        }
        assert_eq!((wrong(&d), d.password().as_str()), (1, "NEW234"), "the old code was sent once");
        assert!(r.reconnect(Some("typo")).unwrap_err().contains("refused the code"));
        r.reconnect(Some("new234")).unwrap();
        assert_eq!(r.link(), Link::Up);
        assert!(r.info().is_ok());
        r.disconnect();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn refused_or_taken_asks_nothing_of_the_host() {
        // Nothing listens there: an answer comes from here.
        let r = Remote::connect_parts("127.0.0.1:1");
        *r.link.lock().unwrap() = Link::Refused("refused".into());
        assert_eq!(r.info().unwrap_err(), "127.0.0.1:1 refused the code: enter the one its terminal shows now");
        *r.link.lock().unwrap() = Link::Taken("taken".into());
        assert_eq!(r.info().unwrap_err(), "taken");
        r.refresh();
        assert_eq!(r.link(), Link::Taken("taken".into()));
    }

    #[test]
    fn a_taken_camera_is_taken_back_only_when_asked() {
        let (addr, d, dir) = host("remote-taken");
        let r = Remote::connect(&addr, PASSWORD).unwrap();
        lapse(&d);
        let other = Remote::connect(&addr, PASSWORD).unwrap();
        r.refresh();
        assert!(matches!(r.link(), Link::Taken(e) if e.contains("in use by another flasher")), "{:?}", r.link());
        assert!(r.reconnect(None).unwrap_err().contains("in use by"));
        other.disconnect();
        r.refresh();
        assert!(matches!(r.link(), Link::Taken(_)), "not by itself");
        r.reconnect(None).unwrap();
        assert_eq!(r.link(), Link::Up);
        r.disconnect();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_job_followed_through_a_restart_carries_on_with_the_new_code() {
        let fake = Arc::new(Fake::default());
        let (open, gate) = channel();
        *fake.gate.lock().unwrap() = Some(gate);
        let dir = scratch("remote-job-restart");
        let d = daemon(&dir, fake);
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let d2 = d.clone();
        std::thread::spawn(move || serve(l, d2));
        let r = Remote::connect(&addr, PASSWORD).unwrap();
        let (tx, log) = channel();
        let j = Job::new(tx, Arc::new(AtomicBool::new(false)));
        let r2 = r.clone();
        let t = std::thread::spawn(move || r2.detect(&j));
        std::thread::sleep(Duration::from_millis(300));
        restart(&d, "NEW234");
        r.refresh();
        assert!(matches!(r.link(), Link::Refused(_)));
        std::thread::sleep(Duration::from_millis(1500));
        assert!(!t.is_finished(), "it waits for the code");
        r.reconnect(Some("NEW234")).unwrap();
        open.send(()).unwrap();
        assert_eq!(t.join().unwrap().unwrap(), "done");
        let said: Vec<String> = log
            .try_iter()
            .filter_map(|e| match e {
                crate::job::Event::Log(l) => Some(l),
                _ => None,
            })
            .collect();
        assert!(said.iter().any(|l| l.contains("refused the code") && l.contains("carries on there")), "{said:?}");
        assert!(said.iter().any(|l| l.starts_with("Connected to")), "{said:?}");
        r.disconnect();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn every_operation_through_the_daemon() {
        let fake = Arc::new(Fake::default());
        let (addr, _d, dir) = host_with_fake("remote-ops", fake.clone());
        let r = Remote::connect(&addr, PASSWORD).unwrap();
        assert!(r.status().camera);
        assert_eq!(r.info().unwrap(), "build=k1cam-test\nrunning_slot=b\n");
        let (tx, rx) = channel();
        let j = Job::new(tx, Arc::new(AtomicBool::new(false)));
        // A backup comes back into a new file, checked, and the host lets it go.
        let path = dir.join("backup.bin");
        assert!(r.backup(&j, &path).unwrap().ends_with(" done"));
        assert_eq!(std::fs::read(&path).unwrap(), b"backup bytes");
        assert!(r.jobs().unwrap().is_empty());
        // An existing file is never overwritten; the host keeps that backup.
        assert!(r.dump(&j, &path, false).unwrap_err().contains("keeps the backup"));
        let held = r.jobs().unwrap();
        assert_eq!(held.len(), 1);
        assert!(held[0].output.is_some());
        let again = dir.join("dump.bin");
        r.save_output(&held[0].id, &again).unwrap();
        assert!(r.jobs().unwrap().is_empty());
        // A flash sends the image and brings its backup back next to it.
        let image = dir.join("k1cam.bin");
        std::fs::write(&image, vec![0u8; crate::ops::FLASH_SIZE]).unwrap();
        assert_eq!(r.flash(&j, &image, true, true, true).unwrap(), "done");
        assert!(std::fs::read_dir(&dir).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with("k1cam-backup-")));
        let b = crate::bundle::load(Path::new("tests/data/golden.upd")).unwrap();
        assert_eq!(r.update(&j, &b, true).unwrap(), "done");
        assert_eq!(r.reboot_trial(&j).unwrap(), "done");
        // What the jobs said reached the job here.
        assert!(rx.try_iter().any(|e| matches!(e, crate::job::Event::Log(l) if l == "working")));
        // Tuning and snapshots.
        let t = RemoteTuner(r.clone());
        assert!(t.diagnostic().unwrap().is_none());
        *fake.diagnostic.lock().unwrap() = Some(crate::diag::tests::record());
        assert_eq!(t.diagnostic().unwrap().unwrap(), crate::diag::tests::record());
        assert_eq!(t.sizes().unwrap()[0], (1920, 1080));
        assert_eq!(t.set(0, 999).unwrap(), 255);
        assert_eq!(t.get(&[0, 1]), [(0, 7), (1, 7)]);
        let s = r.snapshot((640, 360), false, 0, true).unwrap();
        assert_eq!(&s.jpeg[..2], &[0xff, 0xd8]);
        assert_eq!(s.diagnostic.unwrap().unwrap().unwrap().exposure_us, 12345);
        let a = r.snapshot((640, 360), true, 0, false).unwrap();
        assert!(r.snapshot((640, 360), true, a.seq, false).unwrap().seq > a.seq);
        r.stop_snapshots();
        // The console, both ways.
        let mut p = RemotePort::open(r.clone()).unwrap();
        p.write_all(b"ls\r").unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(p.read(&mut buf).unwrap_err().kind(), io::ErrorKind::TimedOut);
        r.disconnect();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_job_carries_on_when_its_gui_goes_away() {
        let dir = scratch("remote-away");
        let fake = Arc::new(Fake::default());
        let (open, gate) = channel();
        *fake.gate.lock().unwrap() = Some(gate);
        let d = daemon(&dir, fake);
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        std::thread::spawn(move || serve(l, d));
        let r = Remote::connect(&addr, PASSWORD).unwrap();
        let (tx, _rx) = channel();
        let j = Job::new(tx, Arc::new(AtomicBool::new(false)));
        let r2 = r.clone();
        let path = dir.join("b.bin");
        let p2 = path.clone();
        let t = std::thread::spawn(move || r2.backup(&j, &p2));
        // The GUI quits mid-job; another (the same one, restarted) finds the job.
        std::thread::sleep(Duration::from_millis(300));
        r.disconnect();
        let back = Remote::connect(&addr, PASSWORD).unwrap();
        let jobs = back.jobs().unwrap();
        assert_eq!((jobs.len(), jobs[0].done.is_none()), (1, true));
        open.send(()).unwrap();
        let (tx, _rx) = channel();
        let j = Job::new(tx, Arc::new(AtomicBool::new(false)));
        let done = back.follow(&j, &jobs[0].id).unwrap();
        assert!(done.output.is_some());
        let _ = t.join();
        let _ = std::fs::remove_dir_all(dir);
    }

    impl Remote {
        /// A Remote with no session, for tests that take one by hand.
        fn connect_parts(addr: &str) -> Remote {
            Remote {
                host: addr.into(),
                addr: addr.into(),
                hello: Hello { api: api::API, version: String::new(), os: String::new(), arch: String::new() },
                name: String::new(),
                password: Mutex::new(PASSWORD.into()),
                client: client_name(),
                session: Mutex::new(String::new()),
                status: Mutex::new(Status::default()),
                link: Mutex::new(Link::Up),
                stop: AtomicBool::new(false),
            }
        }
    }
}
