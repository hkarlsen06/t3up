//! Motion: an intro, drifting dust, and effects that show something is happening. Each one is a pure
//! function of time over the rendered buffer, drawn after the frame it decorates.
use std::collections::HashMap;
use std::f32::consts::TAU;
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use super::theme::{ACCENT, AMBER, BG, DIM, GREEN, MUTED, RED, SPIN, TEXT, blend, fg, hex};
use crate::model::Status;

/// How long the intro plays; it dissolves into the dashboard over its last `DISSOLVE`.
pub const INTRO: Duration = Duration::from_millis(2100);
const DISSOLVE: f32 = 0.5;
/// How long a finished card's border glows, and a version takes to decode.
const PULSE: f32 = 1.1;
const DECODE: f32 = 0.55;
/// A modal fades in over this long; a toast slides in (and out) over `SLIDE`.
pub const FADE: f32 = 0.18;
pub const SLIDE: f32 = 0.28;

const VIOLET: Color = hex(0xc792ea);
const TEAL: Color = hex(0x7fdbca);
const WHITE: Color = hex(0xffffff);

#[derive(Debug, Default)]
pub struct Motion {
    /// Off with `T3UP_NO_MOTION`, and in tests, which render fixed frames.
    pub on: bool,
    /// When the intro started; None once it's over (or skipped).
    pub intro: Option<Instant>,
    /// A card's border glows after its run ends: host -> (when, color).
    pub pulses: HashMap<String, (Instant, Color)>,
    /// When a tool's version arrived, so it decodes in: (host, tool) -> when.
    pub arrived: HashMap<(String, String), Instant>,
    /// When the open modal opened.
    pub modal_since: Option<Instant>,
}

impl Motion {
    pub fn new(on: bool) -> Self {
        Motion { on, intro: on.then(Instant::now), ..Default::default() }
    }

    /// Time into the intro, if it's playing.
    pub fn intro_at(&self) -> Option<f32> {
        let t = self.intro?.elapsed();
        (t < INTRO).then_some(t.as_secs_f32())
    }

    pub fn pulse(&mut self, host: &str, color: Color) {
        if self.on {
            self.pulses.insert(host.to_string(), (Instant::now(), color));
        }
    }

    pub fn arrive(&mut self, host: &str, tool: &str) {
        if self.on {
            self.arrived.insert((host.to_string(), tool.to_string()), Instant::now());
        }
    }

    /// The pulse color a card's border takes now, mixed over `base`.
    pub fn border(&self, host: &str, base: Color) -> Color {
        match self.pulses.get(host) {
            Some((at, color)) if self.on => {
                let k = (1.0 - at.elapsed().as_secs_f32() / PULSE).max(0.0);
                blend(*color, base, k * k)
            }
            _ => base,
        }
    }

    /// `text` as it decodes in, left to right, after the version arrived.
    pub fn decode(&self, host: &str, tool: &str, text: &str) -> String {
        let Some(at) = self.arrived.get(&(host.to_string(), tool.to_string())).filter(|_| self.on) else {
            return text.to_string();
        };
        let age = at.elapsed().as_secs_f32();
        if age >= DECODE {
            return text.to_string();
        }
        let shown = (age / DECODE * text.chars().count() as f32) as usize;
        let frame = (age * 20.0) as u32;
        text.chars()
            .enumerate()
            .map(|(i, c)| {
                if i < shown || !c.is_ascii_alphanumeric() {
                    c
                } else {
                    let pool = b"0123456789abcdef#";
                    pool[(hash(i as u32, frame, tool.len() as u32) * pool.len() as f32) as usize % pool.len()] as char
                }
            })
            .collect()
    }

    /// How far in a meter has grown, 0 to 1, since `key` arrived for `host`.
    pub fn grown(&self, host: &str, key: &str) -> f32 {
        match self.arrived.get(&(host.to_string(), key.to_string())) {
            Some(at) if self.on => ease_out(at.elapsed().as_secs_f32() / DECODE),
            _ => 1.0,
        }
    }

    /// Whether anything is mid-animation (beyond the always-on dust): the caller redraws faster.
    pub fn busy(&self) -> bool {
        self.on
            && (self.intro_at().is_some()
                || self.pulses.values().any(|(at, _)| at.elapsed().as_secs_f32() < PULSE)
                || self.arrived.values().any(|at| at.elapsed().as_secs_f32() < DECODE)
                || self.modal_since.is_some_and(|at| at.elapsed().as_secs_f32() < FADE))
    }

    /// Forget animations that are done, so the maps don't grow.
    pub fn prune(&mut self) {
        self.pulses.retain(|_, (at, _)| at.elapsed().as_secs_f32() < PULSE);
        self.arrived.retain(|_, at| at.elapsed().as_secs_f32() < DECODE);
        if self.intro.is_some_and(|at| at.elapsed() >= INTRO) {
            self.intro = None;
        }
    }
}

/// A stable pseudo-random number in [0, 1) for these inputs.
pub fn hash(a: u32, b: u32, c: u32) -> f32 {
    let mut x = a.wrapping_mul(0x9E37_79B1) ^ b.wrapping_mul(0x85EB_CA77) ^ c.wrapping_mul(0xC2B2_AE3D);
    x ^= x >> 15;
    x = x.wrapping_mul(0x2C1B_3C6D);
    x ^= x >> 12;
    x = x.wrapping_mul(0x297A_2D39);
    x ^= x >> 15;
    (x >> 8) as f32 / (1u32 << 24) as f32
}

pub fn ease_out(x: f32) -> f32 {
    1.0 - (1.0 - x.clamp(0.0, 1.0)).powi(3)
}

fn ease_in_out(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// A color on the accent → violet → teal loop, `u` in turns.
fn spectrum(u: f32) -> Color {
    let u = u.rem_euclid(1.0) * 3.0;
    let (a, b, k) = match u as u32 {
        0 => (ACCENT, VIOLET, u),
        1 => (VIOLET, TEAL, u - 1.0),
        _ => (TEAL, ACCENT, u - 2.0),
    };
    blend(b, a, ease_in_out(k))
}

// ── the intro ───────────────────────────────────────────────────────────────

/// 't3up' in a 5x7 pixel font.
const WORD: [[&str; 7]; 4] = [
    [".#...", ".#...", "####.", ".#...", ".#...", ".#...", "..###"],
    ["####.", "....#", "....#", ".###.", "....#", "....#", "####."],
    [".....", ".....", "#...#", "#...#", "#...#", "#...#", ".####"],
    [".....", ".....", "####.", "#...#", "#...#", "####.", "#...."],
];

/// The wordmark's pixels at scale `s` (each font pixel is s×s half-block pixels), and its size.
fn wordmark(s: usize) -> (Vec<(usize, usize)>, usize, usize) {
    let mut px = vec![];
    for (g, glyph) in WORD.iter().enumerate() {
        for (y, row) in glyph.iter().enumerate() {
            for (x, c) in row.bytes().enumerate() {
                if c == b'#' {
                    for (dy, dx) in (0..s).flat_map(|dy| (0..s).map(move |dx| (dy, dx))) {
                        px.push(((g * 6 + x) * s + dx, y * s + dy));
                    }
                }
            }
        }
    }
    (px, (4 * 6 - 1) * s, 7 * s)
}

/// Draw the intro over the dashboard in `buf`: the wordmark sweeps in on a wave, the servers tick
/// off as their checks land, then it all dissolves into the dashboard underneath.
pub fn intro(buf: &mut Buffer, t: f32, hosts: &[(String, Status)]) {
    let area = buf.area;
    let mut o = Buffer::empty(area);
    o.set_style(area, Style::new().bg(BG).fg(TEXT));
    sparkle(&mut o, area, t);
    let s = if area.width >= 56 && area.height >= 16 { 2 } else { 1 };
    let (pixels, pw, ph) = wordmark(s);
    let rows = ph.div_ceil(2) as u16;
    let x0 = area.x + area.width.saturating_sub(pw as u16) / 2;
    let y0 = area.y + area.height.saturating_sub(rows + 4) / 2;
    // Half-block canvas: two pixels per cell, top and bottom. A drop shadow first, then the light.
    let mut canvas: HashMap<(usize, usize), Color> = HashMap::new();
    let reveal = t / 0.85;
    let span = (pw + ph * 2) as f32;
    let shadow = blend(ACCENT, BG, 0.09);
    for &(x, y) in &pixels {
        let front = (x as f32 + y as f32 * 2.0) / span;
        if reveal < front {
            continue;
        }
        let age = (reveal - front) * 0.85;
        let wave = 0.8 + 0.2 * (x as f32 * 0.32 - y as f32 * 0.2 - t * 7.0).sin();
        let mut color = blend(spectrum(x as f32 / pw as f32 * 0.8 - t * 0.35), BG, wave);
        if age < 0.18 {
            color = blend(WHITE, color, 1.0 - age / 0.18);
        }
        canvas.entry((x + 1, y + 1)).or_insert(shadow);
        canvas.insert((x, y), color);
    }
    for row in 0..=rows {
        for col in 0..(pw + s) as u16 {
            let (top, bottom) =
                (canvas.get(&(col as usize, row as usize * 2)), canvas.get(&(col as usize, row as usize * 2 + 1)));
            if top.is_none() && bottom.is_none() {
                continue;
            }
            if let Some(cell) = o.cell_mut((x0 + col, y0 + row)) {
                cell.set_symbol("▀").set_fg(*top.unwrap_or(&BG)).set_bg(*bottom.unwrap_or(&BG));
            }
        }
    }
    // The tagline, then each server as its check lands.
    let text_in = ease_out((t - 0.45) / 0.4);
    let tag = Line::styled("T3 Code servers, kept current", fg(blend(MUTED, BG, text_in)));
    center(&mut o, y0 + rows + 1, &tag);
    let mut spans = vec![];
    for (i, (name, status)) in hosts.iter().enumerate() {
        let (mark, color) = match status {
            Status::Ok => ('✓', GREEN),
            Status::Failed => ('✗', RED),
            Status::Running => (SPIN[(t * 10.0) as usize % SPIN.len()], AMBER),
            Status::Idle => ('·', DIM),
        };
        if i > 0 {
            spans.push(Span::raw("   "));
        }
        spans.push(Span::styled(format!("{mark} "), fg(blend(color, BG, text_in))));
        spans.push(Span::styled(name.clone(), fg(blend(MUTED, BG, text_in))));
    }
    center(&mut o, y0 + rows + 3, &Line::from(spans));
    // Dissolve: each cell flips to the dashboard at its own moment, roughly top to bottom.
    let d = ((t - (INTRO.as_secs_f32() - DISSOLVE)) / DISSOLVE).clamp(0.0, 1.0);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let at = 0.65 * hash(x as u32, y as u32, 7) + 0.35 * (y - area.y) as f32 / area.height.max(1) as f32;
            if d > 0.0 && ease_in_out(d) * 1.02 > at {
                continue;
            }
            if let (Some(from), Some(to)) = (o.cell((x, y)), buf.cell_mut((x, y))) {
                *to = from.clone();
            }
        }
    }
}

fn center(buf: &mut Buffer, y: u16, line: &Line) {
    let w = line.width() as u16;
    if y < buf.area.bottom() {
        let x = buf.area.x + buf.area.width.saturating_sub(w) / 2;
        buf.set_line(x, y, line, buf.area.right().saturating_sub(x));
    }
}

/// Stars that fade in and out across the intro's background.
fn sparkle(buf: &mut Buffer, area: Rect, t: f32) {
    let n = (area.width as u32 * area.height as u32) / 28;
    for i in 0..n {
        let x = area.x + (hash(i, 1, 3) * area.width as f32) as u16;
        let y = area.y + (hash(i, 2, 3) * area.height as f32) as u16;
        let k = (0.5 + 0.5 * (t * (1.5 + 2.0 * hash(i, 4, 3)) + hash(i, 5, 3) * TAU).sin()).powi(3);
        if let Some(cell) = buf.cell_mut((x, y)) {
            let star = ["·", "⋅", "∙", "✦"][(hash(i, 6, 3) * 3.3) as usize];
            cell.set_symbol(star).set_fg(blend(spectrum(hash(i, 7, 3)), BG, 0.15 + 0.5 * k));
        }
    }
}

// ── ambient ────────────────────────────────────────────────────────────────

/// Dust drifting up through the empty space in `area`, a braille dot each, never over `solid`.
pub fn dust(buf: &mut Buffer, area: Rect, solid: &[Rect], t: f32) {
    let (w, h) = (area.width as f32 * 2.0, area.height as f32 * 4.0);
    if w < 1.0 || h < 1.0 {
        return;
    }
    let n = (area.width as u32 * area.height as u32) / 36;
    let mut cells: HashMap<(u16, u16), (u8, f32)> = HashMap::new();
    for i in 0..n {
        let speed = 0.8 + 1.8 * hash(i, 11, 0);
        let sway = (t * (0.3 + 0.4 * hash(i, 12, 0)) + hash(i, 13, 0) * TAU).sin() * 2.2;
        let x = (hash(i, 14, 0) * w + sway).rem_euclid(w);
        let y = (hash(i, 15, 0) * h - t * speed).rem_euclid(h);
        let (cx, cy) = (area.x + (x / 2.0) as u16, area.y + (y / 4.0) as u16);
        if solid.iter().any(|r| r.contains((cx, cy).into())) {
            continue;
        }
        let bit = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]][y as usize % 4][x as usize % 2];
        let glow = 0.5 + 0.5 * (t * (0.7 + hash(i, 16, 0)) + hash(i, 17, 0) * TAU).sin();
        let entry = cells.entry((cx, cy)).or_insert((0, 0.0));
        entry.0 |= bit;
        entry.1 = entry.1.max(glow);
    }
    for ((x, y), (bits, glow)) in cells {
        if let Some(cell) = buf.cell_mut((x, y)).filter(|c| c.symbol() == " ") {
            let dot = char::from_u32(0x2800 + bits as u32).unwrap_or(' ');
            cell.set_char(dot).set_fg(blend(MUTED, BG, 0.16 + 0.36 * glow));
        }
    }
}

// ── on cards ───────────────────────────────────────────────────────────────

fn is_border(symbol: &str) -> bool {
    matches!(symbol, "─" | "│" | "╭" | "╮" | "╰" | "╯" | "━" | "┆" | "┼" | "┿" | "┴" | "┷")
}

/// The border cells of `area`, clockwise from the top-left corner.
fn perimeter(area: Rect) -> Vec<(u16, u16)> {
    let (l, r, t, b) = (area.left(), area.right() - 1, area.top(), area.bottom() - 1);
    let mut p: Vec<(u16, u16)> = (l..=r).map(|x| (x, t)).collect();
    p.extend((t + 1..=b).map(|y| (r, y)));
    p.extend((l..r).rev().map(|x| (x, b)));
    p.extend((t + 1..b).rev().map(|y| (l, y)));
    p
}

/// Light running left to right along the rule in the top row of `area` (`heads` of it, evenly apart),
/// `speed` cells a second, fading in at `strength`. Called on the rules over and under a row together,
/// so they light up side by side, framing it: a job runs there (two when it updates), or it's selected.
pub fn streak(buf: &mut Buffer, area: Rect, t: f32, color: Color, heads: usize, speed: f32, strength: f32) {
    let tail = (area.width as f32 / 4.0).clamp(8.0, 28.0);
    let len = area.width as f32 + tail; // the light leaves on the right before it comes in on the left
    for k in 0..heads {
        let head = (t * speed + len * k as f32 / heads as f32).rem_euclid(len);
        for d in 0..tail as usize {
            let x = head - d as f32;
            if x < 0.0 || x >= area.width as f32 {
                continue;
            }
            if let Some(cell) = buf.cell_mut((area.x + x as u16, area.y)).filter(|c| is_border(c.symbol())) {
                let k = (1.0 - d as f32 / tail).powf(1.3) * strength;
                let lit = if d < 2 { blend(WHITE, color, 0.8) } else { color };
                cell.set_fg(blend(lit, cell.fg, k));
            }
        }
    }
}

/// Color of a check vs an update in motion: teal for a look, amber for a change.
pub fn running_color(update: bool) -> Color {
    if update { AMBER } else { TEAL }
}

/// A band of light sweeping across the text in row `y` of `area`, once per `period` seconds.
pub fn shimmer(buf: &mut Buffer, area: Rect, y: u16, t: f32, period: f32) {
    let width = area.width as f32;
    let pos = (t / period).fract() * (width + 16.0) - 8.0;
    for x in area.left()..area.right() {
        if let Some(cell) =
            buf.cell_mut((x, y)).filter(|c| c.symbol().trim().chars().any(|c| !is_border(&c.to_string())))
        {
            let d = (x - area.x) as f32 - pos;
            let k = (-d * d / 10.0).exp();
            cell.set_fg(blend(WHITE, cell.fg, k * 0.85));
        }
    }
}

/// Recolor a border's cells to `color` (the Update all button breathing).
pub fn tint_border(buf: &mut Buffer, area: Rect, color: Color) {
    for (x, y) in perimeter(area) {
        if let Some(cell) = buf.cell_mut((x, y)).filter(|c| is_border(c.symbol())) {
            cell.set_fg(color);
        }
    }
}

/// Fade the cells of `area` in from the background, `k` from 0 (gone) to 1 (fully drawn).
pub fn fade_in(buf: &mut Buffer, area: Rect, k: f32) {
    if k >= 1.0 {
        return;
    }
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            // An image cell's foreground is the image's id: leave it be.
            if let Some(cell) = buf.cell_mut((x, y)).filter(|c| !super::theme::is_image(c.symbol())) {
                let (f, b) = (blend(cell.fg, BG, k), blend(cell.bg, BG, k));
                cell.set_fg(f).set_bg(b);
            }
        }
    }
}

/// The breathing accent of something that wants attention.
pub fn breathe(t: f32) -> Color {
    blend(blend(WHITE, ACCENT, 0.45), ACCENT, 0.5 + 0.5 * (t * 2.4).sin())
}

/// A wave of light running down `rows` rows every few seconds: how lit `row` is now, 0 to 1.
pub fn ripple(t: f32, row: usize, rows: usize) -> f32 {
    let pos = (t / 3.2).fract() * (rows as f32 + 4.0) - 2.0;
    let d = pos - row as f32;
    (-d * d / 0.9).exp()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ripple_reaches_the_rows_in_order() {
        let peak = |row| {
            (0..320).max_by(|&a, &b| ripple(a as f32 / 100.0, row, 5).total_cmp(&ripple(b as f32 / 100.0, row, 5)))
        };
        assert!(peak(0) < peak(2) && peak(2) < peak(4));
    }

    #[test]
    fn decode_settles_and_hash_is_stable() {
        let mut m = Motion::new(true);
        m.arrive("one-s", "Codex");
        let partial = m.decode("one-s", "Codex", "0.161.0");
        assert_eq!(partial.chars().count(), 7);
        assert!(partial.contains('.'), "punctuation stays put: {partial}");
        m.arrived.insert(("one-s".into(), "Codex".into()), Instant::now() - Duration::from_secs(2));
        assert_eq!(m.decode("one-s", "Codex", "0.161.0"), "0.161.0");
        assert_eq!(Motion::new(false).decode("a", "b", "1.0"), "1.0");
        assert_eq!(hash(1, 2, 3), hash(1, 2, 3));
        assert!((0.0..1.0).contains(&hash(9, 9, 9)));
    }

    #[test]
    fn intro_draws_then_dissolves_away() {
        let area = Rect::new(0, 0, 80, 24);
        let hosts = vec![("one-s".to_string(), Status::Ok), ("mdr".to_string(), Status::Running)];
        let mut buf = Buffer::empty(area);
        intro(&mut buf, 1.2, &hosts);
        let text: String = buf.content.iter().map(|c| c.symbol()).collect();
        assert!(text.contains('▀') && text.contains("one-s") && text.contains("kept current"));
        let mut buf = Buffer::empty(area);
        intro(&mut buf, INTRO.as_secs_f32(), &hosts);
        assert!(buf.content.iter().all(|c| c.symbol() == " "), "fully dissolved into what was underneath");
    }

    #[test]
    fn dust_stays_out_of_solid_areas() {
        let area = Rect::new(0, 0, 60, 20);
        let solid = [Rect::new(0, 0, 60, 10)];
        let mut buf = Buffer::empty(area);
        dust(&mut buf, area, &solid, 3.0);
        let lit: Vec<_> = buf.content.iter().enumerate().filter(|(_, c)| c.symbol() != " ").collect();
        assert!(!lit.is_empty());
        assert!(lit.iter().all(|(i, _)| *i / 60 >= 10));
    }
}
