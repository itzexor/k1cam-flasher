// SPDX-License-Identifier: MIT
//! What the daemon and the GUI's remote mode say to each other: the port, the
//! protocol version, the headers, and the JSON bodies. The routes are listed
//! in daemon.rs.

use serde::{Deserialize, Serialize};

/// 19249 is 0x4b31, the camera's USB product id. It is outside the ranges
/// Linux and Windows pick outgoing ports from, and IANA has it unassigned.
pub const PORT: u16 = 19249;

/// The protocol's version: the routes are under /api/v<API>/.
pub const API: u32 = 1;

pub const SESSION: &str = "K1cam-Session";
/// A snapshot's sequence number, age in milliseconds, and size.
pub const SEQ: &str = "K1cam-Seq";
pub const AGE: &str = "K1cam-Age-Ms";
/// Optional snapshot diagnostic record, hex encoded; "error" on a failed read.
pub const DIAGNOSTIC: &str = "K1cam-Diagnostic";
#[cfg(target_os = "linux")]
pub const WIDTH: &str = "K1cam-Width";
#[cfg(target_os = "linux")]
pub const HEIGHT: &str = "K1cam-Height";
/// Console output: the offset after the bytes sent, and whether the port is still open.
pub const NEXT: &str = "K1cam-Next";
pub const OPEN: &str = "K1cam-Open";
pub const SHA256: &str = "K1cam-Sha256";

/// A host lets go of the camera this long after its controlling GUI was last
/// heard from. Every request renews it; the GUI asks for the status every second.
pub const LEASE_MS: u64 = 15_000;

/// GET /api/hello, without the password: whether the two can talk at all.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Hello {
    pub api: u32,
    pub version: String,
    pub os: String,
    pub arch: String,
}

/// POST /api/v1/session: who wants control.
#[derive(Serialize, Deserialize)]
pub struct NewSession {
    pub client: String,
    /// The existing window's session credential, when reconnecting. A name
    /// and address identify a person, not a particular window.
    #[serde(default)]
    pub resume: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Session {
    pub id: String,
    pub host: String,
}

/// What a refused session says about who has the camera.
#[derive(Serialize, Deserialize)]
pub struct InUse {
    pub error: String,
    pub holder: String,
    pub age_ms: u64,
}

/// GET /api/v1/status: what is attached to the host, and its running job.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Status {
    /// A camera in USB boot mode (cloner).
    pub cloner: bool,
    /// A camera running k1cam.
    pub camera: bool,
    /// Its video node and its console.
    pub video: bool,
    pub console: bool,
    pub job: Option<String>,
}

/// A job, from its log line `after` on (GET /api/v1/jobs/ID?after=N).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Job {
    pub id: String,
    pub op: String,
    /// Unix time, seconds.
    pub started: u64,
    pub log: Vec<String>,
    /// The number of log lines so far: `after` for the next request.
    pub next: usize,
    pub progress: f32,
    pub done: Option<Result<String, String>>,
    /// A backup the host keeps until the job is dismissed.
    pub output: Option<Output>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Output {
    pub size: u64,
    pub sha256: String,
}
