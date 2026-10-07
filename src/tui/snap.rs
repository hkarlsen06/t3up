//! Test-only: a demo dashboard in representative states, rendered to colored SVG
//! (`/tmp/t3up-snap-*.svg`) so the design can be looked at: `qlmanage -t -s 1600 -o /tmp /tmp/t3up-snap-*.svg`.
use std::collections::BTreeMap;
use std::fmt::Write;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};

use super::app::{App, Modal};
use super::logos::Logos;
use super::theme::{BG, TEXT, rgb};
use super::view;
use crate::model::{Event, Mode};

const CELL_W: f32 = 8.4;
const CELL_H: f32 = 17.0;

fn hex(color: Color, default: Color) -> String {
    let [r, g, b] = rgb(if color == Color::Reset { default } else { color });
    format!("#{r:02x}{g:02x}{b:02x}")
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The buffer as SVG: a rect per run of one background, a text per run of one style. Half and
/// full blocks (the logos' half-block fallback) are drawn as rects, since no font draws them flush.
pub fn svg(buf: &Buffer) -> String {
    let (w, h) = (buf.area.width, buf.area.height);
    // qlmanage crops anything but a square canvas.
    let side = (w as f32 * CELL_W).max(h as f32 * CELL_H).ceil();
    let mut out = String::new();
    let _ = write!(
        out,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{}" height="{}" viewBox="0 0 {} {}" font-family="Menlo, 'DejaVu Sans Mono', monospace" font-size="14"><rect width="100%" height="100%" fill="{}"/>"##,
        side,
        side,
        side,
        side,
        hex(BG, BG)
    );
    for y in 0..h {
        let top = y as f32 * CELL_H;
        // Backgrounds, merged along the row.
        let mut x = 0;
        while x < w {
            let cell = &buf[(x, y)];
            let reversed = cell.modifier.contains(Modifier::REVERSED);
            let color = hex(if reversed { cell.fg } else { cell.bg }, if reversed { TEXT } else { BG });
            let mut end = x + 1;
            while end < w {
                let c = &buf[(end, y)];
                let r = c.modifier.contains(Modifier::REVERSED);
                if hex(if r { c.fg } else { c.bg }, if r { TEXT } else { BG }) != color {
                    break;
                }
                end += 1;
            }
            if color != hex(BG, BG) {
                let _ = write!(
                    out,
                    r##"<rect x="{:.1}" y="{top:.1}" width="{:.1}" height="{CELL_H}" fill="{color}"/>"##,
                    x as f32 * CELL_W,
                    (end - x) as f32 * CELL_W
                );
            }
            x = end;
        }
        // Text.
        let mut x = 0;
        while x < w {
            let cell = &buf[(x, y)];
            let sym = cell.symbol();
            let reversed = cell.modifier.contains(Modifier::REVERSED);
            let fg = if reversed { cell.bg } else { cell.fg };
            let fg = hex(fg, if reversed { BG } else { TEXT });
            let px = x as f32 * CELL_W;
            match sym {
                "▀" => {
                    let _ = write!(
                        out,
                        r##"<rect x="{px:.1}" y="{top:.1}" width="{CELL_W}" height="{:.1}" fill="{fg}"/>"##,
                        CELL_H / 2.0
                    );
                    x += 1;
                    continue;
                }
                "▄" => {
                    let _ = write!(
                        out,
                        r##"<rect x="{px:.1}" y="{:.1}" width="{CELL_W}" height="{:.1}" fill="{fg}"/>"##,
                        top + CELL_H / 2.0,
                        CELL_H / 2.0
                    );
                    x += 1;
                    continue;
                }
                "█" => {
                    let _ = write!(
                        out,
                        r##"<rect x="{px:.1}" y="{top:.1}" width="{CELL_W}" height="{CELL_H}" fill="{fg}"/>"##
                    );
                    x += 1;
                    continue;
                }
                " " | "" => {
                    x += 1;
                    continue;
                }
                _ => {}
            }
            let mut end = x + 1;
            let mut text = sym.to_string();
            while end < w {
                let c = &buf[(end, y)];
                let r = c.modifier.contains(Modifier::REVERSED);
                let f = hex(if r { c.bg } else { c.fg }, if r { BG } else { TEXT });
                if f != fg || c.modifier != cell.modifier || matches!(c.symbol(), "▀" | "▄" | "█" | " " | "") {
                    break;
                }
                text.push_str(c.symbol());
                end += 1;
            }
            let weight = if cell.modifier.contains(Modifier::BOLD) { r#" font-weight="bold""# } else { "" };
            let _ = write!(
                out,
                r#"<text x="{px:.1}" y="{:.1}" fill="{fg}"{weight} textLength="{:.1}" lengthAdjust="spacing" xml:space="preserve">{}</text>"#,
                top + CELL_H - 4.5,
                (end - x) as f32 * CELL_W,
                esc(&text)
            );
            x = end;
        }
    }
    out.push_str("</svg>");
    out
}

/// One frame of the app at this size.
pub fn frame(app: &mut App, w: u16, h: u16) -> Buffer {
    let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
    view::render(app, &Logos::halfblocks(), &mut buf);
    buf
}

/// The buffer's text, one string per row.
pub fn text(buf: &Buffer) -> Vec<String> {
    (0..buf.area.height).map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>()).collect()
}

pub fn has(buf: &Buffer, needle: &str) -> bool {
    text(buf).iter().any(|l| l.contains(needle))
}

fn feed(app: &mut App, host: &str, events: &[Event]) {
    for e in events {
        app.on_job(host, e.clone());
    }
}

fn done(detail: &str) -> Event {
    Event::Done(detail.into())
}

/// Four servers in different states and this machine, as a real session might look.
pub fn demo() -> App {
    let names = ["one-s", "one-m", "mdr", "box"].map(String::from).to_vec();
    let known =
        BTreeMap::from([("one-s".to_string(), ["T3", "Codex", "Claude", "OpenCode"].map(String::from).to_vec())]);
    let mut app = App::new(names, "/tmp/t3up-demo/logs".into(), known);
    // Busiest first: the cards keep this order.
    app.threads = [("one-s", 120), ("one-m", 80), ("mdr", 40)].map(|(h, n)| (h.to_string(), n)).into();
    app.reorder();
    app.take_effects();
    app.local = "Ganz-Harbour".into();
    app.desktop = "0.0.46-nightly.20261003.2623".into();
    app.latest = [("T3", "0.0.46-nightly.20261003.2648"), ("Codex", "0.161.0"), ("Claude", "2.1.289")]
        .into_iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
    let t3 = "T3: t3 0.0.46-nightly.20261003.2623";
    let log = |h: &str| Event::Start { log: format!("/tmp/{h}-153012-check.log").into() };
    // Healthy, an update waiting, a tool signed out, agents running, a machine line.
    app.hosts[0].reset(Mode::Check, "all");
    feed(
        &mut app,
        "one-s",
        &[
            log("one-s"),
            Event::Begin("T3".into()),
            done(t3),
            done("Codex: codex-cli 0.160.0"),
            done("Claude: 2.1.288 (Claude Code)"),
            done("OpenCode: 1.18.34"),
            Event::Auth("Claude".into()),
            done("Health: OK"),
            Event::Sys("cpu 6% · cpus 4 · disk 21% · up 1d 5h".into()),
            Event::Busy(2),
            Event::Complete,
            Event::Exit { code: Some(0), error: None },
        ],
    );
    // An update running: T3 done, Codex in progress.
    app.hosts[1].reset(Mode::Update, "all");
    feed(
        &mut app,
        "one-m",
        &[
            log("one-m"),
            Event::Begin("T3".into()),
            Event::Begin("Codex".into()),
            Event::Begin("Claude".into()),
            done("T3: t3 0.0.46-nightly.20261003.2623 -> t3 0.0.46-nightly.20261003.2648"),
            done("Claude: 2.1.288 (Claude Code) -> 2.1.289 (Claude Code)"),
            Event::Output { tag: "Codex".into(), text: "added 1 package in 2s".into() },
            Event::Sys("cpu 48% · disk 63% · up 12d 2h".into()),
        ],
    );
    app.hosts[1].installed.insert("Codex".into());
    app.hosts[1].installed.insert("Claude".into());
    // A step failed.
    app.hosts[2].reset(Mode::Update, "all");
    feed(
        &mut app,
        "mdr",
        &[
            log("mdr"),
            Event::Begin("T3".into()),
            done("T3: t3 0.0.46-nightly.20261003.2623 -> t3 0.0.46-nightly.20261003.2648"),
            Event::Output {
                tag: "Codex".into(),
                text: "EACCES: permission denied, mkdir '/usr/lib/node_modules'".into(),
            },
            Event::Fail("Codex".into()),
            Event::Exit { code: Some(1), error: None },
        ],
    );
    // Never got as far as a step.
    app.hosts[3].reset(Mode::Check, "all");
    feed(
        &mut app,
        "box",
        &[
            log("box"),
            Event::Output { tag: String::new(), text: "ssh: connect to host box port 22: Connection refused".into() },
            Event::Exit { code: Some(255), error: None },
        ],
    );
    // This machine: its providers, and the desktop app as its T3.
    app.show_local();
    app.on_result(super::app::Res::Desktop("0.0.46-nightly.20261003.2623".into()));
    feed(
        &mut app,
        "Ganz-Harbour",
        &[
            log("Ganz-Harbour"),
            done("Codex: codex-cli 0.161.0"),
            done("Claude: 2.1.289 (Claude Code)"),
            Event::Sys("cpu 21% · cpus 10 · disk 74% · up 3d 4h".into()),
            Event::Complete,
            Event::Exit { code: Some(0), error: None },
        ],
    );
    app.selected = 1; // one-s
    app.take_effects();
    app.toasts.clear();
    app.modal = None;
    app
}

fn save(name: &str, app: &mut App, w: u16, h: u16) {
    let buf = frame(app, w, h);
    std::fs::write(format!("/tmp/t3up-snap-{name}.svg"), svg(&buf)).expect("write snapshot");
}

#[test]
fn snapshots() {
    let mut app = demo();
    save("dashboard", &mut app, 100, 30);
    save("wide", &mut app, 113, 36);
    save("roomy", &mut app, 140, 44);
    app.show_output = true;
    app.selected = 3;
    save("output", &mut app, 100, 30);
    app.show_output = false;
    app.selected = 1;
    app.open_actions();
    app.toast(super::app::Sev::Info, "Done", "one-m updated in 12s");
    app.toast(super::app::Sev::Error, "Failed", "mdr: Codex: EACCES: permission denied, mkdir '/usr/lib/node_modules'");
    save("menu", &mut app, 100, 30);
    app.toasts.clear();
    app.modal = None;
    app.open_update_all();
    save("update-all", &mut app, 100, 30);
    app.modal = Some(Modal::Help { scroll: 0 });
    save("help", &mut app, 100, 30);
    app.modal = None;
    let hosts = ["one-s", "one-m", "mdr", "box"].map(String::from).to_vec();
    let ssh = ["staging", "prod-eu", "one-s"].map(String::from).to_vec();
    app.modal = Some(Modal::Servers(super::app::Servers {
        text: String::new(),
        hosts,
        input: Default::default(),
        ssh,
        row: Some(1),
        tab: 0,
    }));
    save("servers", &mut app, 100, 30);
    app.modal = None;
    app.begin_update(vec!["one-s".into(), "one-m".into()], "all");
    save("confirm", &mut app, 100, 30);
    app.modal = None;
    app.open_version();
    save("version", &mut app, 100, 30);
    app.modal = None;
    app.on_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('p'),
        crossterm::event::KeyModifiers::CONTROL,
    ));
    for c in "upd".chars() {
        app.on_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char(c),
            crossterm::event::KeyModifiers::NONE,
        ));
    }
    save("palette", &mut app, 100, 30);
    app.modal = None;
    app.queued.insert("mdr".into());
    app.queued.insert("box".into());
    app.hosts[3].status = crate::model::Status::Ok;
    save("queued", &mut app, 100, 30);
    app.queued.clear();
    app.hosts[1].installed.extend(["Grok".to_string(), "Pi".to_string()]);
    app.hosts[1].current.insert("Grok".into(), "0.3.0".into());
    app.hosts[1].steps.insert("Grok".into(), (crate::model::StepState::Done, "0.3.0".into()));
    app.hosts[1].steps.insert("Pi".into(), (crate::model::StepState::Done, "0.5.0".into()));
    app.on_job("one-s", Event::Exit { code: Some(0), error: None });
    save("six-tools", &mut app, 130, 30);
    app.hosts[1].installed.remove("Grok");
    app.hosts[1].installed.remove("Pi");
    app.on_job("one-s", Event::Exit { code: Some(0), error: None });
    app.size = (60, 24);
    save("narrow", &mut app, 60, 24);
    // Toasts: stacked, then spread out under the pointer.
    app.size = (100, 30);
    for host in ["depressed-louis", "mdr", "one-s"] {
        app.toast(super::app::Sev::Info, "Done", format!("{host} updated in 2s"));
    }
    save("toasts", &mut app, 100, 30);
    let stack = app.hits.iter().find(|(_, h)| *h == super::app::Hit::Toasts).expect("toast stack").0;
    app.on_mouse(crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Moved,
        column: stack.x + 2,
        row: stack.y,
        modifiers: crossterm::event::KeyModifiers::NONE,
    });
    save("toasts-open", &mut app, 100, 30);
    app.open_actions();
    save("narrow-menu", &mut app, 60, 24);
    app.modal = None;
}

#[test]
fn snapshot_changes() {
    use crate::changelog::Release;
    let mut app = demo();
    app.latest.insert("OpenCode".into(), "1.19.0".into());
    let t3 = ("T3".to_string(), "0.0.46-nightly.20261003.2623".to_string(), "0.0.46-nightly.20261003.2648".to_string());
    let codex = ("Codex".to_string(), "0.160.0".to_string(), "0.161.0".to_string());
    let release = |tag: &str, date: &str, notes: &[&str]| Release {
        tag: tag.into(),
        date: date.into(),
        notes: notes.iter().map(|s| s.to_string()).collect(),
    };
    app.changelogs.insert(
        t3.clone(),
        Ok(vec![
            release("v0.0.46-nightly.20261004.2648", "2026-10-04", &["fix(web): keep workspace panels below dialogs", "feat(server): resume provider sessions after a restart, without losing queued messages when the socket drops twice in a row", "chore: bump deps"]),
            release("v0.0.46-nightly.20261003.2631", "2026-10-03", &["fix: orphaned worktrees are cleaned up on thread delete"]),
        ]),
    );
    app.changelogs.insert(codex.clone(), Err("GitHub rate limit exceeded".into()));
    app.modal = Some(Modal::Changes(super::app::Changes { host: "one-s".into(), items: vec![t3, codex], scroll: 0 }));
    save("changes", &mut app, 100, 30);
}
