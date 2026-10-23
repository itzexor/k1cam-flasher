// SPDX-License-Identifier: MIT
//! Overlays for the Tuning tab's preview: where the camera meters exposure and
//! how bright each of its zones is, clipped highlights and crushed shadows, a
//! histogram and a framing grid. They come from the picture and the controls
//! alone. The metering geometry follows uvcd (compute_view, zone_in_view and
//! metering_weights in uvcd_camera.c); what subject metering picks is
//! estimated from the picture, and the tuning file's own weights aren't known
//! here.

use eframe::egui::{pos2, vec2, Color32, ColorImage, FontId, Galley, Painter, Pos2, Rect, Stroke, StrokeKind};
use std::sync::Arc;

/// The sensor uvcd meters over: the K1's GC2083, which uvcd also assumes
/// when the kernel doesn't say.
const SENSOR: (i32, i32) = (1920, 1080);
/// The ISP meters on this many zones across the sensor, and as many down.
const ZONES: usize = 15;
pub type Grid<T> = [[T; ZONES]; ZONES];

/// The part of the sensor a stream shows, as uvcd's compute_view has it: the
/// largest centred rectangle of the frame's aspect ratio, narrowed by zoom and
/// moved by pan and tilt. uvcd may hold the zoom below what was asked for at
/// the ISP's clock, which isn't known here.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl View {
    pub fn new(frame: [usize; 2], zoom: i32, pan: i32, tilt: i32) -> Self {
        let (sw, sh) = (SENSOR.0 as i64, SENSOR.1 as i64);
        let (ow, oh) = (frame[0].max(1) as i64, frame[1].max(1) as i64);
        let (mut bw, mut bh) = (sw, sw * oh / ow);
        if bh > sh {
            (bw, bh) = (sh * ow / oh, sh);
        }
        let zoom = zoom.max(100) as i64;
        let (w, h) = ((bw * 100 / zoom) as i32 & !1, (bh * 100 / zoom) as i32 & !1);
        let (spare_x, spare_y) = (SENSOR.0 - w, SENSOR.1 - h);
        let x = spare_x / 2 + ((spare_x / 2) as i64 * pan as i64 / 36000) as i32;
        let y = spare_y / 2 - ((spare_y / 2) as i64 * tilt as i64 / 36000) as i32;
        Self { x: x.clamp(0, spare_x) & !1, y: y.clamp(0, spare_y) & !1, w, h }
    }

    /// A zone's centre on the sensor.
    fn centre(r: usize, c: usize) -> (i32, i32) {
        ((2 * c as i32 + 1) * SENSOR.0 / 30, (2 * r as i32 + 1) * SENSOR.1 / 30)
    }

    /// A zone counts when its centre is in the view.
    fn holds(&self, r: usize, c: usize) -> bool {
        let (x, y) = Self::centre(r, c);
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    /// Where a point of the sensor shows in `frame`, the picture on screen.
    fn at(&self, frame: Rect, x: f32, y: f32) -> Pos2 {
        let u = (x - self.x as f32) / self.w as f32;
        let v = (y - self.y as f32) / self.h as f32;
        frame.min + vec2(u * frame.width(), v * frame.height())
    }

    /// A zone in `frame`.
    fn zone(&self, frame: Rect, r: usize, c: usize) -> Rect {
        let (zw, zh) = (SENSOR.0 as f32 / ZONES as f32, SENSOR.1 as f32 / ZONES as f32);
        let (x, y) = (c as f32 * zw, r as f32 * zh);
        Rect::from_min_max(self.at(frame, x, y), self.at(frame, x + zw, y + zh))
    }

    /// The zone column (or row, with the height) a pixel of a frame `n`
    /// pixels wide falls in.
    fn zone_of(&self, i: usize, n: usize, across: bool) -> usize {
        let (start, len, sensor) = if across { (self.x, self.w, SENSOR.0) } else { (self.y, self.h, SENSOR.1) };
        let s = start as f32 + (i as f32 + 0.5) / n as f32 * len as f32;
        ((s * ZONES as f32 / sensor as f32) as usize).min(ZONES - 1)
    }
}

/// uvcd's metering modes, in the order of the control's values.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Metering {
    /// The sensor tuning file's own weights.
    Tuning,
    Average,
    Center,
    Spot,
    Subject,
}

impl Metering {
    pub fn from_value(v: i32) -> Option<Self> {
        let all = [Self::Tuning, Self::Average, Self::Center, Self::Spot, Self::Subject];
        all.get(usize::try_from(v).ok()?).copied()
    }
}

/// Each zone's weight, 0-8, as uvcd's metering_weights gives it: 8 throughout
/// for Average; 8 within a fifth of the way from the centre to the edge for
/// Spot (or the zone under the centre, when that holds none); 8 easing to 2
/// at the edge for Center, which Subject starts from. Zones out of the view
/// get none.
pub fn weights(view: &View, mode: Metering) -> Grid<u8> {
    let (cx, cy) = (view.x + view.w / 2, view.y + view.h / 2);
    let mut w = [[0; ZONES]; ZONES];
    let mut any = false;
    for (r, row) in w.iter_mut().enumerate() {
        for (c, weight) in row.iter_mut().enumerate() {
            if !view.holds(r, c) {
                continue;
            }
            let (zx, zy) = View::centre(r, c);
            // 0 at the centre of the view, 1 at its edge
            let dx = (zx - cx).abs() as f32 / (view.w / 2) as f32;
            let dy = (zy - cy).abs() as f32 / (view.h / 2) as f32;
            let d = dx.max(dy);
            *weight = match mode {
                Metering::Average => 8,
                Metering::Spot if d <= 0.2 => 8,
                Metering::Spot => 0,
                _ => (8.0 - 6.0 * d + 0.5) as u8,
            };
            any |= *weight != 0;
        }
    }
    if !any {
        w[(cy * ZONES as i32 / SENSOR.1) as usize][(cx * ZONES as i32 / SENSOR.0) as usize] = 8;
    }
    w
}

/// Subject metering as uvcd works it out from zones this bright (in linear
/// light): zones from 1.5 to 3 times the view's median brightness are eased
/// from the Center weights up to full weight. The weights it settles on, and
/// the zones it takes for the subject (those at least half way).
pub fn subject(view: &View, light: &Grid<Option<f32>>) -> (Grid<u8>, Grid<bool>) {
    let mut w = weights(view, Metering::Center);
    let mut marked = [[false; ZONES]; ZONES];
    let mut all: Vec<f32> = light.iter().flatten().flatten().copied().collect();
    if all.is_empty() {
        return (w, marked);
    }
    all.sort_by(f32::total_cmp);
    // Below this, ratios are dark-zone noise.
    let median = all[all.len() / 2].max(4.0);
    for r in 0..ZONES {
        for c in 0..ZONES {
            if let Some(z) = light[r][c] {
                let s = ((z / median - 1.5) / 1.5).clamp(0.0, 1.0);
                w[r][c] = (w[r][c] as f32 + (8.0 - w[r][c] as f32) * s + 0.5) as u8;
                marked[r][c] = s >= 0.5;
            }
        }
    }
    (w, marked)
}

/// What the overlays take from a frame.
pub struct Measure {
    /// Each zone's mean luma (0-255), for the zones in the view.
    pub zones: Grid<Option<f32>>,
    /// Luma, of every other pixel each way.
    pub hist: [u32; 256],
    /// The share of those pixels with a channel at 250 or more, and with all
    /// at 6 or less.
    pub clipped: f32,
    pub crushed: f32,
}

const STRIPE: Color32 = Color32::from_rgba_unmultiplied_const(255, 40, 40, 200);
const CRUSHED: Color32 = Color32::from_rgba_unmultiplied_const(40, 110, 255, 170);

/// Measures a frame, and paints a mask at half its size: stripes over clipped
/// pixels and blue over crushed ones.
pub fn measure(img: &ColorImage, view: &View) -> (Measure, ColorImage) {
    let [w, h] = img.size;
    let (mw, mh) = (w.div_ceil(2), h.div_ceil(2));
    let cols: Vec<usize> = (0..mw).map(|i| view.zone_of(2 * i, w, true)).collect();
    let rows: Vec<usize> = (0..mh).map(|j| view.zone_of(2 * j, h, false)).collect();
    let (mut sum, mut n) = ([[0u64; ZONES]; ZONES], [[0u32; ZONES]; ZONES]);
    let mut hist = [0u32; 256];
    let (mut clipped, mut crushed) = (0u32, 0u32);
    let mut mask = vec![Color32::TRANSPARENT; mw * mh];
    for j in 0..mh {
        let line = &img.pixels[2 * j * w..][..w];
        for i in 0..mw {
            let p = line[2 * i];
            let (r, g, b) = (p.r() as u32, p.g() as u32, p.b() as u32);
            let y = (299 * r + 587 * g + 114 * b + 500) / 1000;
            hist[y as usize] += 1;
            sum[rows[j]][cols[i]] += y as u64;
            n[rows[j]][cols[i]] += 1;
            let top = r.max(g).max(b);
            if top >= 250 {
                clipped += 1;
                if (i + j) / 3 % 2 == 0 {
                    mask[j * mw + i] = STRIPE;
                }
            } else if top <= 6 {
                crushed += 1;
                mask[j * mw + i] = CRUSHED;
            }
        }
    }
    let mut zones = [[None; ZONES]; ZONES];
    for r in 0..ZONES {
        for c in 0..ZONES {
            if view.holds(r, c) && n[r][c] > 0 {
                zones[r][c] = Some(sum[r][c] as f32 / n[r][c] as f32);
            }
        }
    }
    let total = (mw * mh).max(1) as f32;
    let m = Measure { zones, hist, clipped: clipped as f32 / total, crushed: crushed as f32 / total };
    (m, ColorImage::new([mw, mh], mask))
}

/// Luma (0-255) in linear light, near enough: the picture's gamma taken as 2.2.
pub fn linear(zones: &Grid<Option<f32>>) -> Grid<Option<f32>> {
    zones.map(|row| row.map(|z| z.map(|y| 255.0 * (y / 255.0).powf(2.2))))
}

/// The zones in five slices of brightness, after the view's darkest zone, its
/// median and its brightest, each side of the median in thirds: 0 near the
/// darkest, 1 between it and the median, 2 near the median, 3 between it and
/// the brightest, 4 near the brightest. Also those three levels.
pub fn slices(zones: &Grid<Option<f32>>) -> (Grid<Option<u8>>, [f32; 3]) {
    let mut all: Vec<f32> = zones.iter().flatten().flatten().copied().collect();
    if all.is_empty() {
        return ([[None; ZONES]; ZONES], [0.0; 3]);
    }
    all.sort_by(f32::total_cmp);
    let (min, med, max) = (all[0], all[all.len() / 2], all[all.len() - 1]);
    let slice = |z: f32| {
        if z <= med {
            let t = if med > min { (z - min) / (med - min) } else { 1.0 };
            [0, 1, 2][((t * 3.0) as usize).min(2)]
        } else {
            let t = (z - med) / (max - med);
            [2, 3, 4][((t * 3.0).ceil() as usize).clamp(1, 3) - 1]
        }
    };
    (zones.map(|row| row.map(|z| z.map(slice))), [min, med, max])
}

/// The luma that `share` of the histogram's pixels are at or above.
pub fn level(hist: &[u32; 256], share: f32) -> u8 {
    let total: u32 = hist.iter().sum();
    let mut above = 0;
    for (y, n) in hist.iter().enumerate().rev() {
        above += n;
        if above as f32 >= total as f32 * share {
            return y as u8;
        }
    }
    0
}

/// The slices' colours, from the darkest to the brightest.
pub const SLICES: [Color32; 5] = [
    Color32::from_rgb(40, 100, 200),
    Color32::from_rgb(120, 175, 235),
    Color32::from_rgb(230, 230, 230),
    Color32::from_rgb(245, 165, 65),
    Color32::from_rgb(230, 60, 45),
];
const SHADE: Color32 = Color32::from_black_alpha(170);
/// Under text and the histogram: dark enough that dots don't show through.
pub const BACKDROP: Color32 = Color32::from_black_alpha(215);
const MARK: Color32 = Color32::from_rgb(255, 215, 0);

/// Metering over `frame`: a dot on each zone in the view, its area the zone's
/// weight (a ring where the weight isn't known) and its colour the zone's
/// slice; an X on the zones subject metering takes for the subject; and the
/// spot outlined, with an X at its centre.
pub fn draw_metering(
    p: &Painter,
    frame: Rect,
    view: &View,
    weights: Option<&Grid<u8>>,
    slices: &Grid<Option<u8>>,
    marked: &Grid<bool>,
    spot: bool,
) {
    let cell = view.zone(frame, 0, 0).size();
    let most = (cell.x.min(cell.y) * 0.28).clamp(2.0, 11.0);
    let mut lit = Rect::NOTHING;
    for r in 0..ZONES {
        for c in 0..ZONES {
            let Some(slice) = slices[r][c] else { continue };
            let zone = view.zone(frame, r, c);
            let color = SLICES[slice as usize];
            match weights.map(|w| w[r][c]) {
                Some(0) => {}
                Some(w) => {
                    p.circle(zone.center(), most * (w as f32 / 8.0).sqrt(), color, Stroke::new(1.0, SHADE));
                    lit |= zone;
                }
                None => {
                    p.circle_stroke(zone.center(), most * 0.7, Stroke::new(3.0, SHADE));
                    p.circle_stroke(zone.center(), most * 0.7, Stroke::new(1.5, color));
                }
            }
            if marked[r][c] {
                cross(p, zone.center(), most * 0.9);
            }
        }
    }
    if spot && lit.is_positive() {
        p.rect_stroke(lit, 2.0, Stroke::new(3.5, SHADE), StrokeKind::Inside);
        p.rect_stroke(lit, 2.0, Stroke::new(1.5, MARK), StrokeKind::Inside);
        cross(p, frame.center(), most);
    }
}

/// An X, outlined so that it shows on anything.
fn cross(p: &Painter, at: Pos2, size: f32) {
    for (width, color) in [(4.0, SHADE), (2.0, MARK)] {
        p.line_segment([at - vec2(size, size), at + vec2(size, size)], Stroke::new(width, color));
        p.line_segment([at + vec2(-size, size), at + vec2(size, -size)], Stroke::new(width, color));
    }
}

/// Thirds and the centre, for aiming the camera.
pub fn draw_grid(p: &Painter, frame: Rect) {
    let mut lines = Vec::new();
    for t in [1.0 / 3.0, 2.0 / 3.0] {
        let (x, y) = (frame.left() + frame.width() * t, frame.top() + frame.height() * t);
        lines.push([pos2(x, frame.top()), pos2(x, frame.bottom())]);
        lines.push([pos2(frame.left(), y), pos2(frame.right(), y)]);
    }
    let (c, arm) = (frame.center(), frame.height().min(frame.width()) * 0.04);
    lines.push([c - vec2(arm, 0.0), c + vec2(arm, 0.0)]);
    lines.push([c - vec2(0.0, arm), c + vec2(0.0, arm)]);
    for (width, color) in [(3.0, Color32::from_black_alpha(110)), (1.0, Color32::from_white_alpha(170))] {
        for l in &lines {
            p.line_segment(*l, Stroke::new(width, color));
        }
    }
}

/// The luma histogram in `area`, with the clipping ends tinted and the
/// median and the brightest 1% marked.
pub fn draw_histogram(p: &Painter, area: Rect, hist: &[u32; 256]) {
    p.rect_filled(area, 4.0, BACKDROP);
    let inner = area.shrink(4.0);
    let at = |y: f32| inner.left() + y / 255.0 * inner.width();
    p.rect_filled(Rect::from_x_y_ranges(at(0.0)..=at(6.0), inner.y_range()), 0.0, CRUSHED.gamma_multiply(0.5));
    p.rect_filled(Rect::from_x_y_ranges(at(250.0)..=at(255.0), inner.y_range()), 0.0, STRIPE.gamma_multiply(0.5));
    // A column per point of width, of the tallest bin under it.
    let width = inner.width().max(1.0) as usize;
    let column = |i: usize| {
        let from = i * 256 / width;
        hist[from..((i + 1) * 256 / width).max(from + 1)].iter().copied().max().unwrap_or(0)
    };
    let cols: Vec<u32> = (0..width).map(column).collect();
    let tallest = cols.iter().copied().max().unwrap_or(0).max(1) as f32;
    for (i, n) in cols.iter().enumerate() {
        let (x, top) = (inner.left() + i as f32 + 0.5, inner.bottom() - inner.height() * *n as f32 / tallest);
        p.line_segment([pos2(x, inner.bottom()), pos2(x, top)], Stroke::new(1.0, Color32::from_gray(200)));
    }
    for (y, color) in [(level(hist, 0.5), Color32::WHITE), (level(hist, 0.01), MARK)] {
        let x = at(y as f32);
        p.line_segment([pos2(x, inner.top()), pos2(x, inner.bottom())], Stroke::new(1.5, color));
    }
}

/// A piece of a legend line.
pub enum Item {
    Text(String),
    Swatch(Color32),
}

/// Lines of text and colour swatches in a dark box, from `at` down.
pub fn draw_legend(p: &Painter, at: Pos2, lines: &[Vec<Item>]) {
    let font = FontId::proportional(12.0);
    let (pad, gap, line) = (6.0, 4.0, p.ctx().fonts_mut(|f| f.row_height(&font)) + 3.0);
    // Laid out first, for the size of the box drawn under them.
    let mut texts: Vec<(Pos2, Arc<Galley>)> = Vec::new();
    let mut swatches = Vec::new();
    let mut width: f32 = 0.0;
    for (n, items) in lines.iter().enumerate() {
        let y = at.y + pad + n as f32 * line + line / 2.0;
        let mut x = at.x + pad;
        for item in items {
            match item {
                Item::Text(t) => {
                    let g = p.layout_no_wrap(t.clone(), font.clone(), Color32::WHITE);
                    let size = g.size();
                    texts.push((pos2(x, y - size.y / 2.0), g));
                    x += size.x + gap;
                }
                Item::Swatch(c) => {
                    swatches.push((Rect::from_min_size(pos2(x, y - 5.0), vec2(10.0, 10.0)), *c));
                    x += 10.0 + gap;
                }
            }
        }
        width = width.max(x - gap - at.x + pad);
    }
    p.rect_filled(Rect::from_min_size(at, vec2(width, lines.len() as f32 * line + 2.0 * pad)), 4.0, BACKDROP);
    for (pos, g) in texts {
        p.galley(pos, g, Color32::WHITE);
    }
    for (r, c) in swatches {
        p.rect(r, 2.0, c, Stroke::new(1.0, SHADE), StrokeKind::Inside);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: [usize; 2] = [1280, 720];

    #[test]
    fn the_view_follows_uvcd() {
        assert_eq!(View::new(FULL, 100, 0, 0), View { x: 0, y: 0, w: 1920, h: 1080 });
        // 4:3 is cropped from the middle, not squashed.
        assert_eq!(View::new([640, 480], 100, 0, 0), View { x: 240, y: 0, w: 1440, h: 1080 });
        assert_eq!(View::new(FULL, 200, 0, 0), View { x: 480, y: 270, w: 960, h: 540 });
        // Pan all the way right and tilt all the way up.
        assert_eq!(View::new(FULL, 200, 36000, 36000), View { x: 960, y: 0, w: 960, h: 540 });
    }

    #[test]
    fn the_fixed_weights_follow_uvcd() {
        let full = View::new(FULL, 100, 0, 0);
        assert!(weights(&full, Metering::Average).iter().flatten().all(|w| *w == 8));
        // The spot at full view: the middle three zones each way.
        let spot = weights(&full, Metering::Spot);
        let lit: Vec<(usize, usize)> = (0..225).map(|i| (i / 15, i % 15)).filter(|&(r, c)| spot[r][c] == 8).collect();
        assert_eq!(lit, [(6, 6), (6, 7), (6, 8), (7, 6), (7, 7), (7, 8), (8, 6), (8, 7), (8, 8)]);
        assert!(spot.iter().flatten().all(|w| *w == 0 || *w == 8));
        let center = weights(&full, Metering::Center);
        assert_eq!((center[7][7], center[0][0], center[7][0], center[3][7]), (8, 2, 2, 5));
        // Zoomed in, zones out of the view get nothing; a spot that holds no
        // zone's centre (here, panned to between two) takes the zone under it.
        let zoomed = View::new(FULL, 400, 3200, 0);
        let spot = weights(&zoomed, Metering::Spot);
        assert_eq!(spot.iter().flatten().filter(|w| **w > 0).count(), 1);
        assert_eq!(spot[7][8], 8);
        assert_eq!(weights(&zoomed, Metering::Average)[0][0], 0);
    }

    #[test]
    fn subject_metering_takes_what_stands_out() {
        let full = View::new(FULL, 100, 0, 0);
        let mut light = [[Some(20.0); ZONES]; ZONES];
        light[2][3] = Some(100.0); // 5x the median: the subject
        light[2][4] = Some(40.0); // 2x: eased a third of the way up
        let (w, marked) = subject(&full, &light);
        let base = weights(&full, Metering::Center);
        assert_eq!((w[2][3], marked[2][3]), (8, true));
        assert!(w[2][4] > base[2][4] && w[2][4] < 8 && !marked[2][4]);
        assert_eq!((w[12][12], marked[12][12]), (base[12][12], false));
    }

    #[test]
    fn a_frame_is_measured_by_zone_and_by_pixel() {
        // The left third black, the rest white: crushed, then clipped.
        let (w, h) = (1280, 720);
        let mut img = ColorImage::filled([w, h], Color32::WHITE);
        for y in 0..h {
            for x in 0..w / 3 {
                img.pixels[y * w + x] = Color32::BLACK;
            }
        }
        let (m, mask) = measure(&img, &View::new(FULL, 100, 0, 0));
        assert_eq!(mask.size, [640, 360]);
        assert!((m.crushed - 1.0 / 3.0).abs() < 0.01 && (m.clipped - 2.0 / 3.0).abs() < 0.01);
        assert_eq!((m.zones[0][0], m.zones[14][14]), (Some(0.0), Some(255.0)));
        assert_eq!((level(&m.hist, 0.5), level(&m.hist, 0.9)), (255, 0));
        // Zones out of a zoomed view have no level.
        let (m, _) = measure(&img, &View::new(FULL, 200, 0, 0));
        assert!(m.zones[0][0].is_none() && m.zones[7][7].is_some());
    }

    #[test]
    fn zones_fall_in_five_slices_around_the_median() {
        let mut zones = [[None; ZONES]; ZONES];
        for (i, z) in [0.0, 10.0, 25.0, 30.0, 50.0, 60.0, 70.0, 95.0, 100.0].into_iter().enumerate() {
            zones[0][i] = Some(z);
        }
        let (s, [min, med, max]) = slices(&zones);
        assert_eq!((min, med, max), (0.0, 50.0, 100.0));
        let got: Vec<u8> = s[0][..9].iter().map(|z| z.unwrap()).collect();
        assert_eq!(got, [0, 0, 1, 1, 2, 2, 3, 4, 4]);
        assert!(s[1][0].is_none());
    }
}
