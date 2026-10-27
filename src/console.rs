// SPDX-License-Identifier: MIT
//! The camera's console on its USB serial port (CDC-ACM, found by the k1cam USB
//! ids). Connect opens it on request; type a line, Enter sends it.

use eframe::egui;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const MAX_LINES: usize = 5000;

/// Where the parser is in an escape sequence.
enum Seq {
    Text,
    Escape,
    Csi(Vec<u8>),
    Osc,
    OscEscape,
}

/// What a dumb terminal would show: lines of text, with carriage return,
/// backspace, tab and line erase applied, and other escape sequences (colours,
/// cursor queries) dropped.
pub struct Term {
    lines: VecDeque<String>,
    cur: Vec<char>,
    col: usize,
    seq: Seq,
    utf8: Vec<u8>,
}

impl Term {
    pub fn new() -> Self {
        Self { lines: VecDeque::new(), cur: Vec::new(), col: 0, seq: Seq::Text, utf8: Vec::new() }
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.cur.clear();
        self.col = 0;
    }

    /// Lines so far, the unfinished one last.
    pub fn rows(&self) -> usize {
        self.lines.len() + 1
    }

    pub fn row(&self, i: usize) -> String {
        match self.lines.get(i) {
            Some(l) => l.clone(),
            None => self.cur.iter().collect(),
        }
    }

    pub fn text(&self) -> String {
        let mut s: String = self.lines.iter().flat_map(|l| [l.as_str(), "\n"]).collect();
        s.extend(self.cur.iter());
        s
    }

    fn put(&mut self, c: char) {
        if self.col < self.cur.len() {
            self.cur[self.col] = c;
        } else {
            self.cur.resize(self.col, ' ');
            self.cur.push(c);
        }
        self.col += 1;
    }

    fn newline(&mut self) {
        self.lines.push_back(self.cur.drain(..).collect());
        if self.lines.len() > MAX_LINES {
            self.lines.pop_front();
        }
        self.col = 0;
    }

    fn csi(&mut self, fin: u8, params: &[u8]) {
        let n = std::str::from_utf8(params).ok().and_then(|p| p.split(';').next()?.parse::<usize>().ok());
        match fin {
            b'K' => match n.unwrap_or(0) {
                0 => self.cur.truncate(self.col),
                1 => self.cur.iter_mut().take(self.col).for_each(|c| *c = ' '),
                _ => self.cur.clear(),
            },
            b'C' => self.col += n.unwrap_or(1).max(1),
            b'D' => self.col = self.col.saturating_sub(n.unwrap_or(1).max(1)),
            b'G' => self.col = n.unwrap_or(1).saturating_sub(1),
            b'J' if n == Some(2) || n == Some(3) => self.clear(),
            _ => {}
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            match &mut self.seq {
                Seq::Text => {}
                Seq::Escape => {
                    self.seq = match b {
                        b'[' => Seq::Csi(Vec::new()),
                        b']' => Seq::Osc,
                        _ => Seq::Text,
                    };
                    continue;
                }
                Seq::Csi(p) => {
                    if (0x40..=0x7e).contains(&b) {
                        let params = std::mem::take(p);
                        self.seq = Seq::Text;
                        self.csi(b, &params);
                    } else if p.len() < 32 {
                        p.push(b);
                    }
                    continue;
                }
                Seq::Osc => {
                    match b {
                        0x07 => self.seq = Seq::Text,
                        0x1b => self.seq = Seq::OscEscape,
                        _ => {}
                    }
                    continue;
                }
                Seq::OscEscape => {
                    self.seq = if b == b'\\' { Seq::Text } else { Seq::Osc };
                    continue;
                }
            }
            if b >= 0x80 || !self.utf8.is_empty() {
                self.utf8.push(b);
                match std::str::from_utf8(&self.utf8) {
                    Ok(s) => {
                        let c = s.chars().next().unwrap_or('\u{fffd}');
                        self.utf8.clear();
                        self.put(c);
                    }
                    Err(e) if e.error_len().is_none() && self.utf8.len() < 4 => {}
                    Err(_) => {
                        self.utf8.clear();
                        self.put('\u{fffd}');
                    }
                }
                continue;
            }
            match b {
                0x1b => self.seq = Seq::Escape,
                b'\r' => self.col = 0,
                b'\n' => self.newline(),
                0x08 => self.col = self.col.saturating_sub(1),
                b'\t' => {
                    for _ in 0..8 - self.col % 8 {
                        self.put(' ');
                    }
                }
                0x20..=0x7e => self.put(b as char),
                _ => {}
            }
        }
    }
}

enum Msg {
    Opened,
    Data(Vec<u8>),
    Failed(String),
}

/// A connection, run by a thread of its own: it opens the port, reads and
/// writes it, and closes it. A device that stops answering can hold an open or
/// a close for seconds, which must not freeze the window.
struct Conn {
    name: String,
    out: Sender<Vec<u8>>,
    rx: Receiver<Msg>,
    stop: Arc<AtomicBool>,
    /// The port is open (until then, it is being opened).
    open: bool,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// The connection thread: what arrives goes to `tx`, what comes from `out` to
/// the port, until `stop`.
fn run_port(name: String, out: Receiver<Vec<u8>>, tx: Sender<Msg>, stop: Arc<AtomicBool>, repaint: impl Fn()) {
    let mut port = match serialport::new(&name, 115200).timeout(Duration::from_millis(50)).open() {
        Ok(p) => p,
        Err(e) => {
            let _ = tx.send(Msg::Failed(format!("{name}: {e}")));
            repaint();
            return;
        }
    };
    let _ = tx.send(Msg::Opened);
    repaint();
    let mut buf = [0u8; 4096];
    while !stop.load(Ordering::Relaxed) {
        for bytes in out.try_iter() {
            if let Err(e) = port.write_all(&bytes) {
                let _ = tx.send(Msg::Failed(format!("{name}: {e}")));
                repaint();
                return;
            }
        }
        match port.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if tx.send(Msg::Data(buf[..n].to_vec())).is_err() {
                    break;
                }
                repaint();
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted) => {}
            Err(_) => break,
        }
    }
    repaint();
}

pub struct Console {
    own: crate::gui::Own,
    port: Option<String>,
    conn: Option<Conn>,
    /// The old port must finish closing before another connection or a job.
    closing: Option<JoinHandle<()>>,
    note: String,
    term: Term,
    input: String,
    history: Vec<String>,
    browse: Option<usize>,
    scanned: Option<Instant>,
}

/// The serial port of an attached k1cam camera, if any.
fn find_port() -> Option<String> {
    serialport::available_ports().ok()?.into_iter().find_map(|p| match p.port_type {
        serialport::SerialPortType::UsbPort(u) if crate::xu::CAMERA_IDS.contains(&(u.vid, u.pid)) => Some(p.port_name),
        _ => None,
    })
}

impl Console {
    pub fn new(own: crate::gui::Own) -> Self {
        Self {
            own,
            port: None,
            conn: None,
            closing: None,
            note: "Waiting for the camera's console".into(),
            term: Term::new(),
            input: String::new(),
            history: Vec::new(),
            browse: None,
            scanned: None,
        }
    }

    /// The camera's console port is there, open or not.
    pub fn found(&self) -> bool {
        self.port.is_some()
    }

    /// Open, opening, or still closing.
    pub fn in_use(&self) -> bool {
        self.conn.is_some() || self.closing.is_some()
    }

    pub fn connected(&self) -> bool {
        self.conn.as_ref().is_some_and(|c| c.open)
    }

    pub fn disconnect(&mut self) {
        if let Some(mut c) = self.conn.take() {
            c.stop.store(true, Ordering::Relaxed);
            self.closing = c.thread.take();
            self.note = "Disconnected".into();
        }
    }

    /// The job waits for the serial port to close before it starts.
    pub fn stop(&mut self) -> Vec<JoinHandle<()>> {
        self.disconnect();
        self.closing.take().into_iter().collect()
    }

    /// Takes in what arrived and discovers ports without opening them.
    pub fn poll(&mut self) {
        if self.closing.as_ref().is_some_and(|t| t.is_finished()) {
            self.closing = None;
        }
        if let Some(c) = &mut self.conn {
            let (mut failed, mut closed) = (None, false);
            loop {
                match c.rx.try_recv() {
                    Ok(Msg::Data(b)) => self.term.feed(&b),
                    Ok(Msg::Opened) => {
                        c.open = true;
                        self.note = format!("Connected to {}. Other programs can't open it until you disconnect.", c.name);
                    }
                    Ok(Msg::Failed(e)) => failed = Some(e),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        closed = true;
                        break;
                    }
                }
            }
            if let Some(e) = failed {
                self.disconnect();
                self.note = e;
            } else if closed {
                let name = c.name.clone();
                self.disconnect();
                self.note = format!("{name} closed; press Connect to reconnect");
            }
        }
        if self.scanned.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
            return;
        }
        self.scanned = Some(Instant::now());
        let found = find_port();
        if found != self.port {
            if self.conn.as_ref().is_some_and(|c| Some(&c.name) != found.as_ref()) {
                self.disconnect();
            }
            if found.is_none() {
                self.note = "Waiting for the camera's console".into();
            } else if self.conn.is_none() {
                self.note = "Connect to open the camera's console".into();
            }
            self.port = found;
        }
    }

    /// Opens the port on request, once the previous connection has closed.
    fn connect(&mut self, name: &str, ctx: &egui::Context) {
        if self.conn.is_some() || self.closing.is_some() {
            return;
        }
        #[cfg(target_os = "linux")]
        {
            let users = crate::linux::device_users(name);
            if !users.is_empty() {
                self.note = format!("{name} is open in {}; not connecting so that it keeps its output", users.join(", "));
                return;
            }
        }
        if let Err(e) = self.own.hold() {
            self.note = e;
            return;
        }
        let (tx, rx) = channel();
        let (out, out_rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let (name2, stop2, ctx) = (name.to_string(), stop.clone(), ctx.clone());
        let thread = std::thread::spawn(move || run_port(name2, out_rx, tx, stop2, || ctx.request_repaint()));
        self.note = format!("Connecting to {name}…");
        self.conn = Some(Conn { name: name.into(), out, rx, stop, open: false, thread: Some(thread) });
    }

    fn send(&mut self, bytes: &[u8]) {
        if let Some(c) = &self.conn {
            let _ = c.out.send(bytes.to_vec());
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let connected = self.connected();
        ui.horizontal(|ui| {
            if self.conn.is_some() {
                if ui.button("Disconnect").clicked() {
                    self.disconnect();
                }
            } else {
                // There before the port is, so that the tab looks the same either way.
                let r = ui.add_enabled(self.port.is_some() && self.closing.is_none(), egui::Button::new("Connect"));
                let r = r.on_disabled_hover_text("No camera console is attached");
                if let Some(name) = self.port.clone().filter(|_| r.clicked()) {
                    self.connect(&name, ui.ctx());
                }
            }
            if ui.add_enabled(connected, egui::Button::new("Ctrl-C")).on_hover_text("Stops what runs in the foreground, e.g. logf").clicked() {
                self.send(&[3]);
            }
            if ui.button("Clear").clicked() {
                self.term.clear();
            }
            if ui.button("Copy all").clicked() {
                ui.ctx().copy_text(self.term.text());
            }
        });
        let color = if connected {
            crate::gui::ok_color(ui)
        } else if self.conn.is_some() || self.port.is_some() {
            crate::gui::warn_color(ui)
        } else {
            crate::gui::weak_color(ui)
        };
        crate::gui::status(ui, color, self.note.as_str());
        let row = ui.text_style_height(&egui::TextStyle::Monospace);
        let input = ui.spacing().interact_size.y + ui.spacing().item_spacing.y * 2.0;
        egui::ScrollArea::both()
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .max_height((ui.available_height() - input).max(row))
            .show_rows(ui, row, self.term.rows(), |ui, rows| {
                for i in rows {
                    ui.add(egui::Label::new(egui::RichText::new(self.term.row(i)).monospace()).wrap_mode(egui::TextWrapMode::Extend));
                }
            });
        let edit = egui::TextEdit::singleline(&mut self.input)
            .font(egui::TextStyle::Monospace)
            .desired_width(f32::INFINITY)
            .hint_text("Enter sends the line; an empty one wakes the console's prompt");
        let r = ui.add_enabled(connected, edit);
        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            let line = std::mem::take(&mut self.input);
            self.send(format!("{line}\r").as_bytes());
            if !line.is_empty() && self.history.last() != Some(&line) {
                self.history.push(line);
            }
            self.browse = None;
            r.request_focus();
        } else if r.has_focus() {
            let (up, down) = ui.input(|i| (i.key_pressed(egui::Key::ArrowUp), i.key_pressed(egui::Key::ArrowDown)));
            if up && !self.history.is_empty() {
                let i = self.browse.map_or(self.history.len() - 1, |i| i.saturating_sub(1));
                self.browse = Some(i);
                self.input = self.history[i].clone();
            } else if down {
                match self.browse {
                    Some(i) if i + 1 < self.history.len() => {
                        self.browse = Some(i + 1);
                        self.input = self.history[i + 1].clone();
                    }
                    Some(_) => {
                        self.browse = None;
                        self.input.clear();
                    }
                    None => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(chunks: &[&[u8]]) -> String {
        let mut t = Term::new();
        chunks.iter().for_each(|c| t.feed(c));
        t.text()
    }

    #[test]
    fn line_discipline() {
        assert_eq!(shown(&[b"a\r\nb\r\n"]), "a\nb\n");
        // The shell's echo of a backspace, and line redraws.
        assert_eq!(shown(&[b"lsx\x08 \x08\r\n"]), "ls \n");
        assert_eq!(shown(&[b"abcdef\r\x1b[Kxy"]), "xy");
        assert_eq!(shown(&[b"abcdef\x1b[3D\x1b[K!"]), "abc!");
        assert_eq!(shown(&[b"abc\rX"]), "Xbc");
        assert_eq!(shown(&[b"a\tb"]), "a       b");
        // Colours, cursor queries and titles leave nothing behind.
        assert_eq!(shown(&[b"\x1b[1;34m~ #\x1b[0m \x1b[6n\x1b]0;cam\x07$"]), "~ # $");
        // Sequences and UTF-8 split across reads.
        assert_eq!(shown(&[b"\x1b[", b"1;3", b"1mok\xc3", b"\xa9"]), "ok\u{e9}");
        assert_eq!(shown(&[b"\xff!"]), "\u{fffd}!");
        // `clear`
        assert_eq!(shown(&[b"old\r\n\x1b[H\x1b[2Jnew"]), "new");
    }

    /// The connection thread on a pseudo-terminal: it opens it, passes data
    /// both ways, and lets go when told to stop.
    #[test]
    #[cfg(target_os = "linux")]
    fn connection_thread() {
        use std::os::fd::{FromRawFd, OwnedFd};
        let (mut master, mut slave) = (0, 0);
        assert_eq!(unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), std::ptr::null()) }, 0);
        let name = std::fs::read_link(format!("/proc/self/fd/{slave}")).unwrap().to_string_lossy().into_owned();
        let mut master = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(master) });
        let (tx, rx) = channel();
        let (out, out_rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let t = std::thread::spawn(move || run_port(name, out_rx, tx, stop2, || {}));
        assert!(matches!(rx.recv_timeout(Duration::from_secs(5)), Ok(Msg::Opened)));
        master.write_all(b"# ").unwrap();
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Msg::Data(d)) => assert_eq!(d, b"# "),
            _ => panic!("no data from the port"),
        }
        out.send(b"ls\r".to_vec()).unwrap();
        let mut got = [0u8; 3];
        master.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"ls\r");
        stop.store(true, Ordering::Relaxed);
        t.join().unwrap();
        unsafe { libc::close(slave) };
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn stopping_the_console_releases_the_port_without_reconnecting() {
        use std::os::fd::{FromRawFd, OwnedFd};
        let (mut master, mut slave) = (0, 0);
        assert_eq!(unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), std::ptr::null()) }, 0);
        let name = std::fs::read_link(format!("/proc/self/fd/{slave}")).unwrap().to_string_lossy().into_owned();
        let (_master, _slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        let mut console = Console::new(crate::gui::Own::default());
        console.port = Some(name.clone());
        console.scanned = Some(Instant::now());
        console.poll();
        assert!(console.conn.is_none());
        console.connect(&name, &egui::Context::default());
        assert!(matches!(console.conn.as_ref().unwrap().rx.recv_timeout(Duration::from_secs(5)), Ok(Msg::Opened)));
        let threads = console.stop();
        assert_eq!(threads.len(), 1);
        for t in threads {
            t.join().unwrap();
        }
        let _port = serialport::new(&name, 115200).open().expect("the console must have released its exclusive port");
        console.poll();
        assert!(console.conn.is_none(), "a completed job must not reconnect the console");
    }

    #[test]
    fn keeps_the_last_lines() {
        let mut t = Term::new();
        for i in 0..MAX_LINES + 10 {
            t.feed(format!("{i}\r\n").as_bytes());
        }
        assert_eq!(t.rows(), MAX_LINES + 1);
        assert_eq!(t.row(0), "10");
    }
}
