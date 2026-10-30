# k1cam-flasher

A small, single-binary GUI to back up, flash and update the Creality K1 camera (Ingenic T31L, 8 MiB SPI NOR) on Linux, Windows and macOS, and the same operations as a command line, which also builds as a small static binary for the Linux boards a camera gets plugged into (see [Command line](#command-line) and [Static builds](#static-builds)). The camera can also be plugged into another Linux computer running `k1cam-flasher daemon`, and used from the GUI over the network (see [A camera on another computer](#a-camera-on-another-computer)).

- **Backup & Flash (cloner)**: full read and full flash with the Ingenic cloner protocol, through the boot ROM's USB boot mode, ported from [thingino-dfu](https://github.com/gtxaspec/thingino-dfu) (T31 family only), with read-back verification.
- **A/B Update (k1cam)**: dump, update the inactive slot, reboot into the trial or cancel it, over the k1cam UVC extension unit protocol (same as `uvcdctl firmware`). On Linux it uses the uvcvideo XU ioctl on `/dev/videoN` (needs access to the video node, e.g. the `video` group); on macOS it uses libusb (untested). Not available on Windows itself, only through [another computer](#a-camera-on-another-computer). The tab shows what the connected camera runs (build, kernel commit, slots, boot state) next to what the chosen `k1cam.upd` holds (build, commit, kernel, sizes against the slot sizes, and its os-release, after checking its digests), and whether the camera would take it. `k1cam-flasher FILE.upd` opens the A/B tab with that bundle.
- **Tuning** (Linux, or [another computer](#a-camera-on-another-computer) with snapshots): press *Connect* for the camera's picture, live (MJPEG at a size and rate you choose), as large as the window allows with every image control still in view: picture, exposure, white balance, noise, view and encoder settings, as `uvcdctl` names them, flowing down columns beside and below the picture. The camera keeps every change itself; an active control that differs from its default stands out, clicking its name puts the default back, and *Factory defaults* resets them all. Exposure time and white balance temperature have an *auto* check box on their row, and while it is on the slider follows what the camera chooses. *Overlays* on the picture show where the camera meters exposure and how bright each of its zones is, clipped highlights and crushed shadows, a histogram and a framing grid, and the picture reads out the pixel under the pointer. *Snapshot* saves the frame on screen as the camera sent it (JPEG). The preview runs only while the tab is shown; *Pause* frees the video, and *Disconnect* closes tuning entirely.
- **Console**: the camera's USB serial console (CDC-ACM, same USB ids). *Connect* works once the console is detected; press it to open the console. It doesn't connect while another program has the port open (Linux), so that program keeps its output; while connected, other programs can't open the port until you press *Disconnect*. Type a line and press Enter (an empty line wakes the prompt), *Ctrl-C* stops what runs in the foreground. After a reboot or unplug, press *Connect* again.
- **Help** (*Help*, top right): forcing USB boot mode (see below), drivers and permissions, and using a camera on another computer.
- **Connection menu** (left of *Help*): whether the camera is plugged into this computer or into another one running the daemon; see [below](#a-camera-on-another-computer).

The status bar under the tabs shows whether the open tab's device is attached (green when it is) and follows a running operation (blue): its progress, what it last said, the time taken and left, and *Cancel*. Beside Backup & Flash and A/B Update, the log shows each operation under a heading with when it started, how long it took and how it ended; it opens by itself when one starts, and folded away to a strip it counts the operations finished since, in red if one failed. A tab's lines are all there from the start, so nothing moves as results come in. Dropping a `k1cam.upd` on the window opens it on the A/B tab, a `.bin` image on Backup & Flash. Closing the window while an operation runs asks first.

| A/B Update | Tuning | Console |
|---|---|---|
| ![A/B Update tab](docs/screenshot-update.png) | ![Tuning tab](docs/screenshot-tuning.png) | ![Console tab](docs/screenshot-console.png) |
| **Backup & Flash** | **Help** | |
| ![Backup & Flash tab](docs/screenshot-cloner.png) | ![Help tab](docs/screenshot-help.png) | |

> Take and check a backup before relying on it.

ISP diagnostics are available locally and over the remote link. With the
new firmware/kernel, Metering uses the ISP's actual weights, crop and zone
levels, and Histogram shows the linear whole-sensor histogram plus highlight
guard markers. The legend reports exposure, gain, AE target/mean and accepted
compensation. Older firmware keeps labelled picture estimates. See
[the diagnostic-channel design](docs/diagnostics.md) for the protocol and rollout.

## Download

The newest build of `main`, replaced on every push, is the [nightly release](https://github.com/itzexor/k1cam-flasher/releases/tag/nightly):

| Platform | GUI | Command line (static) |
|---|---|---|
| Linux x86_64 | [k1cam-flasher-linux-x86_64](https://github.com/itzexor/k1cam-flasher/releases/download/nightly/k1cam-flasher-linux-x86_64) | [k1cam-flasher-cli-linux-x86_64](https://github.com/itzexor/k1cam-flasher/releases/download/nightly/k1cam-flasher-cli-linux-x86_64) |
| Linux aarch64 (64-bit Raspberry Pi OS) | | [k1cam-flasher-cli-linux-aarch64](https://github.com/itzexor/k1cam-flasher/releases/download/nightly/k1cam-flasher-cli-linux-aarch64) |
| Linux armv7 (32-bit Raspberry Pi OS) | | [k1cam-flasher-cli-linux-armv7](https://github.com/itzexor/k1cam-flasher/releases/download/nightly/k1cam-flasher-cli-linux-armv7) |
| Linux armv6 (Pi Zero, Pi 1) | | [k1cam-flasher-cli-linux-armv6](https://github.com/itzexor/k1cam-flasher/releases/download/nightly/k1cam-flasher-cli-linux-armv6) |
| Linux MIPS, little-endian (MIPS32r2 or later) | | [k1cam-flasher-cli-linux-mipsel](https://github.com/itzexor/k1cam-flasher/releases/download/nightly/k1cam-flasher-cli-linux-mipsel) |
| Windows x86_64 | [k1cam-flasher-windows-x86_64.exe](https://github.com/itzexor/k1cam-flasher/releases/download/nightly/k1cam-flasher-windows-x86_64.exe) | |
| macOS Apple Silicon | [k1cam-flasher-macos-arm64](https://github.com/itzexor/k1cam-flasher/releases/download/nightly/k1cam-flasher-macos-arm64) | |
| macOS Intel | [k1cam-flasher-macos-x86_64](https://github.com/itzexor/k1cam-flasher/releases/download/nightly/k1cam-flasher-macos-x86_64) | |

[SHA256SUMS](https://github.com/itzexor/k1cam-flasher/releases/download/nightly/SHA256SUMS) has their checksums. On Linux and macOS, `chmod +x` the file; macOS also wants `xattr -d com.apple.quarantine` on it before it runs.

## Getting the camera into USB boot mode

The bootrom only listens on USB when the SoC cannot boot from the NOR, so you short two NOR pins while it powers up.

![Pins to short](docs/short-nor-pins.jpg)

1. Unplug the camera from power and USB.
2. With tweezers, hold together the two right-most pins of the top-right row of pins beside the XMC 25QH64DH NOR chip (marked in the picture).
3. Keep holding while you plug in USB so the camera powers up.
4. After about a second release the short. `lsusb` shows `a108:c309` and the app shows a green status.

The short only matters at power-up. If the device does not appear, power-cycle and retry. The same text is in the app's help (*Help*).

## Driver setup

Running k1cam firmware uses `f055:4b31` (unofficial FOSS community VID,
`4b31` = "K1"), manufacturer `k1cam`, product `K1 Camera`. This is a project
convention, not an exclusive allocation. Earlier `1d6b:0110` firmware is
also recognized so updates and rollback can cross the ID change. The boot
ROM's cloner identity remains `a108:c309`. See the
[firmware USB identity notes](https://github.com/itzexor/k1cam-firmware/blob/k1cam/USB-IDENTITY.md)
for the rationale and collision checks.

- **Linux**: `sudo cp udev/99-k1cam-flasher.rules /etc/udev/rules.d/ && sudo udevadm control --reload && sudo udevadm trigger`. A/B Update and Tuning need access to the camera's `/dev/video*` node (the `video` group), the console to `/dev/ttyACM*` (the `dialout` group, `uucp` on Arch).
- **Windows**: bind WinUSB to `a108:c309` with [Zadig](https://zadig.akeo.ie/) while the camera is in USB boot mode.
- **macOS**: nothing to install.

## Using it

Attaching a camera or opening a tab does not open its console, start video,
or read its controls. On A/B Update, *Connect* reads the firmware information
and *Refresh* reads it again. Choosing a valid `.upd` also reads an attached
camera's information for comparison. Firmware operations disconnect tuning
and the console and wait for their device access to finish before starting;
press *Connect* again to use them afterward. While an operation runs, file
loading (including drops) and camera controls are locked; the tabs, the log
and Cancel remain available.

- Backup: *Detect flash chip* checks the link, *Back up flash...* saves the 8 MiB image and shows its SHA-256. Backups are named for the time in UTC (`k1cam-backup-20261020T073812Z.bin`).
- Flash: choose `k1cam.bin` (exactly 8 MiB; the tab checks the size when you choose it), optionally back up first, tick the confirmation, press *Flash*. This replaces U-Boot and installs the A/B layout once. After that, update with `k1cam.upd` on the A/B tab.
- Update: choose `k1cam.upd` and compare it with what the camera runs; the bundle is read again when the file changes on disk, and what is shown is what gets installed. The camera is sent the bundle only, not the release information after it, so cameras on firmware from before that section was added take these files too.
- Progress shows in the status bar with the time taken and, for long steps, the time left; each operation's outcome shows beside the button that started it and in the log, which *Copy* and *Save…* take as text.

## Command line

Every operation also runs without the GUI; `k1cam-flasher help` lists them:

```
k1cam-flasher info                      # a running camera's firmware information
k1cam-flasher update k1cam.upd          # install into the inactive slot, reboot into the trial
k1cam-flasher reboot | cancel           # start or withdraw a staged trial
k1cam-flasher dump backup.bin [--live]  # the whole flash from a running camera
k1cam-flasher detect                    # cloner: identify the flash chip
k1cam-flasher backup backup.bin         # cloner: read the whole flash
k1cam-flasher flash k1cam.bin --yes     # cloner: replace the whole flash
k1cam-flasher check k1cam.upd           # what a bundle holds, no camera needed
k1cam-flasher daemon                   # serve the camera here to a GUI elsewhere (Linux)
```

`flash` backs up next to the image, verifies and reboots unless told not to (`--no-backup`, `--no-verify`, `--no-reboot`); `update --no-reboot` only stages. Progress and messages go to stderr, results to stdout; the exit status is 0 on success, 1 on failure and 2 for usage errors. Output files are never overwritten. Ctrl-C stops an operation cleanly (a dump or upload ends its session on the camera); a second Ctrl-C quits at once.

On Linux, one k1cam-flasher at a time uses the camera: a command, the GUI (while it runs a job, reads the camera's information, tunes or has the console open) or the daemon (while a GUI controls it or a job runs). The others are told which one has it, e.g. `the camera is in use by k1cam-flasher daemon (pid 812), controlled from alex@studio (10.0.0.20)`. Firmware operations also refuse to start while another program has the camera's video node open, and name it; nothing is stopped for you.

## A camera on another computer

The camera can stay plugged into another Linux computer (a Raspberry Pi, a router, the printer's mainboard) while the GUI runs here, on any system. On that computer, as a user who can open the camera (see the udev rule), run:

```
k1cam-flasher daemon
```

It runs in the terminal while you need it and prints what to enter in the GUI:

```
k1cam-flasher daemon: listening on 0.0.0.0:19249 (plain HTTP: use it on a network you trust)
  connect to:  printer-pi, 192.168.1.40
  code:        8ESDUG  (a new one after 5 wrong attempts)
```

The code is six letters and digits (no 0/O or 1/I; case doesn't matter), new each time the daemon starts. The code is asked only to connect: after five different wrong ones the daemon makes and prints a new one, and the window connected already stays connected. A window whose connection lapsed, or whose daemon restarted, asks for the new code once (sending nothing more with the old one meanwhile) and carries on where it was, an operation it follows included. In the GUI, open the connection menu at the top right, choose *Another computer*, and enter one of the addresses and the code. *Remember* keeps the host in this user's configuration directory. `k1cam-flasher --remote HOST` opens the connection menu with that host filled in.

Ctrl-C stops the daemon. While a job runs, the first Ctrl-C lets it finish (no new job starts) and stops the daemon then; a second cancels the job, as the GUI's *Cancel* does, which can leave a flash half-written; a third quits at once. On the way out it names the backups no GUI has saved yet.

Then every tab works on that camera, Windows included:

- **Files stay here.** An image or bundle is sent when its job starts, and the daemon keeps it in memory only. A backup or dump comes back into the file chosen here once the job ends; a flash's backup lands next to the image, as it does locally. Until then the daemon keeps the backup on disk (in `~/.local/state/k1cam-flasher`, or `/var/lib/k1cam-flasher` as root), and it keeps it until a GUI has saved it: if the transfer fails or the window closed, the GUI offers to save it (or discard it) the next time it connects. It keeps at most four; beyond that, new backups are refused rather than old ones deleted.
- **Jobs carry on.** If the connection drops, the job runs on there; the window says so, keeps trying, and catches up when the connection is back. Closing the window offers to leave the job running. A job is never started twice by a repeated request, and one cut off by the daemon stopping is reported as interrupted, never resumed. Jobs are refused if their recovery record cannot be saved; an unreadable record prevents the daemon starting until it is repaired.
- **Tuning shows snapshots.** Instead of live video, *Snapshot* takes one picture (the camera's video runs only for it), and *Auto snapshot (2 fps)* takes one every half second while the tab shows; it is off at each new connection. Nothing is captured until asked. The caption says how old the picture is, and marks it once a control has changed since. *Save snapshot…* saves the picture shown, without taking another. The controls work as they do locally.
- **One window at a time** controls the camera there. It renews its session every second; the daemon lets go of the camera 15 s after it was last heard from (a job keeps running), and until then refuses other GUIs, saying which one has it. The same window reconnects with its session credential, even if its network address changes. A new or restarted window waits for the previous session to disconnect or expire.

The connection is plain HTTP on port 19249 (0x4b31, the camera's USB product id), the code and then a session id in the clear: use it on a network you trust. The port is outside the ranges Linux and Windows pick outgoing ports from, so the daemon can always bind it.

## Static builds

Without the GUI (`--no-default-features`) the flasher is about 1 MiB and depends on nothing but Rust's standard library: on Linux it talks to usbfs and V4L2 itself instead of using libusb, so it links statically for musl targets with no C cross toolchain. `.cargo/config.toml` links the ARM targets with the `rust-lld` that ships with Rust.

```
rustup target add aarch64-unknown-linux-musl      # e.g. Raspberry Pi OS, 64-bit
cargo build --release --no-default-features --target aarch64-unknown-linux-musl
# -> target/aarch64-unknown-linux-musl/release/k1cam-flasher (static)
```

The same works for `x86_64-unknown-linux-musl`, `armv7-unknown-linux-musleabihf` (32-bit Raspberry Pi OS) and `arm-unknown-linux-musleabihf` (ARMv6: Pi Zero, Pi 1); CI builds all four. On the target it needs access to `/dev/video*` (A/B) and `/dev/bus/usb` (cloner), e.g. as root or with the udev rule.

Little-endian MIPS (`mipsel-unknown-linux-musl`: MIPS32r2 or later, software floating point, so an FPU doesn't matter) is a tier 3 Rust target, with no standard library to download. Nightly Rust builds one from source, and `tools/mips-musl.sh` puts the musl it links into the nightly's sysroot, from Rust 1.71.1, the last release that shipped the target; `rust-lld` links it statically as for ARM:

```
rustup toolchain install nightly-2026-10-24 --profile minimal --component rust-src
tools/mips-musl.sh nightly-2026-10-24
cargo +nightly-2026-10-24 build --release --no-default-features --target mipsel-unknown-linux-musl -Z build-std=std,panic_abort
```

CI builds it with that nightly and runs the tests under QEMU (`qemu-mipsel`). It hasn't been tried with a camera on MIPS hardware yet. Big-endian MIPS and MIPS32r1 aren't built.

## Build

`cargo build --release` builds the GUI. It needs a C toolchain (for a Wayland build script on Linux, and libusb, which is vendored, on Windows and macOS); on Linux the build needs no extra GUI libraries beyond the usual X11/Wayland ones at runtime. CI in `.github/workflows/build.yml` builds the GUI on all three OSes (macOS for both Apple Silicon and Intel) and the static command-line binaries, MIPS included.

## Embedded cloner data

`src/cloner/data/` holds the burner SPL and U-Boot (GPL, from thingino-dfu), the T31X DDR config, the SoC-detect MIPS stub, the partition marker and the SPI NOR descriptors, generated byte-exactly with thingino-dfu's own code. Regenerate with `tools/gen-cloner-data/gen.sh /path/to/thingino-dfu-historical`.

## License

Mixed, per file (SPDX identifiers in the sources, texts in `LICENSES/`):

- **MIT**: the GUI, the console, the tuning tab, the command line, the daemon and the GUI's remote mode (`src/daemon.rs`, `src/remote.rs`, `src/http.rs`, `src/api.rs`), the k1cam A/B protocol client, the bundle parser, the image controls, the USB, Linux, serial and V4L2 device layers and the lock (`src/usb.rs`, `src/linux.rs`, `src/serial.rs`, `src/v4l2.rs`, `src/lock.rs`, `src/controls.rs`), udev rule and build files.
- **GPL-2.0-only**: `src/cloner/` and `tools/gen-cloner-data/`, which are derived from [thingino-dfu](https://github.com/gtxaspec/thingino-dfu), and the embedded burner SPL/U-Boot and generated blobs in `src/cloner/data/` (GPL U-Boot, thingino-dfu output).
- **CC0-1.0** (public domain): the photo `docs/short-nor-pins.jpg`, the synthetic test bundle `tests/data/golden.upd` and this documentation.

The built binary links the GPL-2.0-only code, so it as a whole is distributed under GPL-2.0-only; the source for it is this repository.
