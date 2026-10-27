// SPDX-License-Identifier: MIT
mod bundle;
mod cli;
mod cloner;
mod controls;
#[cfg(feature = "gui")]
mod console;
#[cfg(feature = "gui")]
mod gui;
mod job;
#[cfg(target_os = "linux")]
mod linux;
mod ops;
#[cfg(feature = "gui")]
mod overlay;
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
