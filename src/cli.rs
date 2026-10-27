// SPDX-License-Identifier: MIT
//! The headless flasher: the GUI's operations as commands, for scripts and for
//! the small Linux systems the camera gets plugged into (see the README for
//! static builds).

use crate::job::{Event, Job};
use crate::{bundle, cloner, ops, xu};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

const USAGE: &str = "\
usage: k1cam-flasher [COMMAND [ARGS]]

Without a command the GUI starts (in builds that have one).

A running camera (k1cam firmware, A/B layout):
  info                    print the camera's firmware information
  update FILE.upd [--no-reboot]
                          install a bundle into the inactive slot and
                          reboot into it as a trial
  reboot                  reboot into a staged trial
  cancel                  withdraw a staged trial
  dump FILE [--live]      save the whole flash (8 MiB)

With the cloner, on a camera in USB boot mode (boot ROM, a108:c309; see the README):
  detect                  identify the flash chip
  backup FILE             save the whole flash (8 MiB)
  flash FILE.bin --yes [--no-backup] [--no-verify] [--no-reboot]
                          replace the whole flash: U-Boot, both slots and
                          the settings (backs it up next to FILE.bin first)

  check FILE.upd          verify a bundle and print what it holds

  daemon                  serve the camera attached here to a GUI on another
                          computer (Linux, port 19249); prints the password
                          to connect with, a new one after 5 wrong attempts.
                          Ctrl-C waits for a running job.
";

/// Runs the command in `args` and returns the exit status, or None when `args`
/// are for the GUI.
pub fn run(args: &[String]) -> Option<i32> {
    let gui = cfg!(feature = "gui");
    let Some(cmd) = args.first().map(String::as_str) else {
        return if gui { None } else { Some(usage()) };
    };
    if gui && (cmd.starts_with("--tab=") || cmd.ends_with(".upd")) {
        return None;
    }
    if cmd == "daemon" {
        #[cfg(target_os = "linux")]
        return Some(crate::daemon::run(&args[1..]));
        #[cfg(not(target_os = "linux"))]
        {
            eprintln!("k1cam-flasher: the daemon runs on Linux only");
            return Some(2);
        }
    }
    let (opts, files): (Vec<&str>, Vec<&str>) = args[1..].iter().map(String::as_str).partition(|a| a.starts_with("--"));
    let allowed: &[&str] = match cmd {
        "update" => &["--no-reboot"],
        "dump" => &["--live"],
        "flash" => &["--yes", "--no-backup", "--no-verify", "--no-reboot"],
        _ => &[],
    };
    let nfiles = match cmd {
        "help" | "-h" | "--help" => {
            print!("{USAGE}");
            return Some(0);
        }
        "info" | "--info" | "reboot" | "cancel" | "detect" => 0,
        "update" | "dump" | "backup" | "flash" | "check" => 1,
        _ => return Some(usage()),
    };
    if files.len() != nfiles || opts.iter().any(|o| !allowed.contains(o)) {
        return Some(usage());
    }
    let has = |o: &str| opts.contains(&o);
    if cmd == "flash" && !has("--yes") {
        eprintln!("k1cam-flasher: flash erases the whole flash, U-Boot and the A/B layout included; add --yes to go ahead");
        return Some(2);
    }
    // Say plainly which device is missing (Windows has no A/B support to find one).
    let camera = matches!(cmd, "info" | "--info" | "update" | "reboot" | "cancel" | "dump");
    if camera && !cfg!(windows) && !xu::Camera::present() {
        return Some(report(Err("no camera running k1cam firmware is attached (USB f055:4b31 or 1d6b:0110)".into())));
    }
    if matches!(cmd, "detect" | "backup" | "flash") && !cloner::device_present() {
        return Some(report(Err("no camera in USB boot mode is attached (USB a108:c309); see the README".into())));
    }
    // One flasher at a time drives the camera here (a GUI, the daemon, another command).
    #[cfg(target_os = "linux")]
    let _lock = match cmd {
        "check" => None,
        _ => match crate::lock::acquire(format!("k1cam-flasher {cmd} (pid {})", std::process::id())) {
            Ok(l) => Some(l),
            Err(e) => return Some(report(Err(e))),
        },
    };
    let file = files.first().map(PathBuf::from).unwrap_or_default();
    Some(match cmd {
        "info" | "--info" => report(xu::Camera::open().and_then(|mut c| c.info())),
        "check" => report(bundle::load(&file).map(|b| describe(&b))),
        "update" => match bundle::load(&file) {
            Ok(b) => {
                let reboot = !has("--no-reboot");
                run_job(move |j| ops::update(j, &b, reboot))
            }
            Err(e) => report(Err(e)),
        },
        "reboot" => run_job(ops::reboot_trial),
        "cancel" => run_job(ops::cancel_trial),
        "dump" => match new_file(&file) {
            Ok(()) => {
                let live = has("--live");
                run_job(move |j| ops::dump(j, &file, live))
            }
            Err(e) => report(Err(e)),
        },
        "detect" => run_job(|j| {
            let c = cloner::Cloner::probe(j)?;
            Ok(format!("{} ({} KiB, JEDEC {:06x})", c.name, c.size / 1024, c.jedec))
        }),
        "backup" => match new_file(&file) {
            Ok(()) => run_job(move |j| ops::backup(j, &file)),
            Err(e) => report(Err(e)),
        },
        "flash" => {
            let (backup, verify, reboot) = (!has("--no-backup"), !has("--no-verify"), !has("--no-reboot"));
            run_job(move |j| ops::flash(j, &file, backup, verify, reboot))
        }
        _ => unreachable!(),
    })
}

fn usage() -> i32 {
    eprint!("{USAGE}");
    2
}

fn report(r: Result<String, String>) -> i32 {
    match r {
        Ok(m) => {
            println!("{}", m.trim_end());
            0
        }
        Err(e) => {
            eprintln!("k1cam-flasher: {e}");
            1
        }
    }
}

/// Output files are never overwritten: an old backup may be the only good one.
fn new_file(path: &Path) -> Result<(), String> {
    if path.exists() {
        return Err(format!("{} exists; not overwriting it", path.display()));
    }
    Ok(())
}

/// A bundle's identity, `key=value` like `k1cam-mkupd --check`, and its
/// release information as `info:` lines.
fn describe(b: &bundle::Bundle) -> String {
    let mut s = format!(
        "build={}\nboard={}\nlayout={}\nmin_boot_compat={}\nkernel_length={}\nrootfs_length={}\nsha256={}\n",
        b.build,
        b.board,
        b.layout,
        b.min_boot,
        b.kernel_len,
        b.rootfs_len,
        xu::hex(&b.sha256)
    );
    for line in b.release.as_deref().unwrap_or("").lines() {
        s.push_str(&format!("info:{line}\n"));
    }
    s
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Ctrl-C asks the operation to stop (it then cleans up, e.g. ends the camera's
/// session); a second one quits at once.
#[cfg(unix)]
fn catch_interrupt() {
    extern "C" fn handler(_: libc::c_int) {
        if INTERRUPTED.swap(true, Ordering::Relaxed) {
            unsafe { libc::_exit(130) };
        }
    }
    unsafe { libc::signal(libc::SIGINT, handler as extern "C" fn(libc::c_int) as libc::sighandler_t) };
}

#[cfg(not(unix))]
fn catch_interrupt() {}

/// Progress on stderr: one line redrawn on a terminal, every 10% otherwise.
struct Progress {
    tty: bool,
    pct: Option<u32>,
}

impl Progress {
    fn set(&mut self, p: f32) {
        let pct = (p.clamp(0.0, 1.0) * 100.0) as u32;
        if self.tty {
            if self.pct != Some(pct) {
                eprint!("\r{pct:3}%");
                let _ = std::io::stderr().flush();
            }
        } else if self.pct.map(|old| old / 10) != Some(pct / 10) {
            eprintln!("{pct}%");
        }
        self.pct = Some(pct);
    }

    fn line(&mut self, s: &str) {
        match self.pct {
            Some(pct) if self.tty => eprint!("\r    \r{s}\n{pct:3}%"),
            _ => eprintln!("{s}"),
        }
    }

    fn end(&mut self) {
        if self.tty && self.pct.is_some() {
            eprintln!();
        }
    }
}

fn run_job<F>(f: F) -> i32
where
    F: FnOnce(&Job) -> Result<String, String> + Send + 'static,
{
    let (tx, rx) = channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let job = Job::new(tx.clone(), cancel.clone());
    catch_interrupt();
    std::thread::spawn(move || {
        let r = f(&job);
        let _ = tx.send(Event::Done(r));
    });
    let mut progress = Progress { tty: std::io::stderr().is_terminal(), pct: None };
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Log(s)) => progress.line(&s),
            Ok(Event::Progress(p)) => progress.set(p),
            Ok(Event::Done(r)) => {
                progress.end();
                return report(r);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return 1,
        }
        if INTERRUPTED.load(Ordering::Relaxed) && !cancel.swap(true, Ordering::Relaxed) {
            progress.line("interrupted: stopping (Ctrl-C again quits at once)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_args(args: &[&str]) -> Option<i32> {
        run(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn arguments() {
        // Mistakes never reach a device.
        assert_eq!(run_args(&["frobnicate"]), Some(2));
        assert_eq!(run_args(&["backup"]), Some(2));
        assert_eq!(run_args(&["backup", "a", "b"]), Some(2));
        assert_eq!(run_args(&["dump", "x", "--force"]), Some(2));
        assert_eq!(run_args(&["update", "x.upd", "--live"]), Some(2));
        assert_eq!(run_args(&["flash", "x.bin"]), Some(2)); // no --yes
        assert_eq!(run_args(&["help"]), Some(0));
        // Existing output files are refused before anything is opened.
        assert_eq!(run_args(&["backup", "Cargo.toml"]), Some(1));
        assert_eq!(run_args(&["dump", "Cargo.toml"]), Some(1));
        assert_eq!(run_args(&["check", "tests/data/golden.upd"]), Some(0));
        assert_eq!(run_args(&["check", "Cargo.toml"]), Some(1));
        if cfg!(feature = "gui") {
            assert_eq!(run_args(&[]), None);
            assert_eq!(run_args(&["--tab=help"]), None);
            assert_eq!(run_args(&["k1cam.upd"]), None);
        } else {
            assert_eq!(run_args(&[]), Some(2));
            assert_eq!(run_args(&["k1cam.upd"]), Some(2));
        }
    }

    #[test]
    fn describes_a_bundle() {
        let b = bundle::load(Path::new("tests/data/golden.upd")).unwrap();
        let d = describe(&b);
        assert!(d.starts_with("build=k1cam-test-1\nboard=k1cam-t31l-gc2083\nlayout=k1cam-ab-8m-v1\nmin_boot_compat=1\n"));
        assert!(d.contains("kernel_length=5064\nrootfs_length=12288\n"));
        assert!(d.ends_with("info:NAME=k1cam\ninfo:BUILD_ID=\"k1cam+test, 2026-01-01 00:00:00 +0000\"\n"));
    }
}
