// SPDX-License-Identifier: MIT
#[cfg(any(feature = "gui", target_os = "linux"))]
mod api;
mod bundle;
mod cli;
mod cloner;
#[cfg(feature = "gui")]
mod console;
mod controls;
#[cfg(target_os = "linux")]
mod daemon;
#[cfg(feature = "gui")]
mod gui;
#[cfg(any(feature = "gui", target_os = "linux"))]
mod http;
mod job;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod lock;
mod ops;
#[cfg(feature = "gui")]
mod overlay;
#[cfg(target_os = "linux")]
mod serial;
#[cfg(feature = "gui")]
mod tuning;
mod usb;
#[cfg(target_os = "linux")]
mod v4l2;
mod xu;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(status) = cli::run(&args) {
        std::process::exit(status);
    }
    #[cfg(feature = "gui")]
    if let Err(e) = gui::run(&args) {
        eprintln!("k1cam-flasher: {e}");
        std::process::exit(1);
    }
}
