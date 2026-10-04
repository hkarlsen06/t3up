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
use crate::model::{COMPONENTS, Host, Status, StepState, compact, newer, short, updates, versions_detail};

/// A tool's tile: its width, and the gap between tiles.
pub const TILE: u16 = 9;
pub const GAP: u16 = 1;
/// Rows of one tile: logo, version, note.
const TILE_ROWS: u16 = logos::HEIGHT + 2;
const HEADER: u16 = 3;
/// Blank rows between a card's top border and its logos.
const TOP_PAD: u16 = 1;

// ── geometry ───────────────────────────────────────────────────────────────

/// Where the card grid and the output panel go, from the screen size and what is in them.
pub struct Geo {
    pub cols: usize,
    pub card_w: u16,
    pub card_h: u16,
    /// Cards too wide for the screen: one full-width card, tiles wrapped.
    pub wrapped: bool,
    pub per_row: usize,
    pub tile_rows: usize,
    pub gutter: u16,
    pub left: u16,
    pub rows: usize,
    pub content_h: u16,
    pub view: Rect,
    pub out: Rect,
}

pub fn geometry(app: &App) -> Geo {
    let (w, h) = app.size;
    let body = h.saturating_sub(HEADER + 1);
    let out_h = if app.show_output { (body * 45 / 100).max(5).min(body) } else { 0 };
    let view = Rect::new(0, HEADER, w, body - out_h);
    let out = Rect::new(0, HEADER + body - out_h, w, out_h);
    let n = app.tools.len().max(1) as u16;
    let want = 36.max(n * (TILE + GAP) - GAP + 4);
    let avail = w.saturating_sub(4);
    let wrapped = avail < want;
    let card_w = if wrapped { avail.max(12) } else { want };
    let cols = if wrapped { 1 } else { app.hosts.len().min(((avail + 2) / (card_w + 2)) as usize).max(1) };
    let per_row = if wrapped { ((card_w.saturating_sub(4) + GAP) / (TILE + GAP)).max(1) as usize } else { n as usize };
    let tile_rows = (n as usize).div_ceil(per_row);
    let error_row = app.hosts.iter().any(|h| !h.error.is_empty() && !h.steps.is_empty()) as u16;
    let card_h = 2 + TOP_PAD + tile_rows as u16 * TILE_ROWS + (tile_rows as u16 - 1) + 1 + error_row;
    let rows = app.hosts.len().div_ceil(cols).max(1);
    let gutter = if cols > 1 { (w.saturating_sub(cols as u16 * card_w) / (cols as u16 + 1)).clamp(2, 6) } else { 0 };
    let block = cols as u16 * card_w + (cols as u16 - 1) * gutter;
    Geo {
        cols,
        card_w,
        card_h,
        wrapped,
        per_row,
        tile_rows,
        gutter,
        left: w.saturating_sub(block) / 2,
        rows,
        content_h: 2 + rows as u16 * card_h + (rows as u16 - 1),
        view,
        out,
    }
}

pub fn max_scroll(app: &App) -> u16 {
    let g = geometry(app);
    g.content_h.saturating_sub(g.view.height)
}

/// The scroll offset that shows card `i`, moving as little as possible.
pub fn scroll_to(app: &App, i: usize) -> u16 {
    let g = geometry(app);
    let top = 1 + (i / g.cols) as u16 * (g.card_h + 1);
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
            (spinner(t).to_string(), AMBER, if h.mode == crate::model::Mode::Update { "updating" } else { "checking" })
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
        let solid: Vec<Rect> = app.hits.iter().filter(|(_, h)| matches!(h, Hit::Card(_))).map(|(r, _)| *r).collect();
        motion::dust(buf, geometry(app).view, &solid, t);
    }
    if app.show_output {
        output(app, buf);
    }
    footer(app, buf);
    if app.modal.is_some() {
        toasts(app, buf);
        let since = *app.motion.modal_since.get_or_insert_with(std::time::Instant::now);
        let k = if app.motion.on { motion::ease_out(since.elapsed().as_secs_f32() / motion::FADE) } else { 1.0 };
        dim(buf, 0.65 * k);
        modal(app, buf, logos);
        motion::fade_in(buf, app.modal_rect, k);
    } else {
        app.motion.modal_since = None;
        tooltip(app, buf);
        toasts(app, buf);
    }
    if let Some(t) = app.motion.intro_at() {
        let hosts: Vec<_> = app.hosts.iter().map(|h| (h.name.clone(), h.status)).collect();
        motion::intro(buf, t, &hosts);
    }
}

// ── header ─────────────────────────────────────────────────────────────────

/// Space between the screen's edge and the header's and footer's content.
const MARGIN: u16 = 2;

fn header(app: &mut App, buf: &mut Buffer) {
    let w = buf.area.width;
    buf.set_style(Rect::new(0, 0, w, HEADER), Style::new().bg(SURFACE));
    // A filled pill on the text row, like the badge on the left: padded two cells each side.
    let label = "↑ Update all";
    let bw = label.width() as u16 + 4;
    let shown = w >= 40;
    let button = Rect::new(w.saturating_sub(bw + MARGIN), 1, bw, 1);
    let enabled = !app.hosts.is_empty();
    if shown {
        let t = app.clock().as_secs_f32();
        let hovered = app.hover.as_ref().is_some_and(|h| h.hit == Some(Hit::UpdateAll));
        let waiting =
            app.hosts.iter().any(|h| h.status == Status::Ok && !updates(h, &app.latest, &app.target).is_empty());
        let alpha = match (enabled, hovered) {
            (false, _) => 0.0,
            (true, true) => 0.42,
            // Updates waiting: it breathes, gently.
            (true, false) if waiting && app.motion.on => 0.2 + 0.12 * (0.5 + 0.5 * (t * 2.2).sin()),
            (true, false) => 0.22,
        };
        let bg = if enabled { blend(ACCENT, SURFACE, alpha) } else { blend(FAINT, SURFACE, 0.35) };
        let color = if enabled { if hovered { TEXT } else { ACCENT } } else { DIM };
        buf.set_style(button, Style::new().bg(bg));
        put(buf, button.x + 2, 1, bw - 2, &Line::styled(label, bold(color)));
        if enabled {
            // The whole height of the bar is clickable, not just the text row.
            app.hits.push((Rect::new(button.x, 0, bw, HEADER), Hit::UpdateAll));
        }
    }
    let right_edge = if shown { button.x.saturating_sub(3) } else { w.saturating_sub(MARGIN) };
    let avail = right_edge.saturating_sub(MARGIN) as usize;
    let (left, right) = header_text(app, avail);
    let right_w = right.width() as u16;
    put(buf, MARGIN, 1, avail as u16, &left);
    put(buf, right_edge - right_w.min(avail as u16), 1, right_w, &right);
    if app.motion.on {
        motion::sweep_bg(buf, Rect::new(MARGIN, 1, 6.min(w.saturating_sub(MARGIN)), 1), app.clock().as_secs_f32(), 6.0);
    }
}

/// The widest layout that fits: narrow terminals lose where t3up runs, then the long counts,
/// then the long desktop line (an outdated desktop is never dropped, only shortened).
pub fn header_text(app: &App, width: usize) -> (Line<'static>, Line<'static>) {
    let badge = Span::styled(" t3up ", Style::new().fg(BG).bg(ACCENT).add_modifier(Modifier::BOLD));
    let on = if app.local.is_empty() {
        vec![]
    } else {
        vec![Span::styled("   On ", fg(DIM)), Span::styled(app.local.clone(), fg(TEXT))]
    };
    let pinned = if app.target.is_empty() {
        vec![]
    } else {
        vec![Span::styled("   Installs ", fg(DIM)), Span::styled(app.target.clone(), fg(AMBER))]
    };
    let behind = app.outdated_desktop();
    let (full, terse): (Vec<Span>, Vec<Span>) = if app.desktop_updating {
        let s = vec![Span::styled("   updating desktop…", fg(AMBER))];
        (s.clone(), s)
    } else if !behind.is_empty() {
        let bump = compact(&behind, &app.desktop);
        (
            vec![
                Span::styled("   Desktop ", fg(DIM)),
                Span::styled(short(&app.desktop), fg(AMBER)),
                Span::styled(format!(" → {bump} · d to update"), fg(AMBER)),
            ],
            vec![Span::styled(format!("   update desktop → {bump}"), fg(AMBER))],
        )
    } else {
        let v = short(&app.desktop);
        (
            vec![
                Span::styled("   Desktop ", fg(DIM)),
                Span::styled(if v.is_empty() { "not detected".to_string() } else { v }, fg(TEXT)),
            ],
            vec![],
        )
    };
    let count = |f: &dyn Fn(&Host) -> bool| app.hosts.iter().filter(|h| f(h)).count();
    let counts = [
        (count(&Host::running), AMBER, "busy", "…"),
        (app.queued.len(), DIM, "queued", "◌"),
        (count(&|h| h.status == Status::Ok && !crate::model::no_t3(h)), GREEN, "ok", "✓"),
        (count(&|h| h.status == Status::Ok && crate::model::no_t3(h)), AMBER, "no T3", "○"),
        (
            count(&|h| h.status == Status::Ok && !updates(h, &app.latest, &app.target).is_empty()),
            ACCENT,
            "can update",
            "↑",
        ),
        (count(&|h| h.status == Status::Failed), RED, "failed", "✗"),
    ];
    let tally = |short: bool| -> Vec<Span<'static>> {
        counts
            .iter()
            .filter(|c| c.0 > 0)
            .map(|&(n, color, label, mark)| {
                let text = if short { format!("  {mark} {n}") } else { format!("  ● {n} {label}") };
                Span::styled(text, fg(color))
            })
            .collect()
    };
    let cat = |parts: &[&Vec<Span<'static>>]| -> Vec<Span<'static>> {
        std::iter::once(badge.clone()).chain(parts.iter().flat_map(|p| p.iter().cloned())).collect()
    };
    let layouts = [
        (cat(&[&on, &full, &pinned]), false),
        (cat(&[&full, &pinned]), false),
        (cat(&[&full, &pinned]), true),
        (cat(&[&terse, &pinned]), true),
        (cat(&[&terse]), true),
    ];
    let mut chosen = None;
    for (parts, short) in &layouts {
        let (l, r) = (Line::from(parts.clone()), Line::from(tally(*short)));
        if l.width() + r.width() <= width {
            chosen = Some((l, r));
            break;
        }
    }
    // Nothing fits with the counts: they go, the desktop hint stays.
    chosen.unwrap_or_else(|| (Line::from(cat(&[&terse])), Line::default()))
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
        let verb = if running.iter().any(|h| h.mode == crate::model::Mode::Update) { "Updating" } else { "Checking" };
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
    for i in 0..app.hosts.len() {
        let (row, col) = ((i / g.cols) as i32, (i % g.cols) as u16);
        let x = g.left + col * (g.card_w + g.gutter);
        let y = g.view.y as i32 + 1 + row * (g.card_h as i32 + 1) - app.scroll as i32;
        if y + g.card_h as i32 <= g.view.y as i32 || y >= g.view.bottom() as i32 {
            continue;
        }
        let mut card = Buffer::empty(Rect::new(0, 0, g.card_w, g.card_h));
        card.set_style(card.area, Style::new().bg(BG).fg(TEXT));
        let tiles = draw_card(app, &g, logos, i, &mut card, dim_logos);
        let visible = blit(&card, buf, x, y, g.view);
        app.hits.push((visible, Hit::Card(i)));
        // Each tool's tile, on top of its card: tapping one opens that tool's menu.
        for (tile, name) in tiles {
            let top = y + tile.y as i32;
            if top >= g.view.y as i32 && top + tile.height as i32 <= g.view.bottom() as i32 {
                app.hits.push((Rect::new(x + tile.x, top as u16, tile.width, tile.height), Hit::Tool(i, name)));
            }
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

/// The tool's version for its tile: '↑ 2.1.289' after an update, a spinner while it runs.
fn tile_version(app: &App, h: &Host, name: &str) -> Span<'static> {
    let (state, value) = h.steps.get(name).map_or((None, ""), |(s, v)| (Some(*s), v.as_str()));
    let span = if state == Some(StepState::Begin) && h.running() {
        Span::styled(spinner(app.clock()).to_string(), fg(AMBER))
    } else if matches!(state, Some(StepState::Begin | StepState::Fail)) {
        Span::styled("✗ failed", fg(RED))
    } else if value.contains(" → ") {
        let done = match (value.ends_with(')'), h.current.get(name)) {
            (true, Some(cur)) => format!("#{}", cur.rsplit('.').next().unwrap_or(cur)),
            _ => value.rsplit(" → ").next().unwrap_or(value).to_string(),
        };
        Span::styled(format!("↑ {done}"), bold(GREEN))
    } else {
        let value = if value.is_empty() { "—" } else { value };
        Span::styled(value.to_string(), fg(if state.is_some() { TEXT } else { FAINT }))
    };
    let text = span.content.replace("-nightly", "");
    let text = if state == Some(StepState::Done) { app.motion.decode(&h.name, name, &text) } else { text };
    Span::styled(text, span.style)
}

/// One line under the version: 'sign in', or the update waiting. Always there, so cards are equal height.
fn tile_note(app: &App, h: &Host, name: &str, new: Option<&String>) -> Span<'static> {
    if h.auth.iter().any(|a| a == name) {
        Span::styled("sign in", fg(AMBER))
    } else if let (Some(new), Some(cur), false) = (new, h.current.get(name), h.running()) {
        let _ = app;
        Span::styled(format!("→ {}", compact(new, cur)), fg(ACCENT))
    } else {
        Span::raw("")
    }
}

/// Draw card `i` into `buf`; returns where its tools' tiles are (in the card), to tap.
fn draw_card(
    app: &App,
    g: &Geo,
    logos: &Logos,
    i: usize,
    buf: &mut Buffer,
    dim_logos: bool,
) -> Vec<(Rect, &'static str)> {
    let mut tiles = vec![];
    let h = &app.hosts[i];
    let t = app.clock();
    let queued = app.queued.contains(&h.name);
    let selected = i == app.selected;
    let border = if selected {
        ACCENT
    } else if h.status == Status::Failed {
        blend(RED, BG, 0.6)
    } else {
        LINE
    };
    let border = app.motion.border(&h.name, border);
    let (icon, color, label) = status(h, queued, t);
    let new = updates(h, &app.latest, &app.target);
    let mut sub = vec![Span::raw(" "), Span::styled(format!("{icon} {label}"), fg(color))];
    // A queued server (waiting for the canary) has nothing more to say.
    if !queued {
        if h.status == Status::Ok && !new.is_empty() {
            sub.push(Span::styled(format!(" · {} to update", new.len()), fg(ACCENT)));
        }
        if matches!(h.status, Status::Ok | Status::Failed | Status::Running) {
            let secs = h.took().as_secs_f64();
            let took = if h.running() { format!("{secs:.0}s") } else { format!("{secs:.1}s") };
            sub.push(Span::styled(format!("  {took}"), fg(DIM)));
        }
    }
    sub.push(Span::raw(" "));
    // What needs attention sits on the top border, opposite the name.
    let mut alert: Vec<Span<'static>> = vec![];
    if let Some(n) = h.busy.filter(|&n| n > 0) {
        alert.push(Span::styled(format!(" ● {n} agent{} live ", if n == 1 { "" } else { "s" }), fg(AMBER)));
    }
    if !h.rollback.is_empty() {
        alert.push(Span::styled(format!(" ↩ rolled back {} ", h.rollback), fg(AMBER)));
    }
    let name_w = h.name.width() + 4;
    let alert = clip(alert, (g.card_w as usize).saturating_sub(name_w + 4));
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(fg(border))
        .title(Line::styled(format!(" {} ", h.name), bold(if selected { ACCENT } else { TEXT })))
        .title(alert.right_aligned())
        .title_bottom(Line::from(sub).right_aligned())
        .render(buf.area, buf);
    if app.motion.on && h.running() {
        let update = h.mode == crate::model::Mode::Update;
        let secs = t.as_secs_f32() + i as f32 * 0.37; // cards out of step, not marching together
        motion::comet(buf, secs, motion::running_color(update), if update { 2 } else { 1 }, 42.0, 1.0);
        let bottom = buf.area.bottom() - 1;
        motion::shimmer(buf, buf.area, bottom, secs, 1.8);
    } else if app.motion.on && selected {
        motion::comet(buf, t.as_secs_f32(), blend(TEXT, ACCENT, 0.6), 1, 9.0, 0.55);
    }

    let (x0, inner_w) = (2u16, g.card_w.saturating_sub(4));
    let top = 1 + TOP_PAD;
    let tiles_h = g.tile_rows as u16 * TILE_ROWS + (g.tile_rows as u16 - 1);
    let missing = h.status == Status::Ok && crate::model::no_t3(h);
    if missing || h.status == Status::Failed && h.steps.is_empty() {
        // Never got as far as a step, or there's no T3 here: say so, as tall as the tiles.
        let lines = if missing {
            [
                Line::styled("T3 isn't installed on this server", fg(AMBER)),
                Line::styled("enter → install T3 (latest nightly)", fg(DIM)),
            ]
        } else {
            [
                clip(vec![Span::styled(format!("✗ {}", h.error), fg(RED))], inner_w as usize),
                Line::styled("enter → retry or open a terminal", fg(DIM)),
            ]
        };
        for (k, line) in lines.iter().enumerate() {
            put(buf, x0, top + 1 + k as u16, inner_w, line);
        }
    } else {
        let present: Vec<&str> = app
            .tools
            .iter()
            .copied()
            .filter(|n| h.installed.contains(*n) && !matches!(h.steps.get(*n), Some((StepState::Skip, _))))
            .collect();
        let slot = if g.wrapped { TILE + GAP } else { inner_w / app.tools.len().max(1) as u16 };
        let places: Vec<(u16, u16, &str)> = if g.wrapped {
            present
                .iter()
                .enumerate()
                .map(|(k, n)| ((k % g.per_row) as u16 * slot, (k / g.per_row) as u16, *n))
                .collect()
        } else {
            app.tools
                .iter()
                .enumerate()
                .filter(|(_, n)| present.contains(n))
                .map(|(k, n)| (k as u16 * slot, 0, *n))
                .collect()
        };
        for (dx, row, name) in places {
            let tx = x0 + dx + if g.wrapped { 0 } else { slot.saturating_sub(TILE) / 2 };
            let ty = top + row * (TILE_ROWS + 1);
            tiles.push((Rect::new(tx, ty, TILE, TILE_ROWS), name));
            let size = logos.size(name);
            let lx = tx + TILE.saturating_sub(size.width) / 2;
            logos.draw(name, Rect::new(lx, ty, size.width.min(TILE), size.height), buf, dim_logos);
            let version = tile_version(app, h, name);
            let note = tile_note(app, h, name, new.iter().find(|(n, _)| n == name).map(|(_, v)| v));
            for (k, span) in [version, note].into_iter().enumerate() {
                let text = truncate(&span.content, TILE as usize);
                let (cx, line) = centered(Line::styled(text, span.style), TILE);
                put(buf, tx + cx, ty + logos::HEIGHT + k as u16, TILE, &line);
            }
        }
    }
    // The machine, then any error.
    put(buf, x0, top + tiles_h, inner_w, &clip(vec![Span::styled(h.sys.clone(), fg(DIM))], inner_w as usize));
    if !h.error.is_empty() && !h.steps.is_empty() {
        put(
            buf,
            x0,
            top + tiles_h + 1,
            inner_w,
            &clip(vec![Span::styled(format!("✗ {}", h.error), fg(RED))], inner_w as usize),
        );
    }
    tiles
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
fn pills(app: &mut App, buf: &mut Buffer, x: u16, y: u16, w: u16, items: &[(&str, &str, Act)]) {
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
        buf.set_style(area, Style::new().bg(blend(ACCENT, SURFACE, if hovered { 0.34 } else { 0.14 })));
        let line = Line::from(vec![
            Span::styled(format!("  {key}"), bold(ACCENT)),
            Span::styled(format!(" {label}  "), fg(if hovered { TEXT } else { MUTED })),
        ]);
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
            inner.x,
            y,
            inner.width,
            &[("r", "Try again", Act::Retry), ("t", "Open in terminal", Act::Terminal)],
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
            pills(app, buf, x, py, dw, &acts);
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
                inner.x + 3,
                y + 3,
                inner.width - 3,
                &[(&open_key, "Open in browser", Act::Open), (&link_key, "Copy link", Act::CopyLink)],
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
                    inner.x + 3 + cw + 3,
                    y + 2,
                    inner.width.saturating_sub(cw + 6),
                    &[("c", "Copy code", Act::CopyCode)],
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
                inner.x,
                y,
                inner.width,
                &[(&open_key, "Open link", Act::Open), (&link_key, "Copy link", Act::CopyLink)],
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

const HELP: [(&str, &str); 20] = [
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

fn toasts(app: &App, buf: &mut Buffer) {
    let w = 46u16.min(buf.area.width.saturating_sub(4));
    if w < 12 {
        return;
    }
    let mut bottom = buf.area.height.saturating_sub(2); // one row of air above the footer
    let now = std::time::Instant::now();
    for Toast { title, text, sev, born, until } in app.toasts.iter().rev() {
        let color = match sev {
            Sev::Info => ACCENT,
            Sev::Warning => AMBER,
            Sev::Error => RED,
        };
        let rows = wrap(text, w as usize - 4);
        let h = 3 + rows.len() as u16;
        if bottom < HEADER + 1 + h {
            break;
        }
        let y = bottom - h;
        // Slides in from the right edge, and back out just before it expires.
        let (age, left) = (now.duration_since(*born).as_secs_f32(), until.saturating_duration_since(now).as_secs_f32());
        let out = if app.motion.on {
            let k = motion::ease_out(age / motion::SLIDE).min(motion::ease_out(left / motion::SLIDE));
            ((1.0 - k) * (w + 2) as f32) as u16
        } else {
            0
        };
        let whole = Rect::new(0, 0, w, h);
        let mut card = Buffer::empty(whole);
        fill(&mut card, whole, SURFACE);
        Block::bordered().border_type(BorderType::Rounded).border_style(fg(color)).render(whole, &mut card);
        put(&mut card, 2, 1, w - 4, &Line::styled(title.clone(), bold(color)));
        for (k, row) in rows.iter().enumerate() {
            put(&mut card, 2, 2 + k as u16, w - 4, &Line::styled(row.clone(), fg(TEXT)));
        }
        blit(&card, buf, buf.area.width - w - MARGIN + out, y as i32, buf.area);
        bottom = y;
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
    fn dashboard_at_100x30() {
        let mut app = demo();
        let buf = frame(&mut app, 100, 30);
        for needle in [
            " t3up ",
            "Update all",
            "Desktop",
            "→ #2648 · d to update",
            "● 1", // header; tallies are terse at this width
            "one-s",
            "one-m",
            "mdr",
            "box",
            "✓ healthy · 3 to update",
            "0.0.46",
            "0.160.0",
            "2.1.288",
            "1.18.34",
            "→ #2648",
            "→ 0.161.0",
            "sign in",
            "load 0.24 · disk 21%",
            "● 2 agents live",
            "updating",
            "↑ 2.1.289",
            "✗ failed",
            "Codex: EACCES",
            "enter → retry or open a terminal",
            "enter actions",
            "q quit",
        ] {
            if needle == "● 1" {
                continue;
            }
            assert!(has(&buf, needle), "{needle:?} missing from\n{}", text(&buf).join("\n"));
        }
        // The selected card has the accent border, others the line color.
        let card = |i| app.hits.iter().find(|(_, h)| *h == Hit::Card(i)).unwrap().0;
        assert_eq!(buf[(card(0).x, card(0).y)].fg, ACCENT);
        assert_eq!(buf[(card(1).x, card(1).y)].fg, LINE);
        assert_eq!(cell_fg(&buf, "✗ failed").unwrap(), RED);
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
    fn a_server_without_t3_offers_the_install() {
        let mut app = demo();
        let h = &mut app.hosts[1];
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
            let row = &text(&buf)[1];
            let x = row.find("↑ Update all").map(|b| row[..b].chars().count()).expect("button");
            let end = x + "↑ Update all".chars().count();
            let bg = buf[(x as u16, 1)].bg;
            // Two cells of the pill on each side of the label, then the screen's margin.
            for dx in [x - 2, x - 1, end, end + 1] {
                assert_eq!(buf[(dx as u16, 1)].bg, bg, "pill padding at {dx} (width {w})");
            }
            assert_ne!(buf[(x as u16 - 3, 1)].bg, bg, "pill starts two cells before the label (width {w})");
            assert_eq!(end + 2 + MARGIN as usize, w as usize, "right margin (width {w})");
        }
    }

    #[test]
    fn dashboard_at_60x24() {
        let mut app = demo();
        let buf = frame(&mut app, 60, 24);
        for needle in ["Update all", "one-s", "✓ healthy · 3 to update", "update desktop → #2648", "? help"] {
            assert!(has(&buf, needle), "{needle:?} missing from\n{}", text(&buf).join("\n"));
        }
        assert_eq!(geometry(&app).cols, 1);
        // Too small for even a header: still draws.
        frame(&mut app, 20, 6);
        frame(&mut app, 1, 1);
    }

    #[test]
    fn six_tools_wrap_in_a_narrow_card() {
        let mut app = demo();
        app.on_job("one-s", Event::Done("Pi: 0.5.0".into()));
        app.on_job("one-s", Event::Done("Grok: grok 0.3.0".into()));
        app.on_job("one-s", Event::Exit { code: Some(0), error: None });
        let _ = frame(&mut app, 100, 30); // fits in a row: one card per column
        assert!(!geometry(&app).wrapped);
        let buf = frame(&mut app, 60, 40);
        let g = geometry(&app);
        assert!(g.wrapped && g.tile_rows == 2, "{}", g.tile_rows);
        assert!(has(&buf, "0.5.0") && has(&buf, "0.3.0"), "{}", text(&buf).join("\n"));
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
        app.selected = 2;
        app.show_output = true;
        let buf = frame(&mut app, 100, 30);
        assert!(has(&buf, "mdr output · mdr-153012-check.log"), "{}", text(&buf).join("\n"));
        assert!(has(&buf, "✗ Codex  EACCES: permission denied"));
        assert!(has(&buf, "Codex: EACCES"), "tagged line");
        assert_eq!(cell_fg(&buf, "Codex:").unwrap(), ACCENT);
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
                        3 => app.begin_update(vec!["one-s".into()], "all", false),
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
