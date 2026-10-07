//! Rendering: the app's state onto a ratatui buffer. Also registers where things are
//! clickable (`App::hits`) so the mouse handler needs no layout of its own.
use std::sync::LazyLock;
use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Clear, Scrollbar, ScrollbarOrientation, ScrollbarState, StatefulWidget, Widget,
};
use regex::Regex;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::app::{Act, App, Changes, Confirm, Hit, Installer, Item, Menu, Modal, Palette, Servers, Sev, Toast};
use super::flow::{Flow, Kind, State};
use super::logos::{self, Logos};
use super::motion;
use super::theme::*;
use crate::changelog::Release;
use crate::model::{COMPONENTS, Host, Status, StepState, newer, short, updates, versions_detail};

/// A logo's width, in cells (`logos::MAX_WIDTH`).
pub const TILE: u16 = 9;
/// One tool's column: its dashed line, the cell a mark hangs in, its version centered, room after.
const COL: u16 = 15;
const HEADER: u16 = 3;
/// The column heads: a blank row, logos, the newest versions under them, right on the first rule.
const HEAD: u16 = logos::HEIGHT + 2;

// ── geometry ───────────────────────────────────────────────────────────────

/// Where the column heads, the cards and the output panel go, from the screen size and what is in them.
pub struct Geo {
    /// Each server's row: x on screen, y down the (scrolled) content. All are `card_h` tall: the rule
    /// over it, then its line of versions, with a blank row above and below when there's room.
    pub cards: Vec<Rect>,
    pub card_h: u16,
    /// The rows and the rule that closes the table under the last.
    pub content_h: u16,
    /// The logos over the columns; they stay put while the cards scroll under them.
    pub head: Rect,
    pub view: Rect,
    pub out: Rect,
    pub table: Table,
}

#[cfg(test)]
impl Geo {
    /// Cards in the first row.
    pub fn cols(&self) -> usize {
        self.cards.iter().filter(|c| c.y == self.cards[0].y).count()
    }
}

/// The columns every card shares, so versions line up down the screen: name, uptime, a column
/// per tool any server has, then the CPU and disk meters. Offsets count from a card's left border.
#[derive(Debug, Clone, Default)]
pub struct Table {
    name: u16,
    /// 0 once there's no room for it.
    up: u16,
    tools: Vec<&'static str>,
    col: u16,
    /// Each meter's width; 0 once there's no room for them.
    meter: u16,
}

impl Table {
    /// The name, after the server's status mark when it has one.
    const NAME_X: u16 = 4;

    fn up_x(&self) -> u16 {
        Self::NAME_X + self.name + 2
    }

    fn tool_x(&self, k: usize) -> u16 {
        self.up_x() + if self.up > 0 { self.up + 2 } else { 0 } + k as u16 * self.col
    }

    /// Where the CPU meter starts, past the last column's closing line; the disk meter follows two cells after it.
    fn meter_x(&self) -> u16 {
        self.tool_x(self.tools.len()) + 2
    }

    /// The columns' dashed lines: one opening each tool's column, one closing the last.
    fn lines(&self) -> Vec<u16> {
        if self.tools.is_empty() { vec![] } else { (0..=self.tools.len()).map(|k| self.tool_x(k)).collect() }
    }

    fn width(&self) -> u16 {
        let end = if self.meter > 0 { self.meter_x() + 2 * self.meter + 2 } else { self.tool_x(self.tools.len()) };
        // Room for a message where the columns would be (no T3, or no connection).
        (end + 2).max(self.up_x() + 40)
    }
}

/// The tools a card shows a version for, in order.
fn present(h: &Host) -> Vec<&'static str> {
    COMPONENTS
        .iter()
        .map(|(n, _)| *n)
        .filter(|n| h.installed.contains(*n) && !matches!(h.steps.get(*n), Some((StepState::Skip, _))))
        .collect()
}

/// A card with a message where its versions would be: no T3 there, or it never got as far as a step.
fn message_card(h: &Host) -> bool {
    h.status == Status::Ok && crate::model::no_t3(h) || h.status == Status::Failed && h.steps.is_empty()
}

fn uptime(h: &Host) -> Option<&str> {
    sys_parts(&h.sys).find(|(k, _)| *k == "up").map(|(_, v)| v)
}

/// The widest table that fits `avail`: narrower meters, columns too tight for a whole nightly (its build
/// stays), no meters, no uptime, tighter columns still, shorter names. Past that, the rows clip on the right.
fn table(app: &App, avail: u16) -> Table {
    let tools = COMPONENTS
        .iter()
        .map(|(n, _)| *n)
        .filter(|n| app.hosts.iter().any(|h| !message_card(h) && present(h).contains(n)))
        .collect();
    let name = app.hosts.iter().map(|h| h.name.width()).max().unwrap_or(0).clamp(6, 20) as u16;
    let up = app.hosts.iter().filter_map(uptime).map(|u| u.width()).max().unwrap_or(0) as u16;
    let mut t = Table { name, up, tools, col: COL, meter: 10 };
    let shrink: [fn(&mut Table); 7] = [
        |t| t.meter = 8,
        |t| t.col = 13,
        |t| t.meter = 6,
        |t| t.meter = 0,
        |t| t.up = 0,
        |t| t.col = 12,
        |t| t.name = t.name.min(10),
    ];
    for step in shrink {
        if t.width() <= avail {
            break;
        }
        step(&mut t);
    }
    t
}

pub fn geometry(app: &App) -> Geo {
    let (w, h) = app.size;
    let body = h.saturating_sub(HEADER + 1);
    let out_h = if app.show_output { (body * 45 / 100).max(5).min(body) } else { 0 };
    let head_h = if app.hosts.is_empty() { 0 } else { HEAD.min(body - out_h) };
    let head = Rect::new(0, HEADER, w, head_h);
    let view = Rect::new(0, HEADER + head_h, w, body - out_h - head_h);
    let out = Rect::new(0, HEADER + body - out_h, w, out_h);
    let table = table(app, w.saturating_sub(4));
    let cw = table.width().min(w.saturating_sub(2)).max(1);
    let left = w.saturating_sub(cw) / 2;
    // Tall rows, a blank row above and below their versions, when they fit without scrolling; slim otherwise.
    let n = app.hosts.len() as u16;
    let card_h = if n * 4 < view.height { 4 } else { 2 };
    let cards = (0..n).map(|i| Rect::new(left, i * card_h, cw, card_h)).collect();
    Geo { cards, card_h, content_h: n * card_h + 1, head, view, out, table }
}

pub fn max_scroll(app: &App) -> u16 {
    let g = geometry(app);
    g.content_h.saturating_sub(g.view.height)
}

/// The scroll offset that shows card `i`, moving as little as possible.
pub fn scroll_to(app: &App, i: usize) -> u16 {
    let g = geometry(app);
    let top = g.cards.get(i).map_or(1, |c| c.y);
    let bottom = top + g.card_h + 1;
    let mut scroll = app.scroll;
    if top.saturating_sub(1) < scroll {
        scroll = top.saturating_sub(1);
    }
    if bottom > scroll + g.view.height {
        scroll = bottom - g.view.height;
    }
    scroll.min(max_scroll(app))
}

// ── text helpers ───────────────────────────────────────────────────────────

pub fn truncate(s: &str, w: usize) -> String {
    if s.width() <= w {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if used + cw + 1 > w {
            break;
        }
        out.push(c);
        used += cw;
    }
    if w > 0 {
        out.push('…');
    }
    out
}

/// `s` broken into rows at most `w` wide, at spaces where it can.
pub fn wrap(s: &str, w: usize) -> Vec<String> {
    let w = w.max(1);
    let (mut rows, mut cur, mut used) = (vec![], String::new(), 0);
    for word in s.split(' ') {
        let ww = word.width();
        if used > 0 && used + 1 + ww > w {
            rows.push(std::mem::take(&mut cur));
            used = 0;
        }
        if ww > w {
            for ch in word.chars() {
                let c = ch.width().unwrap_or(0);
                if used + c > w {
                    rows.push(std::mem::take(&mut cur));
                    used = 0;
                }
                cur.push(ch);
                used += c;
            }
            continue;
        }
        if used > 0 {
            cur.push(' ');
            used += 1;
        }
        cur.push_str(word);
        used += ww;
    }
    rows.push(cur);
    rows
}

/// Spans cut to `w` columns, ending in an ellipsis if they were cut.
fn clip(spans: Vec<Span<'static>>, w: usize) -> Line<'static> {
    let total: usize = spans.iter().map(|s| s.content.width()).sum();
    if total <= w {
        return Line::from(spans);
    }
    let (mut out, mut left) = (vec![], w.saturating_sub(1));
    for span in spans {
        let sw = span.content.width();
        if sw <= left {
            left -= sw;
            out.push(span);
            continue;
        }
        let mut cut = String::new();
        for c in span.content.chars() {
            let cw = c.width().unwrap_or(0);
            if cw > left {
                break;
            }
            cut.push(c);
            left -= cw;
        }
        out.push(Span::styled(cut, span.style));
        break;
    }
    out.push(Span::styled("…", out.last().map_or(Style::new(), |s| s.style)));
    Line::from(out)
}

fn spinner(t: Duration) -> char {
    SPIN[(t.as_millis() / 100 % 10) as usize]
}

/// (icon, color, label) summarising a host, e.g. ('✓', GREEN, 'healthy').
pub fn status(h: &Host, queued: bool, t: Duration) -> (String, ratatui::style::Color, &'static str) {
    match h.status {
        Status::Running => {
            let doing = match h.mode {
                crate::model::Mode::Check => "checking",
                crate::model::Mode::Update => "updating",
                crate::model::Mode::Remove => "removing",
            };
            (spinner(t).to_string(), AMBER, doing)
        }
        _ if queued => ("◌".into(), DIM, "queued"),
        Status::Ok if crate::model::no_t3(h) => ("○".into(), AMBER, "no T3"),
        Status::Idle => ("·".into(), DIM, "not checked"),
        Status::Failed => ("✗".into(), RED, "failed"),
        Status::Ok => ("✓".into(), GREEN, if h.mode == crate::model::Mode::Update { "updated" } else { "healthy" }),
    }
}

fn fill(buf: &mut Buffer, area: Rect, bg: ratatui::style::Color) {
    Clear.render(area, buf);
    buf.set_style(area, Style::new().bg(bg).fg(TEXT));
}

fn put(buf: &mut Buffer, x: u16, y: u16, w: u16, line: &Line) {
    if y < buf.area.bottom() && x < buf.area.right() {
        buf.set_line(x, y, line, w.min(buf.area.right() - x));
    }
}

fn centered(line: Line<'static>, w: u16) -> (u16, Line<'static>) {
    let lw = line.width() as u16;
    (w.saturating_sub(lw) / 2, line)
}

// ── entry ──────────────────────────────────────────────────────────────────

pub fn draw(frame: &mut ratatui::Frame, app: &mut App, logos: &Logos) {
    render(app, logos, frame.buffer_mut());
}

pub fn render(app: &mut App, logos: &Logos, buf: &mut Buffer) {
    let area = buf.area;
    if area.is_empty() {
        return;
    }
    app.size = (area.width, area.height);
    app.hits.clear();
    app.modal_rect = Rect::default();
    buf.set_style(area, Style::new().bg(BG).fg(TEXT));
    app.scroll = app.scroll.min(max_scroll(app));
    let dim_logos = app.modal.is_some();
    let t = app.clock().as_secs_f32();
    header(app, buf);
    cards(app, logos, buf, dim_logos);
    if app.motion.on {
        // Through the table too (it only lands on blank cells), but never on the logos: an image there
        // would be cut by a dot.
        let g = geometry(app);
        let logos = Rect::new(0, g.head.y + 1, area.width, logos::HEIGHT.min(g.head.height));
        motion::dust(buf, g.head.union(g.view), &[logos], t);
    }
    if app.show_output {
        output(app, buf);
    }
    footer(app, buf);
    if app.modal.is_some() {
        let since = *app.motion.modal_since.get_or_insert_with(std::time::Instant::now);
        // Toasts that were up before the window opened stay behind it.
        toasts(app, buf, Some(since), false);
        let k = if app.motion.on { motion::ease_out(since.elapsed().as_secs_f32() / motion::FADE) } else { 1.0 };
        dim(buf, 0.65 * k);
        modal(app, buf, logos);
        motion::fade_in(buf, app.modal_rect, k);
    } else {
        app.motion.modal_since = None;
        tooltip(app, buf);
    }
    // New ones on top of everything, beside an open window when there's room for that.
    toasts(app, buf, app.modal.is_some().then_some(app.motion.modal_since).flatten(), true);
    if let Some(t) = app.motion.intro_at() {
        let hosts: Vec<_> = app.hosts.iter().map(|h| (h.name.clone(), h.status)).collect();
        motion::intro(buf, t, &hosts);
    }
}

// ── header ─────────────────────────────────────────────────────────────────

/// Space between the screen's edge and the header's and footer's content.
const MARGIN: u16 = 2;

/// 't3up' in half blocks, two rows tall.
const WORDMARK: [&str; 2] = ["▄█▄ ▀██ █ █ █▀█", " █▄ ▄▄█ █▄█ █▀▀"];

fn header(app: &mut App, buf: &mut Buffer) {
    let w = buf.area.width;
    let t = app.clock().as_secs_f32();
    // The name, big when there's room; the text and the button sit on its bottom row.
    let y = HEADER - 1;
    let big = w >= 70;
    let name_w = if big {
        for (k, row) in WORDMARK.iter().enumerate() {
            put(buf, MARGIN, y - 1 + k as u16, row.width() as u16, &Line::styled(*row, bold(ACCENT)));
        }
        let phase = t.rem_euclid(6.0);
        if app.motion.on && phase < 1.2 {
            // Now and then, a light passes over it.
            for row in [y - 1, y] {
                motion::shimmer(buf, Rect::new(MARGIN, y - 1, WORDMARK[0].width() as u16, 2), row, phase, 1.2);
            }
        }
        WORDMARK[0].width() as u16
    } else {
        put(buf, MARGIN, y, 4, &Line::styled("t3up", bold(ACCENT)));
        4
    };
    // A filled pill, split: the button updates everything and says how many servers that brings
    // forward; its '⋯' end opens the menu of what to update.
    let behind =
        app.hosts.iter().filter(|h| h.status == Status::Ok && !updates(h, &app.latest, &app.target).is_empty()).count();
    let label = match behind {
        0 => "↑ Update all".to_string(),
        1 => "↑ Update 1 server".to_string(),
        n => format!("↑ Update {n} servers"),
    };
    let main_w = label.width() as u16 + 3; // a rounded end, then a cell of padding each side
    let more_w = 5; // the divider, padding, '⋯', padding, a rounded end
    let bw = main_w + more_w;
    let shown = w >= 2 * MARGIN + name_w + 2 + bw;
    let button = Rect::new(w.saturating_sub(bw + MARGIN), y, bw, 1);
    let more_x = button.x + main_w;
    let enabled = !app.hosts.is_empty();
    if shown {
        let segment = |hit| {
            let hovered = app.hover.as_ref().is_some_and(|h| h.hit == Some(hit));
            let alpha = match (enabled, hovered) {
                (false, _) => 0.0,
                (true, true) => 0.42,
                // Updates waiting: it breathes, gently.
                (true, false) if behind > 0 && app.motion.on => 0.2 + 0.12 * (0.5 + 0.5 * (t * 2.2).sin()),
                (true, false) => 0.22,
            };
            let bg = if enabled { blend(ACCENT, BG, alpha) } else { blend(FAINT, BG, 0.35) };
            let color = if enabled { if hovered { TEXT } else { ACCENT } } else { DIM };
            (bg, color)
        };
        let (main_bg, main_fg) = segment(Hit::UpdateAll);
        let (more_bg, more_fg) = segment(Hit::UpdateMenu);
        buf.set_style(Rect::new(button.x, y, main_w, 1), Style::new().bg(main_bg));
        buf.set_style(Rect::new(more_x, y, more_w, 1), Style::new().bg(more_bg));
        put(buf, button.x + 2, y, main_w - 2, &Line::styled(label, bold(main_fg)));
        put(buf, more_x + 2, y, 1, &Line::styled("⋯", bold(more_fg)));
        if let Some(cell) = buf.cell_mut((more_x, y)) {
            // On the cell's left edge, so each half's color ends right at it (a '│' sits mid-cell).
            cell.set_symbol("▏").set_fg(BG);
        }
        // Rounded ends: Powerline's half circles, which Ghostty, kitty and WezTerm draw themselves.
        for (x, cap, bg) in [(button.x, "\u{e0b6}", main_bg), (button.right() - 1, "\u{e0b4}", more_bg)] {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_symbol(cap).set_style(Style::new().fg(bg).bg(BG));
            }
        }
        // Over the button: how long updating everyone takes, counting while it runs.
        if let Some(r) = &app.rollout {
            let (mark, color, text) = match r.took {
                None => {
                    (spinner(app.clock()).to_string(), AMBER, format!("updating · {}", minutes(r.started.elapsed())))
                }
                Some((took, false)) => ("✓".into(), GREEN, format!("updated in {}", minutes(took))),
                Some((took, true)) => ("✗".into(), RED, format!("done in {}", minutes(took))),
            };
            let line = Line::from(vec![Span::styled(format!("{mark} "), fg(color)), Span::styled(text, fg(MUTED))]);
            let lw = line.width() as u16;
            put(buf, button.x + bw.saturating_sub(lw) / 2, y - 1, bw, &line);
        }
        if enabled {
            // The whole height of the header is clickable, not just the button's row.
            app.hits.push((Rect::new(button.x, 0, main_w, HEADER), Hit::UpdateAll));
            app.hits.push((Rect::new(more_x, 0, more_w, HEADER), Hit::UpdateMenu));
        }
    }
    let left_x = MARGIN + name_w + 3;
    let right_edge = if shown { button.x.saturating_sub(3) } else { w.saturating_sub(MARGIN) };
    let avail = right_edge.saturating_sub(left_x) as usize;
    let (left, right) = header_text(app, avail);
    let right_w = right.width() as u16;
    put(buf, left_x, y, avail as u16, &left);
    put(buf, right_edge - right_w.min(avail as u16), y, right_w, &right);
}

/// '42s', '3m 07s'.
fn minutes(d: Duration) -> String {
    match d.as_secs() {
        s @ 0..60 => format!("{s}s"),
        s => format!("{}m {:02}s", s / 60, s % 60),
    }
}

/// Beside the name: the pinned version and a newer t3up; beside the button, what's wrong (how many can
/// update is on the button). The widest layout that fits: narrow terminals lose the long words, then the
/// pinned version.
pub fn header_text(app: &App, width: usize) -> (Line<'static>, Line<'static>) {
    let pinned = if app.target.is_empty() {
        vec![]
    } else {
        vec![Span::styled("   Installs ", fg(DIM)), Span::styled(app.target.clone(), fg(AMBER))]
    };
    let count = |f: &dyn Fn(&Host) -> bool| app.hosts.iter().filter(|h| f(h)).count();
    let counts = [
        (count(&|h| h.status == Status::Ok && crate::model::no_t3(h)), AMBER, "no T3", "○"),
        (count(&|h| h.status == Status::Failed), RED, "failed", "✗"),
    ];
    let tally = |short: bool| -> Vec<Span<'static>> {
        counts
            .iter()
            .filter(|c| c.0 > 0)
            .map(|&(n, color, label, mark)| {
                let text = if short { format!("  {mark} {n}") } else { format!("  {mark} {n} {label}") };
                Span::styled(text, fg(color))
            })
            .collect()
    };
    let cat = |parts: &[&Vec<Span<'static>>]| -> Vec<Span<'static>> {
        let mut spans: Vec<Span<'static>> = parts.iter().flat_map(|p| p.iter().cloned()).collect();
        if let Some(first) = spans.first_mut() {
            first.content = first.content.trim_start().to_string().into(); // the header spaces it from the name
        }
        spans
    };
    // A newer t3up: worth a word in every layout.
    let (mine, mine_terse) = match (&app.self_update, app.self_updating) {
        (Some(_), true) => {
            (vec![Span::styled("   updating t3up…", fg(ACCENT))], vec![Span::styled("   updating…", fg(ACCENT))])
        }
        (Some(v), false) => (
            vec![Span::styled(format!("   t3up {v} · U to update"), fg(ACCENT))],
            vec![Span::styled("   U update t3up", fg(ACCENT))],
        ),
        (None, _) => (vec![], vec![]),
    };
    let layouts = [
        (cat(&[&pinned, &mine]), false),
        (cat(&[&pinned, &mine]), true),
        (cat(&[&pinned, &mine_terse]), true),
        (cat(&[&mine_terse]), true),
    ];
    let mut chosen = None;
    for (parts, short) in &layouts {
        let (l, r) = (Line::from(parts.clone()), Line::from(tally(*short)));
        if l.width() + r.width() <= width {
            chosen = Some((l, r));
            break;
        }
    }
    // Nothing fits with the counts: they go.
    chosen.unwrap_or_else(|| (Line::from(cat(&[])), Line::default()))
}

// ── footer ─────────────────────────────────────────────────────────────────

fn footer(app: &App, buf: &mut Buffer) {
    let (w, y) = (buf.area.width, buf.area.height.saturating_sub(1));
    let mut keys = vec![
        ("enter", "actions"),
        ("a", "update all"),
        ("r", "refresh"),
        ("c", "what's new"),
        ("v", "T3 version"),
        ("e", "servers"),
        ("l", "output"),
        ("?", "help"),
        ("q", "quit"),
    ];
    let width = |keys: &[(&str, &str)]| {
        keys.iter().map(|(k, l)| k.width() + l.width() + 1).sum::<usize>() + keys.len().saturating_sub(1) * 3
    };
    let running: Vec<&Host> = app.hosts.iter().filter(|h| h.running()).collect();
    let mut status = vec![];
    if !running.is_empty() {
        let verb = if running.iter().any(|h| h.mode == crate::model::Mode::Update) {
            "Updating"
        } else if running.iter().any(|h| h.mode == crate::model::Mode::Remove) {
            "Removing from"
        } else {
            "Checking"
        };
        let n = running.len();
        let secs = running.iter().map(|h| h.took().as_secs()).max().unwrap_or(0);
        status = vec![
            Span::styled(format!("{} ", spinner(app.clock())), fg(AMBER)),
            Span::styled(format!("{verb} {n} server{}", if n == 1 { "" } else { "s" }), fg(MUTED)),
            Span::styled(format!(" · {secs}s"), fg(DIM)),
        ];
    }
    // The status, then three cells, then the hints; the screen's margin on both sides.
    let status_w = Line::from(status.clone()).width();
    let lead = if status_w > 0 { status_w + 3 } else { 0 };
    let room = (w as usize).saturating_sub(2 * MARGIN as usize);
    for drop in ["output", "servers", "T3 version", "what's new", "refresh", "update all", "actions", "help"] {
        if lead + width(&keys) <= room {
            break;
        }
        keys.retain(|(_, l)| *l != drop);
    }
    let mut spans = status;
    if lead > 0 {
        spans.push(Span::raw("   "));
    }
    for (i, (k, l)) in keys.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", fg(FAINT)));
        }
        spans.push(Span::styled(*k, fg(ACCENT)));
        spans.push(Span::styled(format!(" {l}"), fg(MUTED)));
    }
    put(buf, MARGIN, y, room as u16, &Line::from(spans));
    if app.motion.on && status_w > 0 {
        motion::shimmer(buf, Rect::new(MARGIN, y, status_w as u16, 1), y, app.clock().as_secs_f32(), 1.6);
    }
    let palette = Line::from(vec![Span::styled("^p", fg(ACCENT)), Span::styled(" palette", fg(MUTED))]);
    let pw = palette.width();
    if lead + width(&keys) + 3 + pw <= room && !app.hosts.is_empty() {
        put(buf, w - MARGIN - pw as u16, y, pw as u16, &palette);
    }
}

// ── cards ──────────────────────────────────────────────────────────────────

fn cards(app: &mut App, logos: &Logos, buf: &mut Buffer, dim_logos: bool) {
    let g = geometry(app);
    if app.hosts.is_empty() {
        let line = Line::styled("No servers yet. Press e to add some.", fg(DIM));
        let y = g.view.y + g.view.height / 2;
        let (x, line) = centered(line, g.view.width);
        put(buf, x, y, g.view.width, &line);
        return;
    }
    head(app, &g, logos, buf, dim_logos);
    for (i, place) in g.cards.iter().enumerate() {
        let x = place.x;
        let y = g.view.y as i32 + place.y as i32 - app.scroll as i32;
        if y + g.card_h as i32 <= g.view.y as i32 || y >= g.view.bottom() as i32 {
            continue;
        }
        let mut card = Buffer::empty(Rect::new(0, 0, place.width, g.card_h));
        card.set_style(card.area, Style::new().bg(BG).fg(TEXT));
        let cells = draw_card(app, &g, i, &mut card);
        let visible = blit(&card, buf, x, y, g.view);
        app.hits.push((visible, Hit::Card(i)));
        // Each tool's cell, on top of its card: tapping one opens that tool's menu.
        for (cell, name) in cells {
            let top = y + cell.y as i32;
            if top >= g.view.y as i32 && top + cell.height as i32 <= g.view.bottom() as i32 {
                app.hits.push((Rect::new(x + cell.x, top as u16, cell.width, cell.height), Hit::Tool(i, name)));
            }
        }
    }
    // The rule that closes the table, under the last server.
    let (Some(last), y) = (g.cards.last(), (g.view.y + g.content_h - 1) as i32 - app.scroll as i32) else { return };
    if (g.view.y as i32..g.view.bottom() as i32).contains(&y) {
        let mut close = Buffer::empty(Rect::new(0, 0, last.width, 1));
        let heavy = app.hosts.last().is_some_and(|h| h.local);
        rule(&mut close, &g.table.lines(), heavy, LINE, if heavy { "┷" } else { "┴" });
        blit(&close, buf, last.x, y, g.view);
    }
    // Light on the rules over and under a running (or the selected) row at once, framing it. After every
    // row: the rule under one is drawn with the next.
    for (i, (h, place)) in app.hosts.iter().zip(&g.cards).enumerate().filter(|_| app.motion.on) {
        let t = app.clock().as_secs_f32();
        let (color, heads, speed, strength, t) = if h.running() {
            let update = h.mode != crate::model::Mode::Check; // a change: two lights
            (motion::running_color(update), if update { 2 } else { 1 }, 42.0, 1.0, t + i as f32 * 0.37)
        } else if i == app.selected {
            (blend(TEXT, ACCENT, 0.6), 1, 9.0, 0.55, t) // the idle gleam
        } else {
            continue;
        };
        let top = (g.view.y + place.y) as i32 - app.scroll as i32;
        for y in
            [top, top + g.card_h as i32].into_iter().filter(|y| (g.view.y as i32..g.view.bottom() as i32).contains(y))
        {
            motion::streak(buf, Rect::new(place.x, y as u16, place.width, 1), t, color, heads, speed, strength);
        }
    }
    if g.content_h > g.view.height {
        let mut state = ScrollbarState::new((g.content_h - g.view.height) as usize).position(app.scroll as usize);
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(None)
            .thumb_symbol("┃")
            .thumb_style(fg(FAINT))
            .render(g.view, buf, &mut state);
    }
}

/// A rule across the top row of `buf`, heavy (around this machine) or light, crossing the columns'
/// dashed lines at `lines` with `cross`.
fn rule(buf: &mut Buffer, lines: &[u16], heavy: bool, color: ratatui::style::Color, cross: &str) {
    for x in 0..buf.area.width {
        let symbol = if lines.contains(&x) {
            cross
        } else if heavy {
            "━"
        } else {
            "─"
        };
        buf[(x, 0)].set_symbol(symbol).set_fg(color);
    }
}

/// Copy the part of `src` that lands inside `clip` when placed at (x, y); returns that part's rect.
fn blit(src: &Buffer, dst: &mut Buffer, x: u16, y: i32, clip: Rect) -> Rect {
    let top = y.max(clip.top() as i32);
    let bottom = (y + src.area.height as i32).min(clip.bottom() as i32);
    for dy in top..bottom {
        for sx in 0..src.area.width {
            let (cx, sy) = (x + sx, (dy - y) as u16);
            if cx >= clip.right() {
                continue;
            }
            if let (Some(from), Some(to)) = (src.cell((sx, sy)), dst.cell_mut((cx, dy as u16))) {
                *to = from.clone();
            }
        }
    }
    Rect::new(x, top as u16, src.area.width.min(clip.right().saturating_sub(x)), (bottom - top).max(0) as u16)
}

/// The version an update would bring `tool` to: the pinned T3, else the newest any server is
/// behind on, else the newest there is.
fn newest(app: &App, tool: &str) -> Option<String> {
    if tool == "T3" && !app.target.is_empty() {
        return Some(app.target.clone());
    }
    app.hosts
        .iter()
        .find_map(|h| updates(h, &app.latest, &app.target).into_iter().find(|(n, _)| n == tool).map(|(_, v)| v))
        .or_else(|| app.latest.get(tool).cloned())
}

/// Text centered in a column `col` wide, past its dashed line and the cell a mark (`MARKS`) hangs in,
/// and the logo that sits over it lined up to the pixel: (text's x, logo's x, whether the text is an
/// odd width), from the column's left.
fn over(col: u16, w: u16) -> (u16, u16, bool) {
    let x = 2 + col.saturating_sub(w + 2) / 2;
    let twice_middle = 2 * x + w;
    let odd = twice_middle % 2 == 1;
    (x, twice_middle.saturating_sub(if odd { TILE } else { TILE - 1 }) / 2, odd)
}

/// The column heads: each tool's logo with the latest version under it, so a server behind on one
/// shows it right below; the meters' names beside the logos.
fn head(app: &App, g: &Geo, logos: &Logos, buf: &mut Buffer, dim: bool) {
    let (Some(card), true) = (g.cards.first(), g.head.height >= HEAD) else { return };
    let t = &g.table;
    let y = g.head.y + 1 + logos::HEIGHT;
    let mut any = false;
    for (k, name) in t.tools.iter().enumerate() {
        let x = card.x + t.tool_x(k);
        let text = fit(&newest(app, name).map(|v| version_label(&v)).unwrap_or_default(), t.col as usize - 3);
        any |= !text.is_empty();
        let (tx, lx, odd) = over(t.col, text.width() as u16);
        let logo = Rect::new(x + lx, g.head.y + 1, TILE, logos::HEIGHT).intersection(buf.area);
        logos.draw_tile(name, odd, logo, buf, dim);
        put(buf, x + tx, y, t.col, &Line::styled(text, fg(MUTED)));
    }
    // The columns' dashed lines, from the top of the logos down into the rows.
    for x in t.lines() {
        for y in g.head.y + 1..g.head.bottom() {
            put(buf, card.x + x, y, 1, &Line::styled("┆", fg(LINE)));
        }
    }
    if any {
        // In the names' column: these versions are a row of their own, the one to match.
        put(buf, card.x + Table::NAME_X, y, t.name, &Line::styled("latest", fg(DIM)));
    }
    if t.meter > 0 {
        for (k, name) in ["cpu", "disk"].into_iter().enumerate() {
            let x = card.x + t.meter_x() + k as u16 * (t.meter + 2);
            put(buf, x + (t.meter - name.len() as u16) / 2, y - 1, t.meter, &Line::styled(name, fg(DIM)));
        }
    }
}

/// What hangs left of a version: '↑' newer to install, '✓' just updated.
const MARKS: [char; 2] = ['↑', '✓'];

/// `text` in `w` cells: a nightly that won't fit keeps its build, the part that changes: '#2623'.
fn fit(text: &str, w: usize) -> String {
    match text.split_once(" #") {
        Some((version, build)) if text.width() > w => {
            let mark = version.chars().next().filter(|c| MARKS.contains(c)).map(String::from).unwrap_or_default();
            truncate(&format!("{mark}#{build}"), w)
        }
        _ => truncate(text, w),
    }
}

/// A tool's version in its cell: '0.0.46 #2623', '↑0.0.46 #2623' in the accent when there's newer,
/// '✓2.1.289' after an update, a spinner while it runs, or what it waits on.
fn cell(app: &App, h: &Host, name: &str, behind: bool) -> Span<'static> {
    let (state, value) = h.steps.get(name).map_or((None, ""), |(s, v)| (Some(*s), v.as_str()));
    let shown = h.current.get(name).map_or_else(|| value.to_string(), |v| version_label(v));
    let span = if (state == Some(StepState::Begin) && h.running()) || (h.local && name == "T3" && app.desktop_updating)
    {
        Span::styled(spinner(app.clock()).to_string(), fg(AMBER))
    } else if matches!(state, Some(StepState::Begin | StepState::Fail)) {
        Span::styled("✗ failed", fg(RED))
    } else if h.auth.iter().any(|a| a == name) {
        Span::styled("sign in", fg(AMBER))
    } else if value.contains(" → ") {
        Span::styled(format!("✓{shown}"), bold(GREEN))
    } else if shown.is_empty() {
        Span::styled("—", fg(FAINT))
    } else if behind {
        Span::styled(format!("↑{shown}"), fg(ACCENT))
    } else {
        Span::styled(shown, fg(if state.is_some() { TEXT } else { FAINT }))
    };
    let text = span.content.replace("-nightly", "");
    let text = if state == Some(StepState::Done) { app.motion.decode(&h.name, name, &text) } else { text };
    Span::styled(text, span.style)
}

/// Draw card `i` into `buf`; returns where its tools' cells are (in the card), to tap.
fn draw_card(app: &App, g: &Geo, i: usize, buf: &mut Buffer) -> Vec<(Rect, &'static str)> {
    let mut cells = vec![];
    let h = &app.hosts[i];
    let t = app.clock();
    let queued = app.queued.contains(&h.name);
    let selected = i == app.selected;
    let (icon, color, label) = status(h, queued, t);
    let new = updates(h, &app.latest, &app.target);
    // Healthy is the norm and says nothing: the status shows only what isn't. A run says so by the
    // spinner before the name and in the footer.
    let fine = h.status == Status::Ok && !queued && !crate::model::no_t3(h);
    let mut sub = vec![];
    if !fine && !h.running() {
        sub.push(Span::styled(format!(" {icon} {label} "), fg(color)));
    }
    // What needs attention, on the right under the versions (on the rule when the row is slim).
    let mut alert: Vec<Span<'static>> = vec![];
    if let Some(n) = h.busy.filter(|&n| n > 0) {
        alert.push(Span::styled(format!(" ● {n} agent{} live ", if n == 1 { "" } else { "s" }), fg(AMBER)));
    }
    if !h.rollback.is_empty() {
        alert.push(Span::styled(format!(" ↩ rolled back {} ", h.rollback), fg(AMBER)));
    }
    if fine && h.mode == crate::model::Mode::Update {
        alert.push(Span::styled(" ✓ updated ", fg(GREEN)));
    }
    let w = buf.area.width as usize;
    let missing = h.status == Status::Ok && crate::model::no_t3(h);
    // On the left: what went wrong, or what to do about it.
    let note = if message_card(h) {
        let hint = if missing { "enter → install T3 (latest nightly)" } else { "enter → retry or open a terminal" };
        Span::styled(format!(" {hint} "), fg(DIM))
    } else if !h.error.is_empty() {
        Span::styled(format!(" ✗ {} ", h.error), fg(RED))
    } else {
        Span::raw("")
    };
    let tb = &g.table;
    // The rule over the row, heavier around this machine (the top row).
    let heavy = h.local || i > 0 && app.hosts[i - 1].local;
    rule(buf, &tb.lines(), heavy, LINE, if heavy { "┿" } else { "┼" });
    // The selected row is lit under its rule; a row flashes as its run ends (`Motion::border`).
    let base = if selected { SURFACE } else { BG };
    let lit = blend(app.motion.border(&h.name, base), base, 0.25);
    buf.set_style(Rect::new(0, 1, buf.area.width, g.card_h - 1), Style::new().bg(lit));
    for x in tb.lines() {
        for y in 1..g.card_h {
            put(buf, x, y, 1, &Line::styled("┆", fg(LINE)));
        }
    }
    let (note_x, note_y) = if g.card_h > 2 { (Table::NAME_X - 3, g.card_h - 1) } else { (1, 0) };
    let right = clip([alert, sub].concat(), w.saturating_sub(4));
    let right_w = right.width() as u16;
    put(buf, buf.area.width.saturating_sub(right_w + 1), note_y, right_w, &right);
    let room = buf.area.width.saturating_sub(note_x + right_w + 2);
    put(buf, note_x, note_y, room, &clip(vec![note], room as usize));

    let y = 1 + (g.card_h - 1) / 2;
    // A mark before the name only when something's off: well is the norm and says nothing.
    if !fine {
        put(buf, Table::NAME_X - 2, y, 1, &Line::styled(icon.clone(), fg(color)));
    }
    let name = truncate(&h.name, tb.name as usize);
    put(buf, Table::NAME_X, y, tb.name, &Line::styled(name, bold(if selected { ACCENT } else { TEXT })));
    if let Some(up) = uptime(h).filter(|_| tb.up > 0) {
        put(buf, tb.up_x() + tb.up - up.width() as u16, y, tb.up, &Line::styled(up.to_string(), fg(DIM)));
    }
    if message_card(h) {
        // Never got as far as a step, or there's no T3 here: say so across the columns.
        let line = if missing {
            Line::styled("T3 isn't installed on this server", fg(AMBER))
        } else {
            clip(vec![Span::styled(format!("✗ {}", h.error), fg(RED))], w.saturating_sub(tb.tool_x(0) as usize + 4))
        };
        let x = tb.tool_x(0) + 2;
        put(buf, x, y, buf.area.width.saturating_sub(x + 2), &line);
    } else {
        let present = present(h);
        for (k, name) in tb.tools.iter().enumerate() {
            if !present.contains(name) {
                continue;
            }
            let x = tb.tool_x(k);
            cells.push((Rect::new(x + 1, 1, tb.col - 1, g.card_h - 1), *name));
            let behind = new.iter().any(|(n, _)| n == name);
            let mut span = cell(app, h, name, behind);
            if app.motion.on && span.style.fg == Some(ACCENT) {
                // Light runs down the versions there's newer of.
                let k = motion::ripple(t.as_secs_f32(), i, app.hosts.len());
                span.style = fg(blend(TEXT, ACCENT, 0.7 * k));
            }
            let text = fit(&span.content, tb.col as usize - 3 + usize::from(span.content.starts_with(MARKS)));
            // A mark hangs left of the version, which centers as if alone.
            let mark = text.chars().next().filter(|c| MARKS.contains(c)).map_or(0, |c| c.len_utf8());
            let (mark, number) = text.split_at(mark);
            let cx = over(tb.col, number.width() as u16).0 - mark.width() as u16;
            put(buf, x + cx, y, tb.col, &Line::styled(text.clone(), span.style));
        }
        if tb.meter > 0 {
            for (k, key) in ["cpu", "disk"].into_iter().enumerate() {
                let x = tb.meter_x() + k as u16 * (tb.meter + 2);
                let grown = app.motion.grown(&h.name, "sys");
                put(buf, x, y, tb.meter, &Line::from(meter(&h.sys, key, tb.meter as usize, grown)));
            }
        }
    }
    if app.motion.on && h.running() {
        motion::shimmer(buf, buf.area, y, t.as_secs_f32() + i as f32 * 0.37, 1.8);
    }
    cells
}

/// The `sys` report as (key, value) pairs, e.g. ("cpu", "6%").
fn sys_parts(sys: &str) -> impl Iterator<Item = (&str, &str)> {
    sys.split(" · ").filter_map(|p| p.split_once(' '))
}

/// The `key` ('cpu' or 'disk') meter, `w` cells, `grown` of the way in: how busy is a measure (blue);
/// free disk is good news (green).
fn meter(sys: &str, key: &str, w: usize, grown: f32) -> Vec<Span<'static>> {
    let Some((_, value)) = sys_parts(sys).find(|(k, _)| *k == key) else { return vec![] };
    let color = if key == "disk" { GREEN } else { ACCENT };
    match value.trim_end_matches('%').parse::<f64>() {
        Ok(p) => bar(p / 100.0 * grown as f64, w, color),
        Err(_) => vec![Span::styled(truncate(value, w), fg(DIM))],
    }
}

/// A bar of `cells` in half cells, in `color`, then amber past 70%, red past 90%.
fn bar(frac: f64, cells: usize, color: ratatui::style::Color) -> Vec<Span<'static>> {
    let frac = frac.clamp(0.0, 1.0);
    let color = if frac > 0.9 {
        RED
    } else if frac > 0.7 {
        AMBER
    } else {
        color
    };
    let halves = (frac * (cells * 2) as f64).round() as usize;
    let mut on = "━".repeat(halves / 2);
    if halves % 2 == 1 {
        on.push('╸');
    }
    let rest = "━".repeat(cells - halves.div_ceil(2));
    vec![Span::styled(on, fg(color)), Span::styled(rest, fg(LINE))]
}

/// What hovering a card shows: every tool's full version, and the one an update would install.
fn tooltip(app: &App, buf: &mut Buffer) {
    let Some(hover) = app.hover.as_ref().filter(|h| h.shown) else { return };
    let (x, y) = hover.pos;
    let Some(&(_, Hit::Card(i))) = app.hits.iter().rev().find(|(r, _)| r.contains((x, y).into())) else {
        if y < HEADER && !app.desktop.is_empty() {
            let line = Line::styled(format!(" T3 Code desktop {} ", app.desktop), fg(TEXT));
            popup(buf, x, y + 1, vec![line]);
        }
        return;
    };
    let h = &app.hosts[i];
    let lines: Vec<Line<'static>> = versions_detail(h, &updates(h, &app.latest, &app.target))
        .into_iter()
        .map(|l| Line::styled(format!(" {l} "), fg(TEXT)))
        .collect();
    if !lines.is_empty() {
        popup(buf, x, y + 1, lines);
    }
}

fn popup(buf: &mut Buffer, x: u16, y: u16, lines: Vec<Line<'static>>) {
    let w = lines.iter().map(Line::width).max().unwrap_or(0) as u16 + 2;
    let h = lines.len() as u16 + 2;
    let x = x.min(buf.area.right().saturating_sub(w));
    let y = y.min(buf.area.bottom().saturating_sub(h));
    let area = Rect::new(x, y, w.min(buf.area.width), h.min(buf.area.height));
    fill(buf, area, SURFACE);
    Block::bordered().border_type(BorderType::Rounded).border_style(fg(LINE)).render(area, buf);
    for (k, line) in lines.iter().enumerate() {
        put(buf, x + 1, y + 1 + k as u16, w - 2, line);
    }
}

// ── output ─────────────────────────────────────────────────────────────────

fn output_line(line: &str) -> Vec<Span<'static>> {
    let base = match line.chars().next() {
        _ if line.starts_with("──") || line.starts_with('–') => DIM,
        Some('✓') => GREEN,
        Some('✗') => RED,
        Some('!' | '↩') => AMBER,
        _ => MUTED,
    };
    if base == MUTED
        && let Some((tag, rest)) = line.split_once(": ")
        && (tag == "Health" || COMPONENTS.iter().any(|(n, _)| *n == tag))
    {
        return vec![Span::styled(format!("{tag}:"), fg(ACCENT)), Span::styled(format!(" {rest}"), fg(MUTED))];
    }
    vec![Span::styled(line.to_string(), fg(base))]
}

fn output(app: &mut App, buf: &mut Buffer) {
    let g = geometry(app);
    let area = g.out;
    if area.height < 3 {
        return;
    }
    app.hits.push((area, Hit::Output));
    let host = app.hosts.get(app.selected);
    let title = match host {
        Some(h) => format!(
            " {} output · {} ",
            h.name,
            h.log.as_ref().and_then(|l| l.file_name()).map_or("no runs yet".into(), |n| n.to_string_lossy())
        ),
        None => " output ".into(),
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(fg(LINE))
        .title(Line::styled(title, fg(MUTED)))
        .padding(ratatui::widgets::Padding::horizontal(1));
    let inner = block.inner(Rect::new(2, area.y, area.width.saturating_sub(4), area.height));
    block.render(Rect::new(2, area.y, area.width.saturating_sub(4), area.height), buf);
    let Some(h) = host else { return };
    let mut rows: Vec<Vec<Span<'static>>> = vec![];
    for line in &h.lines {
        let wrapped = wrap(line, inner.width as usize);
        let styled = output_line(line);
        for (k, text) in wrapped.into_iter().enumerate() {
            if k == 0 && styled.len() == 2 && text.starts_with(&styled[0].content.to_string()) {
                let tag = styled[0].content.to_string();
                rows.push(vec![styled[0].clone(), Span::styled(text[tag.len()..].to_string(), styled[1].style)]);
            } else {
                rows.push(vec![Span::styled(text, styled.last().map_or(Style::new(), |s| s.style))]);
            }
        }
    }
    if rows.is_empty() {
        rows.push(vec![Span::styled("Nothing yet. Check or update this server.", fg(DIM))]);
    }
    let visible = inner.height as usize;
    let max = rows.len().saturating_sub(visible);
    app.out_scroll = app.out_scroll.min(max);
    let start = max - app.out_scroll;
    for (k, row) in rows.into_iter().skip(start).take(visible).enumerate() {
        put(buf, inner.x, inner.y + k as u16, inner.width, &Line::from(row));
    }
    if max > 0 {
        let mut state = ScrollbarState::new(max).position(start);
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(None)
            .thumb_symbol("┃")
            .thumb_style(fg(FAINT))
            .render(Rect::new(inner.x, inner.y, inner.width + 1, inner.height), buf, &mut state);
    }
}

// ── modals ─────────────────────────────────────────────────────────────────

/// A centered SURFACE panel; returns (outer, inner) where inner is inside border and padding.
fn panel(app: &mut App, buf: &mut Buffer, w: u16, h: u16) -> (Rect, Rect) {
    let area = buf.area;
    let w = w.min(area.width.saturating_sub(2)).max(1);
    let h = h.min(area.height.saturating_sub(2)).max(1);
    let outer = Rect::new((area.width - w) / 2, (area.height - h) / 2, w, h);
    fill(buf, outer, SURFACE);
    Block::bordered().border_type(BorderType::Rounded).border_style(fg(border())).render(outer, buf);
    app.modal_rect = outer;
    app.hits.clear();
    let inner = Rect::new(outer.x + 3, outer.y + 2, outer.width.saturating_sub(6), outer.height.saturating_sub(4));
    (outer, inner)
}

fn hint(buf: &mut Buffer, inner: Rect, text: &str) {
    put(buf, inner.x, inner.bottom().saturating_sub(1), inner.width, &Line::styled(text.to_string(), fg(DIM)));
}

fn modal(app: &mut App, buf: &mut Buffer, logos: &Logos) {
    let Some(modal) = app.modal.clone() else { return };
    match modal {
        Modal::Menu(m) => menu(app, buf, &m, logos),
        Modal::Version(input) => version(app, buf, &input),
        Modal::Servers(s) => servers(app, buf, &s),
        Modal::Confirm(c) if c.remove => confirm_remove(app, buf, &c, logos),
        Modal::Confirm(c) => confirm(app, buf, &c),
        Modal::Changes(c) => changes(app, buf, c),
        Modal::Help { scroll } => help(app, buf, scroll),
        Modal::Palette(p) => palette(app, buf, &p),
        Modal::Flow(f) => flow_window(app, buf, &f, logos),
        Modal::Installer(i) => installer(app, buf, &i),
    }
}

/// The Installer: each component with a box to tick, what's there already greyed out.
fn installer(app: &mut App, buf: &mut Buffer, inst: &Installer) {
    let h = 4 + 2 + inst.rows.len() as u16 + 2 + 1 + 2;
    let (_, inner) = panel(app, buf, 64, h);
    let target = if inst.names.len() == 1 { inst.names[0].clone() } else { format!("{} servers", inst.names.len()) };
    let title =
        Line::from(vec![Span::styled("Installer", bold(TEXT)), Span::styled(format!("   {target}"), fg(MUTED))]);
    put(buf, inner.x, inner.y, inner.width, &title);
    let label_w = inst.rows.iter().map(|r| r.label.width()).max().unwrap_or(0) + 3;
    for (i, r) in inst.rows.iter().enumerate() {
        let y = inner.y + 2 + i as u16;
        let area = Rect::new(inner.x.saturating_sub(1), y, inner.width + 2, 1);
        if i == inst.cursor {
            buf.set_style(area, Style::new().bg(cursor_bg()));
        }
        let pad = " ".repeat(label_w - r.label.width());
        let line = if r.open() {
            let mark = if r.checked { Span::styled("● ", bold(ACCENT)) } else { Span::styled("○ ", fg(MUTED)) };
            let status = if r.missing == r.total {
                "not installed".to_string()
            } else {
                format!("missing on {} of {}", r.missing, r.total)
            };
            Line::from(vec![
                mark,
                Span::styled(format!("{}{pad}", r.label), bold(TEXT)),
                Span::styled(status, fg(AMBER)),
            ])
        } else {
            let version = r.version.as_deref().map_or(String::new(), |v| format!(" · {v}"));
            Line::from(vec![
                Span::styled("✓ ", fg(blend(GREEN, SURFACE, 0.6))),
                Span::styled(format!("{}{pad}", r.label), fg(DIM)),
                Span::styled(format!("installed{version}"), fg(DIM)),
            ])
        };
        put(buf, inner.x, y, inner.width, &line);
        app.hits.push((area, Hit::Row(i)));
    }
    // The button says what it will do.
    let n = inst.picked().len();
    let y = inner.y + 2 + inst.rows.len() as u16 + 1;
    let label = if n == 0 { "Install".to_string() } else { format!("Install {n}") };
    let bw = label.width() as u16 + 4;
    let area = Rect::new(inner.x, y, bw, 1);
    let hovered = app.hover.as_ref().is_some_and(|h| h.hit == Some(Hit::Install));
    let bg =
        if n == 0 { blend(FAINT, SURFACE, 0.4) } else { blend(ACCENT, SURFACE, if hovered { 0.42 } else { 0.24 }) };
    buf.set_style(area, Style::new().bg(bg));
    put(buf, inner.x + 2, y, bw, &Line::styled(label, bold(if n == 0 { DIM } else { ACCENT })));
    app.hits.push((area, Hit::Install));
    hint(buf, inner, "space tick · a all · enter install · esc close");
}

// ── sign-in and pairing ────────────────────────────────────────────────────

/// One-row buttons, left to right from (x, y) within `w`, wrapping to the next row two down when
/// they don't fit: the key in accent, then what it does.
/// `copied` is the copy button that just worked: it says so for a moment, in place.
fn pills(
    app: &mut App,
    buf: &mut Buffer,
    (x, y, w): (u16, u16, u16),
    items: &[(&str, &str, Act)],
    copied: Option<Act>,
) {
    let (mut at, mut y) = (x, y);
    for &(key, label, act) in items {
        let width = (key.width() + label.width() + 5) as u16;
        if at > x && at + width > x + w {
            (at, y) = (x, y + 2);
        }
        if width > w {
            break;
        }
        let area = Rect::new(at, y, width, 1);
        let hovered = app.hover.as_ref().is_some_and(|h| h.hit == Some(Hit::Act(act)));
        let done = copied == Some(act);
        let tint =
            if done { blend(GREEN, SURFACE, 0.18) } else { blend(ACCENT, SURFACE, if hovered { 0.34 } else { 0.14 }) };
        buf.set_style(area, Style::new().bg(tint));
        let line = if done {
            // Same width as the label it stands in for, so nothing moves.
            let text = format!("{:^w$}", "✓ Copied", w = width as usize);
            Line::styled(text, bold(GREEN))
        } else {
            Line::from(vec![
                Span::styled(format!("  {key}"), bold(ACCENT)),
                Span::styled(format!(" {label}  "), fg(if hovered { TEXT } else { MUTED })),
            ])
        };
        put(buf, at, y, width, &line);
        app.hits.push((area, Hit::Act(act)));
        at += width + 2;
    }
}

/// A step's number and what to do, e.g. '1  Open this link and sign in'.
fn step_line(n: u8, text: &str, note: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{n}  "), bold(ACCENT)),
        Span::styled(text.to_string(), bold(TEXT)),
        Span::styled(if note.is_empty() { String::new() } else { format!("   {note}") }, fg(DIM)),
    ])
}

/// A link, cut to fit with an ellipsis (copy and open use all of it).
fn link_line(url: &str, w: usize) -> Line<'static> {
    Line::styled(truncate(url, w), Style::new().fg(ACCENT).add_modifier(Modifier::UNDERLINED))
}

/// 'AB12-CD345' as 'A B 1 2 - C D 3 4 5': easier to read off and type.
fn spaced(code: &str) -> String {
    code.chars().map(|c| c.to_string()).collect::<Vec<_>>().join(" ")
}

/// A token in groups of four, 'K7PQ 2ZM4 XW9R', so it reads off a screen.
fn grouped(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    chars.chunks(4).map(|c| c.iter().collect::<String>()).collect::<Vec<_>>().join(" ")
}

/// Time left before an ISO expiry: 'in 4:32', or None once it's past.
fn expires_in(iso: &str) -> Option<String> {
    let at = chrono::DateTime::parse_from_rfc3339(iso).ok()?;
    let left = (at.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_seconds();
    (left > 0).then(|| format!("in {}:{:02}", left / 60, left % 60))
}

/// A QR code drawn with half blocks: two modules per cell, top and bottom, dark on light.
fn draw_qr(buf: &mut Buffer, x: u16, y: u16, rows: &[Vec<bool>]) {
    let (dark, light) = (hex(0x16161a), hex(0xf4f4f6));
    for (k, pair) in rows.chunks(2).enumerate() {
        for (i, &top) in pair[0].iter().enumerate() {
            let bottom = pair.get(1).is_some_and(|r| r[i]);
            if let Some(cell) = buf.cell_mut((x + i as u16, y + k as u16)) {
                cell.set_symbol("▀").set_fg(if top { dark } else { light }).set_bg(if bottom { dark } else { light });
            }
        }
    }
}

fn flow_window(app: &mut App, buf: &mut Buffer, f: &Flow, logos: &Logos) {
    let t = app.clock();
    let qr = match (f.kind, &f.url) {
        (Kind::Pair, Some(url)) => super::flow::qr(url, 1),
        _ => None,
    };
    // A QR code beside the details when the screen has room for it, both ways.
    let qr_size = qr.as_ref().map_or(0, |q| q.len() as u16);
    let (sw, sh) = (buf.area.width, buf.area.height);
    // QR, a gap, a details column of 40; padding of three each side. Tall enough for the code and the title.
    let side = qr_size > 0 && sw >= qr_size + 51 && sh >= qr_size.div_ceil(2) + 9;
    let roomy = sh >= qr_size.div_ceil(2) + 10; // a line of hints under it, too
    let (w, h) = match f.kind {
        Kind::Pair if side => (qr_size + 49, qr_size.div_ceil(2) + if roomy { 9 } else { 8 }),
        Kind::Pair => (64, 18),
        Kind::Other => (72, 19),
        Kind::Claude => (72, 19), // room for "that code didn't work" under the input
        Kind::Codex => (72, 19),
    };
    let (_, inner) = panel(app, buf, w, h);
    let iw = inner.width as usize;

    // Title, and how it's going on the right.
    let (chip, color) = match (&f.state, f.kind) {
        (State::Failed(_), _) => ("✗ failed".to_string(), RED),
        (State::Done, Kind::Pair) => ("● ready".into(), GREEN),
        (State::Done, _) => ("✓ signed in".into(), GREEN),
        (State::Running, _) if f.url.is_none() => (format!("{} connecting", spinner(t)), AMBER),
        (State::Running, _) if f.prompt => ("● needs your code".into(), ACCENT),
        (State::Running, _) if f.sent > 0 => (format!("{} checking", spinner(t)), AMBER),
        (State::Running, _) => (format!("{} waiting for you", spinner(t)), AMBER),
    };
    // The tool's logo, what this is, and where.
    let (what, place) = match f.kind {
        Kind::Pair => ("Pair a device".to_string(), format!("with {}", f.host)),
        _ => (format!("Sign in to {}", f.tool), format!("on {}", f.host)),
    };
    let heading = Line::styled(what, bold(TEXT));
    let place = Some(Line::styled(place, fg(MUTED)));
    let rows = if COMPONENTS.iter().any(|(n, _)| *n == f.tool) {
        logo_heading(buf, logos, &f.tool, (inner.x, inner.y, inner.width), &heading, place)
    } else {
        put(buf, inner.x, inner.y, inner.width, &heading);
        put(buf, inner.x, inner.y + 1, inner.width, place.as_ref().unwrap_or(&Line::default()));
        2
    };
    let cw = chip.width() as u16;
    put(buf, inner.right().saturating_sub(cw), inner.y, cw, &Line::styled(chip, fg(color)));
    let mut y = inner.y + rows + 1;

    if let State::Failed(why) = &f.state {
        for line in wrap(&format!("✗ {why}"), iw).into_iter().take(3) {
            put(buf, inner.x, y, inner.width, &Line::styled(line, fg(RED)));
            y += 1;
        }
        y += 1;
        pills(
            app,
            buf,
            (inner.x, y, inner.width),
            &[("r", "Try again", Act::Retry), ("t", "Open in terminal", Act::Terminal)],
            f.copied_now(),
        );
        hint(buf, inner, "esc close");
        return;
    }
    if f.url.is_none() {
        let line = Line::styled(format!("Connecting to {}…", f.host), fg(MUTED));
        put(buf, inner.x, y, inner.width, &line);
        for (k, l) in f.tail(3).into_iter().enumerate() {
            put(buf, inner.x, y + 2 + k as u16, inner.width, &Line::styled(truncate(&l, iw), fg(DIM)));
        }
        hint(buf, inner, "t open in terminal instead · esc cancel");
        return;
    }
    let url = f.url.clone().unwrap_or_default();
    let ctrl = if f.prompt { "ctrl+" } else { "" };
    match f.kind {
        Kind::Pair => {
            let x = if side {
                if let Some(q) = &qr {
                    draw_qr(buf, inner.x, y, q);
                }
                inner.x + qr_size + 3
            } else {
                inner.x
            };
            let dw = inner.right().saturating_sub(x);
            let lines = [
                Line::styled("Scan it with the T3 Code app,", fg(TEXT)),
                Line::styled("or paste the link into Add environment.", fg(MUTED)),
            ];
            let mut yy = y;
            for line in lines {
                put(buf, x, yy, dw, &line);
                yy += 1;
            }
            yy += 1;
            let label = |s: &str| Line::styled(s.to_uppercase(), bold(DIM));
            put(buf, x, yy, dw, &label("Link"));
            put(buf, x, yy + 1, dw, &link_line(&url, dw as usize));
            yy += 3;
            if let Some(code) = &f.code {
                put(buf, x, yy, dw, &label("Token"));
                put(buf, x, yy + 1, dw, &Line::styled(grouped(code), bold(TEXT)));
                yy += 3;
            }
            let (left, left_color) = match f.expires.as_deref().map(expires_in) {
                Some(Some(left)) => (format!("Expires {left}"), MUTED),
                Some(None) => ("Expired: press r for a new link".to_string(), RED),
                None => (String::new(), MUTED),
            };
            let reach = if f.tailscale {
                Line::from(vec![
                    Span::styled("● ", fg(GREEN)),
                    Span::styled("Over Tailscale · ", fg(MUTED)),
                    Span::styled(left, fg(left_color)),
                ])
            } else {
                Line::from(vec![
                    Span::styled("● ", fg(AMBER)),
                    Span::styled("Local network only · ", fg(MUTED)),
                    Span::styled(left, fg(left_color)),
                ])
            };
            put(buf, x, yy, dw, &reach);
            let py = yy + 2;
            let mut acts = vec![("l", "Copy link", Act::CopyLink)];
            if f.code.is_some() {
                acts.push(("c", "Copy token", Act::CopyCode));
            }
            acts.push(("r", "New link", Act::Retry));
            pills(app, buf, (x, py, dw), &acts, f.copied_now());
            if !side && qr.is_some() {
                put(buf, x, py + 2, dw, &Line::styled("Make the window larger to see the QR code", fg(DIM)));
            }
            if roomy || !side {
                hint(buf, inner, "o open the link here · esc close");
            }
        }
        Kind::Codex | Kind::Claude => {
            put(buf, inner.x, y, inner.width, &step_line(1, "Open this link and sign in", ""));
            put(buf, inner.x + 3, y + 1, inner.width - 3, &link_line(&url, iw - 3));
            let open_key = format!("{ctrl}o");
            let link_key = format!("{ctrl}l");
            pills(
                app,
                buf,
                (inner.x + 3, y + 3, inner.width - 3),
                &[(&open_key, "Open in browser", Act::Open), (&link_key, "Copy link", Act::CopyLink)],
                f.copied_now(),
            );
            y += 5;
            if f.kind == Kind::Codex {
                put(
                    buf,
                    inner.x,
                    y,
                    inner.width,
                    &step_line(2, "Enter this code on that page", "expires in 15 minutes"),
                );
                let code = f.code.as_deref().map_or("…".to_string(), spaced);
                let cw = code.width() as u16;
                put(buf, inner.x + 3, y + 2, cw, &Line::styled(code, bold(TEXT)));
                pills(
                    app,
                    buf,
                    (inner.x + 3 + cw + 3, y + 2, inner.width.saturating_sub(cw + 6)),
                    &[("c", "Copy code", Act::CopyCode)],
                    f.copied_now(),
                );
                let wait = format!("{} Waiting for you to finish in the browser…", spinner(t));
                put(buf, inner.x + 3, y + 4, inner.width - 3, &Line::styled(wait, fg(MUTED)));
                hint(buf, inner, "t open in terminal instead · esc cancel");
            } else {
                put(buf, inner.x, y, inner.width, &step_line(2, "Paste the code the page shows you", ""));
                input_box(
                    buf,
                    Rect::new(inner.x + 3, y + 1, inner.width - 3, 3),
                    &f.input,
                    "the code from the browser",
                    f.prompt,
                );
                let note = if f.retry() {
                    Line::styled("That code didn't work. Paste it again.", fg(AMBER))
                } else if f.sent > 0 && f.state == State::Running {
                    Line::styled(format!("{} Checking the code…", spinner(t)), fg(MUTED))
                } else {
                    Line::default()
                };
                put(buf, inner.x + 3, y + 4, inner.width - 3, &note);
                hint(buf, inner, "enter send · ctrl+t open in terminal instead · esc cancel");
            }
        }
        Kind::Other => {
            for (k, l) in f.tail(6).into_iter().enumerate() {
                put(buf, inner.x, y + k as u16, inner.width, &Line::styled(truncate(&l, iw), fg(MUTED)));
            }
            y += 7;
            let open_key = format!("{ctrl}o");
            let link_key = format!("{ctrl}l");
            pills(
                app,
                buf,
                (inner.x, y, inner.width),
                &[(&open_key, "Open link", Act::Open), (&link_key, "Copy link", Act::CopyLink)],
                f.copied_now(),
            );
            if f.prompt {
                input_box(buf, Rect::new(inner.x, y + 2, inner.width, 3), &f.input, "type here, enter sends it", true);
            }
            hint(buf, inner, "enter send · ctrl+t open in terminal instead · esc cancel");
        }
    }
}

/// A logo beside a heading (two rows: the heading, then `sub`); returns the rows it took.
fn logo_heading(
    buf: &mut Buffer,
    logos: &Logos,
    name: &str,
    (x, y, w): (u16, u16, u16),
    heading: &Line<'static>,
    sub: Option<Line<'static>>,
) -> u16 {
    let size = logos.size(name);
    logos.draw(name, Rect::new(x, y, size.width, size.height), buf, false);
    let tx = x + size.width + 2;
    put(buf, tx, y, w.saturating_sub(size.width + 2), heading);
    if let Some(sub) = sub {
        put(buf, tx, y + 1, w.saturating_sub(size.width + 2), &sub);
    }
    size.height.max(1)
}

fn menu(app: &mut App, buf: &mut Buffer, m: &Menu, logos: &Logos) {
    let sections = m.items.iter().skip(1).filter(|i| matches!(i, Item::Section(_))).count();
    let extra = if m.logo.is_some() { logos::HEIGHT - 1 } else { 0 };
    let h = 4 + 1 + 1 + m.items.len() as u16 + sections as u16 + 1 + 1 + extra;
    // Columns sized to what's in them: the widest label plus three cells, then the details.
    let rows = || {
        m.items
            .iter()
            .filter_map(|i| if let Item::Row { label, detail, .. } = i { Some((label, detail)) } else { None })
    };
    let label_w = rows().map(|(l, _)| l.width()).max().unwrap_or(0) + 3;
    let detail_w = rows().map(|(_, d)| d.width()).max().unwrap_or(0);
    let (_, inner) = panel(app, buf, (2 + label_w + detail_w + 6).max(56) as u16, h);
    match m.logo {
        Some(name) => {
            logo_heading(buf, logos, name, (inner.x, inner.y, inner.width), &m.heading, None);
        }
        None => put(buf, inner.x, inner.y, inner.width, &m.heading),
    }
    // The body as lines, then the window of them around the cursor that fits.
    let mut lines: Vec<(Line<'static>, Option<usize>)> = vec![];
    for (i, item) in m.items.iter().enumerate() {
        match item {
            Item::Section(name) => {
                if i > 0 {
                    lines.push((Line::raw(""), None));
                }
                lines.push((Line::styled(name.to_uppercase(), bold(DIM)), None));
            }
            Item::Row { icon, label, detail, .. } => {
                let detail_color = if i == m.cursor { TEXT } else { MUTED };
                let line = Line::from(vec![
                    Span::styled(format!("{icon} "), fg(ACCENT)),
                    Span::styled(format!("{label}{}", " ".repeat(label_w - label.width())), bold(TEXT)),
                    Span::styled(detail.clone(), fg(detail_color)),
                ]);
                lines.push((line, Some(i)));
            }
        }
    }
    let room = inner.height.saturating_sub(4 + extra) as usize;
    let at = lines.iter().position(|(_, i)| *i == Some(m.cursor)).unwrap_or(0);
    let start = (at + 1).saturating_sub(room).min(lines.len().saturating_sub(room));
    for (k, (line, item)) in lines.iter().skip(start).take(room).enumerate() {
        let y = inner.y + 2 + extra + k as u16;
        if let Some(i) = item {
            let row = Rect::new(inner.x.saturating_sub(1), y, inner.width + 2, 1);
            if *i == m.cursor {
                buf.set_style(row, Style::new().bg(cursor_bg()));
            }
            app.hits.push((row, Hit::Row(*i)));
        }
        put(buf, inner.x, y, inner.width, line);
    }
    hint(buf, inner, "↑↓ move · enter choose · esc close");
}

/// A bordered one-line text field; the cursor is a reversed cell.
fn input_box(buf: &mut Buffer, area: Rect, input: &super::input::Input, placeholder: &str, focused: bool) {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(fg(if focused { ACCENT } else { LINE }))
        .render(area, buf);
    let w = area.width.saturating_sub(4) as usize;
    if w == 0 {
        return;
    }
    let mut spans = vec![];
    let caret = if focused { Modifier::REVERSED } else { Modifier::empty() };
    if input.value.is_empty() {
        spans.push(Span::styled(" ", Style::new().add_modifier(caret)));
        spans.push(Span::styled(truncate(placeholder, w.saturating_sub(1)), fg(DIM)));
    } else {
        // Scroll so the cursor stays in view.
        let chars: Vec<char> = input.value.chars().collect();
        let start = (input.cursor + 1).saturating_sub(w);
        let shown: Vec<char> = chars.iter().skip(start).take(w).copied().collect();
        let at = input.cursor - start;
        let text = |r: std::ops::Range<usize>| shown[r].iter().collect::<String>();
        spans.push(Span::styled(text(0..at), fg(TEXT)));
        let under = shown.get(at).map_or(" ".to_string(), |c| c.to_string());
        spans.push(Span::styled(under, Style::new().fg(TEXT).add_modifier(caret)));
        if at < shown.len() {
            spans.push(Span::styled(text(at + 1..shown.len()), fg(TEXT)));
        }
    }
    put(buf, area.x + 2, area.y + 1, area.width.saturating_sub(4), &Line::from(spans));
}

fn button(buf: &mut Buffer, area: Rect, label: &str, color: ratatui::style::Color, active: bool) {
    if active {
        buf.set_style(area, Style::new().bg(cursor_bg()));
    }
    Block::bordered().border_type(BorderType::Rounded).border_style(fg(color)).render(area, buf);
    let (x, line) = centered(Line::styled(label.to_string(), bold(color)), area.width.saturating_sub(2));
    put(buf, area.x + 1 + x, area.y + 1, area.width.saturating_sub(2), &line);
}

fn version(app: &mut App, buf: &mut Buffer, input: &super::input::Input) {
    let (_, inner) = panel(app, buf, 64, 4 + 1 + 1 + 3 + 1 + 2);
    put(buf, inner.x, inner.y, inner.width, &Line::styled("T3 version to install", bold(TEXT)));
    input_box(buf, Rect::new(inner.x, inner.y + 2, inner.width, 3), input, "blank = latest nightly", true);
    let example = Line::styled("e.g. 0.0.46-nightly.20261003.2623", fg(DIM));
    put(buf, inner.x, inner.bottom().saturating_sub(2), inner.width, &example);
    hint(buf, inner, "enter save · esc cancel");
}

fn servers(app: &mut App, buf: &mut Buffer, s: &Servers) {
    let list = s.hosts.len().clamp(1, 12) as u16;
    let suggestions = s.suggestions();
    // Chips flow onto at most two lines.
    let width = 64u16.min(buf.area.width.saturating_sub(2));
    let inner_w = width.saturating_sub(6) as usize;
    let mut chip_rows: Vec<Vec<(usize, String)>> = vec![vec![]];
    let mut used = 0;
    for (i, host) in suggestions.iter().enumerate() {
        let chip = format!("+ {host}");
        let cw = chip.width() + 2;
        if used + cw > inner_w {
            if chip_rows.len() == 2 {
                break;
            }
            chip_rows.push(vec![]);
            used = 0;
        }
        used += cw;
        chip_rows.last_mut().unwrap().push((i, chip));
    }
    let chips = if suggestions.is_empty() { 0 } else { 1 + chip_rows.len() as u16 };
    let h = 4 + 2 + list + 1 + 3 + chips + 1 + 1;
    let (_, inner) = panel(app, buf, 64, h);
    let heading =
        Line::from(vec![Span::styled("Servers", bold(TEXT)), Span::styled(format!("   {}", s.hosts.len()), fg(DIM))]);
    put(buf, inner.x, inner.y, inner.width, &heading);
    let mut y = inner.y + 2;
    if s.hosts.is_empty() {
        put(buf, inner.x, y, inner.width, &Line::styled("No servers yet. Add one below.", fg(DIM)));
    }
    let first = s.row.map_or(0, |r| (r + 1).saturating_sub(list as usize));
    for (k, host) in s.hosts.iter().enumerate().skip(first).take(list as usize) {
        let row = Rect::new(inner.x.saturating_sub(1), y, inner.width + 2, 1);
        let on = s.row == Some(k);
        if on {
            buf.set_style(row, Style::new().bg(cursor_bg()));
        }
        put(buf, inner.x, y, inner.width, &Line::styled(host.clone(), fg(TEXT)));
        let remove = Line::styled("✕ Remove", fg(if on { RED } else { DIM }));
        let rw = remove.width() as u16;
        let rx = inner.right().saturating_sub(rw).max(inner.x);
        put(buf, rx, y, rw, &remove);
        app.hits.push((row, Hit::Row(k)));
        app.hits.push((Rect::new(rx.saturating_sub(1), y, rw + 2, 1), Hit::Remove(k)));
        y += 1;
    }
    y = inner.y + 2 + list + 1;
    let add = Rect::new(inner.right().saturating_sub(10), y, 10, 3);
    input_box(
        buf,
        Rect::new(inner.x, y, inner.width.saturating_sub(11), 3),
        &s.input,
        "SSH alias or user@host",
        s.row.is_none(),
    );
    button(buf, add, "+ Add", ACCENT, false);
    app.hits.push((add, Hit::Add));
    y += 3;
    if !suggestions.is_empty() {
        put(buf, inner.x, y, inner.width, &Line::styled("From ~/.ssh/config", fg(DIM)));
        for (r, row) in chip_rows.iter().enumerate() {
            let mut x = inner.x;
            for (i, chip) in row {
                let cw = chip.width() as u16;
                let hot = s.input.value == **suggestions[*i];
                let style = if hot {
                    Style::new().fg(ACCENT).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                } else {
                    fg(ACCENT)
                };
                put(buf, x, y + 1 + r as u16, cw, &Line::styled(chip.clone(), style));
                app.hits.push((Rect::new(x, y + 1 + r as u16, cw, 1), Hit::Suggest(*i)));
                x += cw + 2;
            }
        }
    }
    hint(buf, inner, "esc or click outside when done · tab picks a suggestion");
}

fn confirm(app: &mut App, buf: &mut Buffer, c: &Confirm) {
    let h = 4 + 2 + c.busy.len() as u16 + 1 + 3;
    let (_, inner) = panel(app, buf, 60, h);
    let title = "Updating T3 restarts it and ends live agent sessions";
    put(buf, inner.x, inner.y, inner.width, &clip(vec![Span::styled(title, bold(AMBER))], inner.width as usize));
    let width = c.busy.iter().map(|(n, _)| n.width()).max().unwrap_or(0);
    for (k, (host, n)) in c.busy.iter().enumerate() {
        let line = Line::from(vec![
            Span::styled(format!("{host:<width$}  "), fg(TEXT)),
            Span::styled(format!("● {n} agent{} live", if *n == 1 { "" } else { "s" }), fg(AMBER)),
        ]);
        put(buf, inner.x, inner.y + 2 + k as u16, inner.width, &line);
    }
    let y = inner.bottom().saturating_sub(3);
    let (a, b) = (Rect::new(inner.x, y, 18, 3), Rect::new(inner.x + 20, y, 12, 3));
    button(buf, a, "Update anyway", if c.cursor == 0 { AMBER } else { DIM }, c.cursor == 0);
    button(buf, b, "Cancel", if c.cursor == 1 { ACCENT } else { DIM }, c.cursor == 1);
    app.hits.push((a, Hit::Row(0)));
    app.hits.push((b, Hit::Row(1)));
}

/// Removing a provider: what goes, what stays, and Cancel first.
fn confirm_remove(app: &mut App, buf: &mut Buffer, c: &Confirm, logos: &Logos) {
    let (name, label) = COMPONENTS.iter().find(|(n, _)| n.to_lowercase() == c.only).copied().unwrap_or(("", ""));
    let host = c.names.first().cloned().unwrap_or_default();
    let h = 4 + 2 + 1 + 2 + c.busy.len() as u16 * 2 + 1 + 3;
    let (_, inner) = panel(app, buf, 62, h);
    let heading = Line::styled(format!("Remove {label}?"), bold(TEXT));
    let place = Some(Line::styled(format!("from {host}"), fg(MUTED)));
    logo_heading(buf, logos, name, (inner.x, inner.y, inner.width), &heading, place);
    let mut y = inner.y + 3;
    let keep = format!("Its settings and sign-in stay, so the Installer can put {label} back as it was.");
    for line in wrap(&keep, inner.width as usize) {
        put(buf, inner.x, y, inner.width, &Line::styled(line, fg(MUTED)));
        y += 1;
    }
    for (_, n) in &c.busy {
        y += 1;
        let live = format!("● {n} agent{} live on {host}: sessions using {label} stop", if *n == 1 { "" } else { "s" });
        put(buf, inner.x, y, inner.width, &clip(vec![Span::styled(live, fg(AMBER))], inner.width as usize));
    }
    let y = inner.bottom().saturating_sub(3);
    let (a, b) = (Rect::new(inner.x, y, 12, 3), Rect::new(inner.x + 14, y, 12, 3));
    button(buf, a, "Remove", if c.cursor == 0 { RED } else { DIM }, c.cursor == 0);
    button(buf, b, "Cancel", if c.cursor == 1 { ACCENT } else { DIM }, c.cursor == 1);
    app.hits.push((a, Hit::Row(0)));
    app.hits.push((b, Hit::Row(1)));
}

const HELP: [(&str, &str); 21] = [
    ("enter / click", "Actions for the selected server"),
    ("← ↑ ↓ → j k", "Move between servers"),
    ("u", "Update menu for the selected server"),
    ("a", "Update all servers (first one alone first)"),
    ("r", "Check every server again"),
    ("c", "What's new: release notes of waiting updates"),
    ("v", "Pin the T3 version updates install"),
    ("e", "Add or remove servers"),
    ("s", "SSH into the selected server"),
    ("l", "Show or hide the output panel"),
    ("pgup / pgdn", "Scroll the output panel or the grid"),
    ("d", "Update the T3 Code desktop app (macOS)"),
    ("i", "Installer: pick what to install on the selected server"),
    ("U", "Update t3up itself, when a new version is out"),
    ("ctrl+p", "Command palette"),
    ("?", "This help"),
    ("q", "Quit (press twice while servers are running)"),
    ("ctrl+c", "Quit now and stop every job"),
    ("esc", "Close a window"),
    ("tab", "Next server (in the editor: next suggestion)"),
    ("mouse", "Click a card for its menu, a tool for its own; wheel scrolls"),
];

fn help(app: &mut App, buf: &mut Buffer, scroll: usize) {
    let keys = HELP.iter().map(|(k, _)| k.width()).max().unwrap_or(0) + 3;
    let wide = keys + HELP.iter().map(|(_, w)| w.width()).max().unwrap_or(0) + 6;
    let (_, inner) = panel(app, buf, wide as u16, 4 + 2 + HELP.len() as u16 + 2);
    put(buf, inner.x, inner.y, inner.width, &Line::styled("Keys", bold(TEXT)));
    let rows = inner.height.saturating_sub(4) as usize;
    let start = scroll.min(HELP.len().saturating_sub(rows));
    for (k, (key, what)) in HELP.iter().enumerate().skip(start).take(rows) {
        let pad = " ".repeat(keys.saturating_sub(key.width()));
        let line = Line::from(vec![Span::styled(format!("{key}{pad}"), fg(ACCENT)), Span::styled(*what, fg(MUTED))]);
        put(buf, inner.x, inner.y + 2 + (k - start) as u16, inner.width, &line);
    }
    hint(buf, inner, "esc close");
}

fn palette(app: &mut App, buf: &mut Buffer, p: &Palette) {
    let matches = p.matches();
    let rows = matches.len().clamp(1, 9) as u16;
    let (_, inner) = panel(app, buf, 64, 4 + 3 + 1 + rows + 1 + 1);
    input_box(buf, Rect::new(inner.x, inner.y, inner.width, 3), &p.input, "type a command", true);
    if matches.is_empty() {
        put(buf, inner.x, inner.y + 4, inner.width, &Line::styled("No command matches.", fg(DIM)));
    }
    let first = (p.cursor + 1).saturating_sub(rows as usize);
    for (k, &i) in matches.iter().enumerate().skip(first).take(rows as usize) {
        let y = inner.y + 4 + (k - first) as u16;
        let row = Rect::new(inner.x.saturating_sub(1), y, inner.width + 2, 1);
        let on = k == p.cursor;
        if on {
            buf.set_style(row, Style::new().bg(cursor_bg()));
        }
        let line = Line::from(vec![
            Span::styled("› ", fg(ACCENT)),
            Span::styled(p.items[i].0.clone(), if on { bold(TEXT) } else { fg(MUTED) }),
        ]);
        put(buf, inner.x, y, inner.width, &line);
        app.hits.push((row, Hit::Row(k)));
    }
    hint(buf, inner, "↑↓ move · enter run · esc close");
}

// ── what's new ─────────────────────────────────────────────────────────────

static COMMIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([a-zA-Z]+)(\([^)]*\))?(!)?:\s*(.*)$").unwrap());
static BUILD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-nightly\.\d{8}\.(\d+)$").unwrap());

/// '0.0.46-nightly.20261003.2623' -> '0.0.46 #2623'.
fn version_label(v: &str) -> String {
    match BUILD.captures(v) {
        Some(c) => format!("{} #{}", short(v).replace("-nightly", ""), &c[1]),
        None => v.to_string(),
    }
}

fn change_header(name: &str, from: &str, to: &str, releases: Option<usize>) -> Line<'static> {
    let (a, b) = (version_label(from), version_label(to));
    let b =
        if short(from) == short(to) && newer(to, from) { b.rsplit(' ').next().unwrap_or(&b).to_string() } else { b };
    let mut spans = vec![Span::styled(name.to_string(), bold(TEXT)), Span::styled(format!("  {a} → {b}"), fg(MUTED))];
    if let Some(n) = releases {
        spans.push(Span::styled(format!(" · {n} release{}", if n == 1 { "" } else { "s" }), fg(DIM)));
    }
    Line::from(spans)
}

fn release_lines(r: &Release, width: usize, out: &mut Vec<Line<'static>>) {
    out.push(Line::styled(format!("  {}  {}", r.tag, r.date), fg(DIM)));
    for note in &r.notes {
        let (kind, scope, text) = match COMMIT.captures(note) {
            Some(c) => (
                Some(c[1].to_string()),
                c.get(2).map_or(String::new(), |m| m.as_str().to_string()) + c.get(3).map_or("", |m| m.as_str()),
                c[4].to_string(),
            ),
            None => (None, String::new(), note.clone()),
        };
        let head = kind.as_ref().map_or(0, |k| k.width() + scope.width() + 2);
        let rows = wrap(&format!("{}{}", " ".repeat(head), text), width.saturating_sub(4));
        for (k, row) in rows.iter().enumerate() {
            let mut spans = vec![Span::styled(if k == 0 { "  • " } else { "    " }, fg(DIM))];
            if k == 0
                && let Some(kind) = &kind
            {
                let color = match kind.as_str() {
                    "feat" => ACCENT,
                    "fix" => GREEN,
                    _ => MUTED,
                };
                spans.push(Span::styled(kind.clone(), bold(color)));
                spans.push(Span::styled(scope.clone(), fg(DIM)));
                spans.push(Span::styled(": ", fg(DIM)));
                spans.push(Span::styled(row.trim_start().to_string(), fg(TEXT)));
                out.push(Line::from(spans));
                continue;
            }
            spans.push(Span::styled(row.trim_start_matches(' ').to_string(), fg(TEXT)));
            out.push(Line::from(spans));
        }
    }
}

fn changes(app: &mut App, buf: &mut Buffer, mut c: Changes) {
    if c.items.is_empty() {
        let (_, inner) = panel(app, buf, 56, 4 + 1 + 1 + 1 + 1);
        put(
            buf,
            inner.x,
            inner.y,
            inner.width,
            &Line::styled(format!("Everything on {} is up to date.", c.host), fg(TEXT)),
        );
        hint(buf, inner, "esc close");
        return;
    }
    let (w, h) = (buf.area.width, buf.area.height);
    let (_, inner) = panel(app, buf, (w * 8 / 10).max(40), (h * 8 / 10).max(10));
    put(
        buf,
        inner.x,
        inner.y,
        inner.width,
        &Line::from(vec![Span::styled("What's new on ", bold(TEXT)), Span::styled(c.host.clone(), bold(ACCENT))]),
    );
    let width = inner.width as usize;
    let mut lines: Vec<Line<'static>> = vec![];
    let t = app.clock();
    for key in &c.items {
        let name = key.0.clone();
        let result = app.changelogs.get(key);
        let count = result.and_then(|r| r.as_ref().ok()).map(Vec::len);
        lines.push(change_header(&name, &key.1, &key.2, count));
        match result {
            None => lines.push(Line::styled(format!("  {} Loading release notes…", spinner(t)), fg(AMBER))),
            Some(Err(e)) => {
                for row in wrap(&format!("Couldn't load release notes: {e}"), width.saturating_sub(2)) {
                    lines.push(Line::styled(format!("  {row}"), fg(AMBER)));
                }
            }
            Some(Ok(releases)) if releases.is_empty() => lines.push(Line::styled("  No release notes found.", fg(DIM))),
            Some(Ok(releases)) => {
                for (n, r) in releases.iter().enumerate() {
                    if n > 0 {
                        lines.push(Line::raw(""));
                    }
                    release_lines(r, width, &mut lines);
                }
            }
        }
        lines.push(Line::raw(""));
    }
    let body = Rect::new(inner.x, inner.y + 2, inner.width, inner.height.saturating_sub(4));
    let max = lines.len().saturating_sub(body.height as usize);
    c.scroll = c.scroll.min(max);
    for (k, line) in lines.iter().skip(c.scroll).take(body.height as usize).enumerate() {
        put(buf, body.x, body.y + k as u16, body.width, line);
    }
    if max > 0 {
        let mut state = ScrollbarState::new(max).position(c.scroll);
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(None)
            .thumb_symbol("┃")
            .thumb_style(fg(DIM))
            .render(Rect::new(body.x, body.y, body.width + 2, body.height), buf, &mut state);
    }
    hint(buf, inner, "↑↓ scroll · pgup/pgdn page · esc close");
    app.modal = Some(Modal::Changes(c));
}

// ── toasts ─────────────────────────────────────────────────────────────────

/// The toast stack, bottom-right. With a window open (`since` it opened), `over` draws the toasts
/// that came after it (on top), else the ones from before (under it); every toast keeps its place.
/// Toasts, newest at the bottom. Several stack: the newest in front, the edges of two older ones
/// peeking out above it. Hovering the stack spreads them out.
fn toasts(app: &mut App, buf: &mut Buffer, since: Option<std::time::Instant>, over: bool) {
    let w = 46u16.min(buf.area.width.saturating_sub(4));
    if w < 12 {
        return;
    }
    let shown: Vec<Toast> =
        app.toasts.iter().rev().filter(|t| since.is_none_or(|s| (t.born >= s) == over)).cloned().collect();
    let open = shown.len() == 1 || app.hover.as_ref().is_some_and(|h| h.hit == Some(Hit::Toasts));
    let floor = buf.area.height.saturating_sub(2); // one row of air above the footer
    let (mut bottom, mut top, mut front) = (floor, floor, None);
    let now = std::time::Instant::now();
    for (k, Toast { title, text, sev, born, until }) in shown.iter().enumerate() {
        let color = match sev {
            Sev::Info => ACCENT,
            Sev::Warning => AMBER,
            Sev::Error => RED,
        };
        // Slides in from the right edge, and back out just before it expires.
        let (age, left) = (now.duration_since(*born).as_secs_f32(), until.saturating_duration_since(now).as_secs_f32());
        let out = if app.motion.on {
            let k = motion::ease_out(age / motion::SLIDE).min(motion::ease_out(left / motion::SLIDE));
            ((1.0 - k) * (w + 2) as f32) as u16
        } else {
            0
        };
        // Behind the front one: its top edge, narrower and fainter the further back.
        if let Some((x, y)) = front.filter(|_| !open) {
            let k = k as u16;
            if k > 2 || y < HEADER + 1 + k {
                break;
            }
            let lw = w - 4 * k;
            let edge = format!("╭{}╮", "─".repeat(lw as usize - 2));
            let style = Style::new().fg(blend(color, SURFACE, 0.7 / k as f32)).bg(SURFACE);
            put(buf, x + 2 * k + out, y - k, lw, &Line::styled(edge, style));
            top = y - k;
            continue;
        }
        let rows = wrap(text, w as usize - 4);
        let h = 3 + rows.len() as u16;
        if bottom < HEADER + 1 + h {
            break;
        }
        let y = bottom - h;
        let whole = Rect::new(0, 0, w, h);
        let mut card = Buffer::empty(whole);
        fill(&mut card, whole, SURFACE);
        Block::bordered().border_type(BorderType::Rounded).border_style(fg(color)).render(whole, &mut card);
        put(&mut card, 2, 1, w - 4, &Line::styled(title.clone(), bold(color)));
        for (k, row) in rows.iter().enumerate() {
            put(&mut card, 2, 2 + k as u16, w - 4, &Line::styled(row.clone(), fg(TEXT)));
        }
        let mut x = buf.area.width - w - MARGIN;
        let modal = app.modal_rect;
        if !modal.is_empty()
            && Rect::new(x, y, w, h).intersects(modal)
            && modal.right() + 2 + w + MARGIN <= buf.area.width
        {
            x = modal.right() + 2;
        }
        blit(&card, buf, x + out, y as i32, buf.area);
        front = front.or(Some((x, y)));
        (bottom, top) = (y, y);
    }
    // Hovering anywhere on the stack (spread out or not) keeps it open.
    if let Some((x, _)) = front {
        app.hits.push((Rect::new(x, top, w, floor - top), Hit::Toasts));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Event;
    use crate::tui::app::Sev;
    use crate::tui::snap::{demo, frame, has, text};

    /// The color of the first cell of `needle` on screen.
    fn cell_fg(buf: &Buffer, needle: &str) -> Option<ratatui::style::Color> {
        let rows = text(buf);
        let (y, row) = rows.iter().enumerate().find(|(_, r)| r.contains(needle))?;
        let x = row[..row.find(needle)?].chars().count();
        Some(buf[(x as u16, y as u16)].fg)
    }

    #[test]
    fn versions_line_up_under_the_newest_one() {
        let mut app = demo();
        let buf = frame(&mut app, 120, 30);
        let rows = text(&buf);
        let col = |row: &str, needle: &str| row.find(needle).map(|b| row[..b].chars().count());
        // The newest T3 heads its column; each server's own sits right under it, in the accent when behind.
        let head = rows.iter().find_map(|r| col(r, "0.0.46 #2648")).expect("head");
        let one_s = rows.iter().find(|r| r.contains("one-s")).unwrap();
        assert_eq!(col(one_s, "0.0.46 #2623"), Some(head), "{}", rows.join("\n"));
        assert_eq!(cell_fg(&buf, "0.0.46 #2623").unwrap(), ACCENT);
        assert_eq!(cell_fg(&buf, "1.18.34").unwrap(), TEXT, "up to date");
    }

    #[test]
    fn dashboard_at_100x30() {
        let mut app = demo();
        let buf = frame(&mut app, 100, 30);
        for needle in [
            "▄█▄ ▀██ █ █ █▀█",        // the name, big
            "✗ 2 failed",             // what's wrong, beside the button
            "↑ Update 2 servers ▏ ⋯", // which counts what it brings forward
            "Ganz-Harbour",           // this machine, first
            "3d 4h",
            "one-s",
            "one-m",
            "mdr",
            "box",
            "#2648", // the newest, heading its column; too wide for it whole, the build stays
            "↑#2623   ┆  ↑0.160.0  ┆   sign in  ┆   1.18.34  ┆",
            "latest",
            "cpu     disk",
            "1d 5h",
            "12d 2h",
            "● 2 agents live",
            "✓#2648",
            "✓2.1.289",
            "✗ failed",
            "Codex: EACCES",
            "enter → retry or open a terminal",
            "enter actions",
            "q quit",
        ] {
            assert!(has(&buf, needle), "{needle:?} missing from\n{}", text(&buf).join("\n"));
        }
        // The selected row is lit under its rule; the others aren't.
        let card = |i| app.hits.iter().find(|(_, h)| *h == Hit::Card(i)).unwrap().0;
        assert_eq!(buf[(card(1).x, card(1).y + 1)].bg, SURFACE);
        assert_eq!(buf[(card(1).x, card(1).y)].bg, BG);
        assert_eq!(buf[(card(0).x, card(0).y + 1)].bg, BG);
        assert_eq!(cell_fg(&buf, "✗ failed").unwrap(), RED);
        // A run shows as the spinner before its name, not in words on its row.
        assert!(!has(&buf, "updating"));
    }

    #[test]
    fn sign_in_and_pairing_windows_show_everything_at_91x27() {
        use crate::tui::app::Res;
        let cases: [(&str, &str, &[&str]); 3] = [
            (
                "Codex",
                "1. Open\r\n   https://auth.openai.com/codex/device\r\n2. Enter this one-time code\r\n   AB12-CD345\r\n",
                &[
                    "https://auth.openai.com/codex/device",
                    "A B 1 2 - C D 3 4 5",
                    "Copy code",
                    "Waiting for you",
                    "Open in browser",
                ],
            ),
            (
                "Claude",
                "visit: https://claude.com/cai/oauth/authorize?code=true\r\nPaste code here if prompted > ",
                &["Paste the code the page shows you", "the code from the browser", "needs your code", "Copy link"],
            ),
            (
                "T3",
                "@@pair tailscale\r\nPairing URL: https://one-s.tail79489d.ts.net/pair#token=K7PQ2ZM4XW9R\r\nToken: K7PQ2ZM4XW9R\r\nExpires: 2099-01-01T00:00:00.000Z\r\n",
                &["K7PQ 2ZM4 XW9R", "Over Tailscale", "Copy token", "New link", "▀"],
            ),
        ];
        for (tool, output, needles) in cases {
            let mut app = demo();
            app.modal = None;
            app.start_flow("one-s", tool);
            let id = match &app.modal {
                Some(Modal::Flow(f)) => f.id,
                _ => panic!("no window for {tool}"),
            };
            app.on_result(Res::FlowOut { id, text: output.into() });
            if tool == "T3" {
                app.on_result(Res::FlowExit { id, code: Some(0) });
            }
            let buf = frame(&mut app, 91, 27);
            for needle in needles {
                assert!(has(&buf, needle), "{tool}: {needle:?} missing from\n{}", text(&buf).join("\n"));
            }
        }
    }

    #[test]
    fn cards_stack_in_one_column_of_rows() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = demo();
        frame(&mut app, 100, 30);
        let g = geometry(&app);
        assert_eq!(g.cols(), 1);
        assert!(g.cards.iter().all(|c| c.x == g.cards[0].x && c.width == g.cards[0].width));
        assert!(g.cards.windows(2).all(|w| w[1].y == w[0].y + g.card_h));
        // Up and down go through them in order, clamped at the ends.
        let key = |app: &mut App, code| app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
        app.selected = 1;
        key(&mut app, KeyCode::Down);
        assert_eq!(app.selected, 2);
        key(&mut app, KeyCode::Up);
        key(&mut app, KeyCode::Up);
        key(&mut app, KeyCode::Up);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn a_mark_hangs_left_of_a_centered_version() {
        let mut app = demo();
        app.on_job("one-m", Event::Done("Codex: codex-cli 0.160.0 -> codex-cli 1.0.47".into()));
        let buf = frame(&mut app, 100, 30);
        let one_m = app.hosts.iter().position(|h| h.name == "one-m").unwrap();
        let row = |tool| {
            let cell = app.hits.iter().find(|(_, h)| *h == Hit::Tool(one_m, tool)).unwrap().0;
            (cell.x..cell.right()).map(|x| buf[(x, cell.y + 1)].symbol()).collect::<String>()
        };
        // Each number centered in its column as if alone, its mark right before it, no space.
        assert_eq!(row("Claude"), "  ✓2.1.289  ");
        assert_eq!(row("Codex"), "  ✓1.0.47   ", "where a lone 1.0.47 would be");
    }

    #[test]
    fn toasts_stack_and_spread_out_on_hover() {
        use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
        let mut app = demo();
        for host in ["depressed-louis", "mdr", "one-s"] {
            app.toast(Sev::Info, "Done", format!("{host} updated in 2s"));
        }
        let buf = frame(&mut app, 100, 30);
        // The newest in front; only the edges of the two before it.
        assert!(has(&buf, "one-s updated") && !has(&buf, "mdr updated") && !has(&buf, "louis updated"));
        let rows = text(&buf);
        let y = rows.iter().position(|r| r.contains("one-s updated")).unwrap() - 2; // the front card's top
        assert!(rows[y - 1].contains("╭──") && rows[y - 2].contains("╭──"), "{}", rows.join("\n"));
        let edge = |r: &str| r[r.rfind('╭').unwrap()..].chars().take_while(|&c| c != '╮').count();
        assert_eq!((edge(&rows[y - 1]), edge(&rows[y - 2])), (41, 37), "narrower further back");
        // Hovering spreads them out, and they wait while you read.
        let stack = app.hits.iter().find(|(_, h)| *h == Hit::Toasts).unwrap().0;
        let at = |x, y| MouseEvent { kind: MouseEventKind::Moved, column: x, row: y, modifiers: KeyModifiers::NONE };
        app.on_mouse(at(stack.x + 2, stack.y));
        assert!(app.dirty);
        let buf = frame(&mut app, 100, 30);
        assert!(has(&buf, "one-s updated") && has(&buf, "mdr updated") && has(&buf, "louis updated"));
        let until = app.toasts[0].until;
        std::thread::sleep(std::time::Duration::from_millis(20));
        app.tick();
        assert!(app.toasts[0].until > until, "paused under the pointer");
        // Away again: stacked.
        app.on_mouse(at(1, 5));
        let buf = frame(&mut app, 100, 30);
        assert!(!has(&buf, "mdr updated"));
    }

    #[test]
    fn copying_says_so_on_the_button_not_in_a_toast() {
        use crate::tui::app::Res;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = demo();
        app.modal = None;
        app.toasts.clear();
        app.start_flow("one-s", "Codex");
        let Some(Modal::Flow(f)) = &app.modal else { panic!() };
        let id = f.id;
        app.on_result(Res::FlowOut {
            id,
            text: "https://auth.openai.com/codex/device\r\none-time code\r\n AB12-CD345\r\n".into(),
        });
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        let buf = frame(&mut app, 91, 27);
        assert!(has(&buf, "✓ Copied") && !has(&buf, "Copy code"), "{}", text(&buf).join("\n"));
        assert!(app.toasts.is_empty());
        // News while the window is open shows on top of it.
        app.toast(Sev::Info, "Signed in", "Claude on one-m");
        let buf = frame(&mut app, 91, 27);
        assert!(has(&buf, "Claude on one-m"), "{}", text(&buf).join("\n"));
    }

    #[test]
    fn a_server_without_t3_offers_the_install() {
        let mut app = demo();
        let h = &mut app.hosts[2];
        h.reset(crate::model::Mode::Check, "all");
        h.apply(Event::Skip("T3: not installed".into()));
        h.apply(Event::Complete);
        h.apply(Event::Exit { code: Some(0), error: None });
        let buf = frame(&mut app, 150, 40);
        for needle in ["○ no T3", "T3 isn't installed on this server", "enter → install T3"] {
            assert!(has(&buf, needle), "{needle:?} missing from\n{}", text(&buf).join("\n"));
        }
    }

    #[test]
    fn update_all_button_is_padded_and_inset() {
        let mut app = demo();
        for w in [60u16, 91, 150] {
            let buf = frame(&mut app, w, 24);
            let (y, label) = (HEADER - 1, "↑ Update 2 servers");
            let row = &text(&buf)[y as usize];
            let x = row.find(label).map(|b| row[..b].chars().count()).expect("button");
            let end = x + label.chars().count();
            let bg = buf[(x as u16, y)].bg;
            // A cell of the pill on each side of the label, the divider, '⋯' padded, its rounded ends,
            // then the screen's margin.
            for dx in [x - 1, end] {
                assert_eq!(buf[(dx as u16, y)].bg, bg, "pill padding at {dx} (width {w})");
            }
            assert_eq!(&row[row.char_indices().nth(end + 1).unwrap().0..][..7], "▏ ⋯", "(width {w})");
            for (dx, cap) in [(x - 2, "\u{e0b6}"), (end + 5, "\u{e0b4}")] {
                assert_eq!(
                    (buf[(dx as u16, y)].symbol(), buf[(dx as u16, y)].fg),
                    (cap, bg),
                    "rounded end (width {w})"
                );
            }
            assert_ne!(buf[(x as u16 - 3, y)].bg, bg, "pill starts two cells before the label (width {w})");
            assert_eq!(end + 6 + MARGIN as usize, w as usize, "right margin (width {w})");
        }
        // Nothing to bring forward: it updates everything all the same.
        app.latest.clear();
        assert!(has(&frame(&mut app, 100, 24), "↑ Update all ▏ ⋯"));
    }

    #[test]
    fn light_frames_a_running_row_on_both_its_rules() {
        let mut app = demo();
        app.motion.on = true;
        app.started = std::time::Instant::now() - Duration::from_secs(1);
        let buf = frame(&mut app, 100, 30);
        let top = |name: &str| {
            let i = app.hosts.iter().position(|h| h.name == name).unwrap();
            app.hits.iter().find(|(_, h)| *h == Hit::Card(i)).unwrap().0.y
        };
        let lit = |y: u16| (0..buf.area.width).any(|x| buf[(x, y)].symbol() == "─" && buf[(x, y)].fg != LINE);
        // one-m runs: the rules over and under it light up together (mdr's top is one-m's bottom).
        assert!(lit(top("one-m")) && lit(top("mdr")));
        // Nothing runs on or is selected around the last row's rules.
        assert!(!lit(top("box")) && !lit(top("box") + 4));
    }

    #[test]
    fn healthy_says_nothing_updated_sits_under_the_versions() {
        let mut app = demo();
        let buf = frame(&mut app, 100, 30);
        assert!(!has(&buf, "healthy") && !has(&buf, " ok"), "{}", text(&buf).join("\n"));
        app.hosts[1].reset(crate::model::Mode::Update, "all");
        app.on_job("one-s", Event::Done("T3: t3 0.0.46-nightly.20261003.2648".into()));
        app.on_job("one-s", Event::Complete);
        app.on_job("one-s", Event::Exit { code: Some(0), error: None });
        let buf = frame(&mut app, 100, 30);
        let rows = text(&buf);
        let one_s = rows.iter().position(|r| r.contains('┆') && r.contains("one-s")).unwrap();
        assert!(rows[one_s + 1].contains("✓ updated"), "{}", rows.join("\n"));
    }

    #[test]
    fn dashboard_at_60x24() {
        let mut app = demo();
        let buf = frame(&mut app, 60, 24);
        for needle in ["t3up", "Update 2 servers", "one-s", "↑#2623", "? help"] {
            assert!(has(&buf, needle), "{needle:?} missing from\n{}", text(&buf).join("\n"));
        }
        assert_eq!(geometry(&app).cols(), 1);
        // Too small for even a header: still draws.
        frame(&mut app, 20, 6);
        frame(&mut app, 1, 1);
    }

    #[test]
    fn menu_toasts_and_changelog_render() {
        let mut app = demo();
        app.open_actions();
        app.toast(Sev::Error, "Failed", "mdr: out of disk");
        let buf = frame(&mut app, 100, 30);
        for needle in [
            "one-s",
            "healthy",
            "UPDATE",
            "Everything",
            "T3 server",
            "→ latest nightly",
            "INSPECT",
            "Check again",
            "What's new",
            "Show output",
            "Create pairing link",
            "Terminal",
            "↑↓ move · enter choose · esc close",
        ] {
            assert!(has(&buf, needle), "{needle:?} missing from\n{}", text(&buf).join("\n"));
        }
        // The backdrop is dimmed.
        assert_ne!(buf[(2, 10)].fg, TEXT);
        let rows = app.hits.iter().filter(|(_, h)| matches!(h, Hit::Row(_))).count();
        assert!(rows >= 12, "{rows}");
        // Toasts wait under an open window, and show once it closes.
        app.modal = None;
        let buf = frame(&mut app, 100, 30);
        assert!(has(&buf, "mdr: out of disk"), "{}", text(&buf).join("\n"));
    }

    #[test]
    fn output_panel_shows_the_selected_hosts_log() {
        let mut app = demo();
        app.selected = 3;
        app.show_output = true;
        let buf = frame(&mut app, 100, 30);
        assert!(has(&buf, "mdr output · mdr-153012-check.log"), "{}", text(&buf).join("\n"));
        assert!(has(&buf, "✗ Codex  EACCES: permission denied"));
        assert!(has(&buf, "Codex: EACCES"), "tagged line");
        // The output's tag, not mdr's row above it.
        let rows = text(&buf);
        let (y, row) = rows.iter().enumerate().rev().find(|(_, r)| r.contains("Codex:")).unwrap();
        let x = row[..row.find("Codex:").unwrap()].chars().count();
        assert_eq!(buf[(x as u16, y as u16)].fg, ACCENT);
    }

    #[test]
    fn six_tools_get_a_column_each_and_narrow_screens_shed_the_extras() {
        let mut app = demo();
        app.on_job("one-s", Event::Done("Pi: 0.5.0".into()));
        app.on_job("one-s", Event::Done("Grok: grok 0.3.0".into()));
        app.on_job("one-s", Event::Exit { code: Some(0), error: None });
        let buf = frame(&mut app, 130, 30);
        assert!(has(&buf, "0.5.0") && has(&buf, "0.3.0") && has(&buf, "disk"), "{}", text(&buf).join("\n"));
        assert_eq!(geometry(&app).table.tools.len(), 6);
        // Narrower: the meters go before any version does.
        let buf = frame(&mut app, 100, 30);
        assert!(has(&buf, "0.5.0") && !has(&buf, "disk"), "{}", text(&buf).join("\n"));
    }

    #[test]
    fn a_nightly_too_wide_keeps_its_build() {
        assert_eq!(fit("0.0.46 #2623", 12), "0.0.46 #2623");
        assert_eq!(fit("0.0.46 #2623", 11), "#2623");
        assert_eq!(fit("↑0.0.46 #2648", 12), "↑#2648");
        assert_eq!(fit("2.1.289 (stable)", 8), "2.1.289…");
    }

    #[test]
    fn minutes_read_as_a_stopwatch() {
        assert_eq!(minutes(Duration::from_secs(42)), "42s");
        assert_eq!(minutes(Duration::from_secs(187)), "3m 07s");
    }

    #[test]
    fn truncate_and_wrap() {
        assert_eq!(truncate("hello world", 8), "hello w…");
        assert_eq!(truncate("hi", 8), "hi");
        assert_eq!(wrap("aaa bbb ccc", 7), ["aaa bbb", "ccc"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
    }

    /// Every window at every size from absurdly small to large: arithmetic on sizes must never panic.
    #[test]
    fn nothing_panics_at_any_size() {
        use crate::tui::app::{Changes, Servers};
        let logos = Logos::halfblocks();
        let mut app = demo();
        app.toast(Sev::Error, "Failed", "a long message that has to wrap over several lines in a narrow toast");
        let key =
            ("T3".to_string(), "0.0.46-nightly.20261003.2623".to_string(), "0.0.46-nightly.20261003.2648".to_string());
        let ssh = vec!["alpha".to_string(), "beta".to_string(), "a-rather-long-host-name.example.com".to_string()];
        let servers = Servers {
            text: String::new(),
            hosts: vec!["one-s".into(); 20],
            input: crate::tui::input::Input::new("typed"),
            ssh,
            row: Some(3),
            tab: 0,
        };
        let states: Vec<Option<Modal>> = vec![
            None,
            Some(Modal::Help { scroll: 5 }),
            Some(Modal::Servers(servers)),
            Some(Modal::Changes(Changes { host: "one-s".into(), items: vec![key], scroll: 99 })),
            Some(Modal::Changes(Changes { host: "box".into(), items: vec![], scroll: 0 })),
        ];
        let sizes: Vec<u16> = (1..=24).chain((25..=140).step_by(13)).collect();
        for (i, state) in states.iter().enumerate() {
            for output in [false, true] {
                for &w in &sizes {
                    for h in (1..=10).chain((11..=50).step_by(8)) {
                        app.modal = state.clone();
                        app.show_output = output;
                        app.selected = i % 4;
                        let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
                        render(&mut app, &logos, &mut buf);
                    }
                }
            }
        }
        // The menus, palette, confirm and prompts, from the app's own entry points.
        for open in 0..6 {
            for &w in &sizes {
                for h in (1..=10).chain((11..=50).step_by(8)) {
                    app.modal = None;
                    app.show_output = false;
                    match open {
                        0 => app.open_actions(),
                        1 => app.open_update_all(),
                        2 => app.open_version(),
                        3 => app.begin_update(vec!["one-s".into()], "all"),
                        4 => app.open_desktop(),
                        _ => {
                            app.modal = Some(Modal::Palette(crate::tui::app::Palette {
                                input: crate::tui::input::Input::new("x"),
                                items: vec![("Refresh".into(), crate::tui::app::Cmd::Refresh); 30],
                                cursor: 20,
                            }))
                        }
                    }
                    let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
                    render(&mut app, &logos, &mut buf);
                }
            }
        }
    }
}
