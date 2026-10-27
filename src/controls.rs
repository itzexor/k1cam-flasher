// SPDX-License-Identifier: MIT
//! The camera's image controls as uvcdctl knows them, and reading and setting
//! them, here (V4L2) or through another computer's daemon. The GUI's Tuning tab
//! and the daemon both use these.

/// Where a control lives: a standard V4L2 control (UVC Processing Unit or
/// Camera Terminal) or a selector on k1cam's Extension Unit 4.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Std(u32),
    Xu(u8),
}

/// A control's current value, range and default, as the camera reports them.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Value {
    pub value: i32,
    pub min: i32,
    pub max: i32,
    pub step: i32,
    pub default: i32,
    /// Has no effect now, e.g. the white balance temperature while white
    /// balance is automatic.
    pub inactive: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Group {
    Picture,
    Exposure,
    WhiteBalance,
    Noise,
    View,
    Encoder,
}

#[cfg_attr(not(feature = "gui"), allow(dead_code))]
impl Group {
    pub const ALL: [Group; 6] = [Group::Picture, Group::Exposure, Group::WhiteBalance, Group::Noise, Group::View, Group::Encoder];

    pub fn title(self) -> &'static str {
        match self {
            Group::Picture => "Picture",
            Group::Exposure => "Exposure",
            Group::WhiteBalance => "White balance",
            Group::Noise => "Noise",
            Group::View => "View",
            Group::Encoder => "Encoder",
        }
    }
}

/// One control as uvcdctl knows it (its number and name are what `uvcdctl
/// get` and `set` take); ranges and defaults come from the camera.
// The headless build doesn't show controls, and only Linux reads them here.
#[cfg_attr(not(all(feature = "gui", target_os = "linux")), allow(dead_code))]
pub struct Def {
    pub id: u32,
    pub name: &'static str,
    pub label: &'static str,
    pub group: Group,
    pub kind: Kind,
    pub help: &'static str,
    /// A choice: its values and what they mean.
    pub choices: &'static [(i32, &'static str)],
    /// An on/off control: what "on" means.
    pub check: &'static str,
    /// The value that means "automatic", if one does.
    pub auto: Option<i32>,
    pub suffix: &'static str,
    /// The control that makes this one automatic, as a check box on this
    /// one's row: its name, and its automatic and manual values.
    pub switch: Option<(&'static str, i32, i32)>,
}

const fn ctl(id: u32, name: &'static str, label: &'static str, group: Group, kind: Kind, help: &'static str) -> Def {
    Def { id, name, label, group, kind, help, choices: &[], check: "", auto: None, suffix: "", switch: None }
}

impl Def {
    const fn choices(mut self, choices: &'static [(i32, &'static str)]) -> Self {
        self.choices = choices;
        self
    }

    const fn check(mut self, check: &'static str) -> Self {
        self.check = check;
        self
    }

    const fn auto(mut self, auto: i32) -> Self {
        self.auto = Some(auto);
        self
    }

    const fn suffix(mut self, suffix: &'static str) -> Self {
        self.suffix = suffix;
        self
    }

    const fn switch(mut self, name: &'static str, auto: i32, manual: i32) -> Self {
        self.switch = Some((name, auto, manual));
        self
    }

    /// Changing it changes what other controls do or report (modes, choices):
    /// read them all again afterwards.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn governs(&self) -> bool {
        !self.choices.is_empty() || !self.check.is_empty()
    }
}

// V4L2_CID_BASE and V4L2_CID_CAMERA_CLASS_BASE
const STD: u32 = 0x0098_0900;
const CAM: u32 = 0x009a_0900;

use Group::*;

/// uvcdctl's controls (package/k1cam-uvcd/uvcdctl), grouped.
pub const DEFS: &[Def] = &[
    ctl(1, "brightness", "Brightness", Picture, Kind::Std(STD), "128: default"),
    ctl(2, "contrast", "Contrast", Picture, Kind::Std(STD + 1), "128: default"),
    ctl(3, "saturation", "Saturation", Picture, Kind::Std(STD + 2), "128: default"),
    ctl(5, "hue", "Hue", Picture, Kind::Std(STD + 3), "0: default"),
    ctl(4, "sharpness", "Sharpness", Picture, Kind::Std(STD + 27), "128: default; more made MJPEG frames too big for USB"),
    ctl(6, "gamma", "Gamma", Picture, Kind::Std(STD + 16), "Gamma x100; 100: the factory curve"),
    ctl(11, "auto-exposure", "Exposure", Exposure, Kind::Std(CAM + 1), "").choices(&[(3, "Automatic"), (1, "Manual")]),
    ctl(13, "exposure-time", "Exposure time", Exposure, Kind::Std(CAM + 2), "In 100 µs units (333 is 33.3 ms)").switch(
        "auto-exposure",
        3,
        1,
    ),
    ctl(12, "exposure-priority", "Exposure priority", Exposure, Kind::Std(CAM + 3), "Lets the frame rate drop to expose longer")
        .check("frame rate may drop"),
    ctl(20, "exposure-compensation", "Compensation", Exposure, Kind::Xu(3), "128: none; higher is brighter"),
    ctl(32, "metering", "Metering", Exposure, Kind::Xu(21), "Subject: what stands out bright, kept from clipping").choices(&[
        (0, "Tuning default"),
        (1, "Average"),
        (2, "Center-weighted"),
        (3, "Spot"),
        (4, "Subject"),
    ]),
    ctl(21, "max-analog-gain", "Max analog gain", Exposure, Kind::Xu(1), "32 per doubling: 160 is 32x (default), 224 the sensor's maximum"),
    ctl(22, "max-digital-gain", "Max digital gain", Exposure, Kind::Xu(2), "32 per doubling: 80 is about 5.7x (default)"),
    ctl(8, "power-line-frequency", "Anti-flicker", Exposure, Kind::Std(STD + 24), "For lights that flicker at the mains frequency")
        .choices(&[(0, "Off"), (1, "50 Hz"), (2, "60 Hz")]),
    ctl(28, "highlight-suppression", "Highlights", Exposure, Kind::Xu(9), "Highlight suppression: 0 off, 1-10 strength"),
    ctl(26, "dynamic-range-compression", "Range compression", Exposure, Kind::Xu(7), "Dynamic range compression; 128: as tuned"),
    ctl(9, "white-balance-auto", "Automatic", WhiteBalance, Kind::Std(STD + 12), "Off: set the temperature").check("on"),
    ctl(10, "white-balance-temperature", "Temperature", WhiteBalance, Kind::Std(STD + 26), "The light's colour temperature")
        .suffix(" K")
        .switch("white-balance-auto", 1, 0),
    ctl(23, "spatial-denoise", "Spatial denoise", Noise, Kind::Xu(4), "128: as tuned"),
    ctl(24, "temporal-denoise", "Temporal denoise", Noise, Kind::Xu(5), "128: as tuned; high values smear moving things"),
    ctl(25, "defective-pixel-correction", "Pixel correction", Noise, Kind::Xu(6), "Defective pixel correction; 128: as tuned"),
    ctl(31, "rotation", "Rotation", View, Kind::Xu(22), "For a camera mounted upside down").choices(&[(0, "0°"), (180, "180°")]),
    ctl(14, "zoom", "Zoom", View, Kind::Std(CAM + 13), "Digital zoom x100; 100: the full view, at most 290 at 1920x1080"),
    ctl(15, "pan", "Pan", View, Kind::Std(CAM + 8), "Moves a zoomed view; positive is right"),
    ctl(16, "tilt", "Tilt", View, Kind::Std(CAM + 9), "Moves a zoomed view; positive is up"),
    ctl(50, "mjpeg-quality", "MJPEG quality", Encoder, Kind::Xu(19), "Higher is better (and bigger)"),
    ctl(40, "h264-bitrate-kbps", "H.264 bitrate", Encoder, Kind::Xu(13), "Target bitrate").auto(0).suffix(" kbps"),
    ctl(41, "h264-rate-control", "Rate control", Encoder, Kind::Xu(14), "H.264 rate control").choices(&[
        (0, "CBR"),
        (1, "VBR"),
        (2, "Capped VBR"),
    ]),
    ctl(43, "h264-min-qp", "H.264 min QP", Encoder, Kind::Xu(16), "Lowest QP, the best quality; automatic is 20").auto(-1),
    ctl(44, "h264-max-qp", "H.264 max QP", Encoder, Kind::Xu(17), "Highest QP, the worst quality").auto(-1),
    ctl(45, "h264-profile", "H.264 profile", Encoder, Kind::Xu(18), "").choices(&[(0, "Baseline"), (1, "Main"), (2, "High")]),
];

/// The camera's image controls, wherever they are reached. Each call may wait
/// on the camera (or the network), so callers keep them off the GUI thread.
pub trait Tuner: Send {
    /// The MJPEG frame sizes the camera offers, largest first.
    fn sizes(&self) -> Result<Vec<(u32, u32)>, String>;
    /// Every control's state, in DEFS order; None where the camera lacks one.
    fn load(&self) -> Result<Vec<Option<Value>>, String>;
    /// Sets DEFS[i] to `value`; returns what the camera then reports.
    fn set(&self, i: usize, value: i32) -> Result<i32, String>;
    /// The current values of these controls (those it could read).
    fn get(&self, which: &[usize]) -> Vec<(usize, i32)>;
    /// Factory defaults for every control.
    fn reset(&self) -> Result<(), String>;
}

#[cfg(target_os = "linux")]
pub use local::Local;

#[cfg(target_os = "linux")]
mod local {
    use super::{Kind, Tuner, Value, DEFS};
    use crate::v4l2;
    use std::path::{Path, PathBuf};

    /// The controls on this computer's video node.
    pub struct Local {
        node: PathBuf,
        ctl: v4l2::Controls,
    }

    impl Local {
        pub fn open(node: &Path) -> Result<Self, String> {
            Ok(Self { node: node.into(), ctl: v4l2::Controls::open(node)? })
        }
    }

    impl Tuner for Local {
        fn sizes(&self) -> Result<Vec<(u32, u32)>, String> {
            v4l2::frame_sizes(&self.node)
        }

        fn load(&self) -> Result<Vec<Option<Value>>, String> {
            load_all(&self.ctl)
        }

        fn set(&self, i: usize, value: i32) -> Result<i32, String> {
            let kind = DEFS.get(i).ok_or("no such control")?.kind;
            self.ctl.write(kind, value).and_then(|_| self.ctl.get(kind)).map_err(|e| e.to_string())
        }

        fn get(&self, which: &[usize]) -> Vec<(usize, i32)> {
            which.iter().filter_map(|&i| Some((i, self.ctl.get(DEFS.get(i)?.kind).ok()?))).collect()
        }

        fn reset(&self) -> Result<(), String> {
            factory_reset(&self.ctl)
        }
    }

    /// Every control's state; a control the camera doesn't have is None. The
    /// first one that fails otherwise ends the reading, so that a camera that
    /// doesn't answer costs one timeout, not one per control.
    fn load_all(ctl: &v4l2::Controls) -> Result<Vec<Option<Value>>, String> {
        let mut out = Vec::new();
        for d in DEFS {
            match ctl.read(d.kind) {
                Ok(v) => out.push(Some(v)),
                Err(e) if v4l2::unsupported(&e) => out.push(None),
                Err(e) => return Err(format!("reading {}: {e}", d.name)),
            }
        }
        Ok(out)
    }

    /// Factory defaults (as `uvcdctl reset`): the camera resets every control,
    /// but out of band, so uvcvideo keeps reporting the old standard values until
    /// they are written. Writing each default back refreshes them; a control
    /// governed by an automatic mode comes first, before that mode turns back on.
    fn factory_reset(ctl: &v4l2::Controls) -> Result<(), String> {
        ctl.factory_reset().map_err(|e| format!("factory reset: {e}"))?;
        for d in DEFS.iter().rev() {
            if let Kind::Std(_) = d.kind {
                if let Ok(v) = ctl.read(d.kind) {
                    if !v.inactive {
                        let _ = ctl.write(d.kind, v.default);
                    }
                }
            }
        }
        Ok(())
    }
}
