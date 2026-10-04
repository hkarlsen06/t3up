//! The servers editor against a real (temporary) servers file. One test: it sets environment variables.
use std::collections::BTreeMap;
use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use t3up::model::{Event, Mode};
use t3up::tui::app::{App, Effect, Hit, Modal, Sev};
use t3up::tui::{logos::Logos, view};

fn press(app: &mut App, text: &str) {
    for c in text.chars() {
        app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
}

fn key(app: &mut App, code: KeyCode) {
    app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn click(app: &mut App, x: u16, y: u16) {
    app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    });
}

/// Draw, so the app knows where things are.
fn draw(app: &mut App) {
    view::render(app, &Logos::halfblocks(), &mut Buffer::empty(Rect::new(0, 0, 100, 30)));
}

fn click_hit(app: &mut App, hit: Hit) {
    draw(app);
    let r = app.hits.iter().find(|(_, h)| *h == hit).unwrap_or_else(|| panic!("no {hit:?}")).0;
    click(app, r.x + r.width / 2, r.y);
}

fn servers_modal(app: &App) -> &t3up::tui::app::Servers {
    match &app.modal {
        Some(Modal::Servers(s)) => s,
        other => panic!("editor not open: {other:?}"),
    }
}

fn start_jobs(app: &mut App) -> Vec<(String, Mode)> {
    app.take_effects()
        .into_iter()
        .filter_map(|e| match e {
            Effect::StartJob(j) => Some((j.host, j.mode)),
            _ => None,
        })
        .collect()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

#[test]
fn servers_editor() {
    let dir = std::env::temp_dir().join(format!("t3up-editor-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".ssh")).unwrap();
    std::fs::write(dir.join(".ssh/config"), "Host alpha beta *\n  User me\nHost gamma\n").unwrap();
    let servers = dir.join("config/servers");
    // SAFETY: the only test in this binary, so nothing else reads the environment meanwhile.
    unsafe {
        std::env::set_var("HOME", &dir);
        std::env::set_var("T3UP_SERVERS_FILE", &servers);
    }
    let logs = dir.join("state/logs");

    // No servers: the editor opens by itself, with suggestions from ~/.ssh/config.
    let mut app = App::new(vec![], logs.clone(), BTreeMap::new());
    assert_eq!(servers_modal(&app).suggestions(), ["alpha", "beta", "gamma"]);
    // Bad hosts are refused and nothing is written.
    press(&mut app, "bad;host");
    key(&mut app, KeyCode::Enter);
    assert!(!servers.exists());
    assert_eq!(app.toasts.last().unwrap().sev, Sev::Error);
    // Tab pulls a suggestion into the input; enter adds it. The new file explains itself.
    key(&mut app, KeyCode::Esc);
    press(&mut app, "e");
    key(&mut app, KeyCode::Tab);
    assert_eq!(servers_modal(&app).input.value, "beta");
    key(&mut app, KeyCode::Enter);
    assert_eq!(read(&servers), "# t3up servers: one SSH alias or user@host per line.\nbeta\n");
    assert_eq!(servers_modal(&app).hosts, ["beta"]);
    // A chip adds its host straight away.
    click_hit(&mut app, Hit::Suggest(0));
    assert_eq!(servers_modal(&app).hosts, ["beta", "alpha"]);
    // Closing applies the list and checks the new servers.
    key(&mut app, KeyCode::Esc);
    assert_eq!(app.hosts.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["beta", "alpha"]);
    assert_eq!(start_jobs(&mut app), vec![("beta".into(), Mode::Check), ("alpha".into(), Mode::Check)]);

    // Comments survive, kept servers keep their state, removed ones go, new ones get checked.
    let text = "# test\n\nfirst # unavailable\nsecond";
    std::fs::write(&servers, text).unwrap();
    let mut app = App::new(vec!["first".into(), "second".into(), "third".into()], logs, BTreeMap::new());
    app.take_effects();
    app.on_job("second", Event::Sys("kept".into()));
    app.on_job("second", Event::Complete);
    app.on_job("second", Event::Exit { code: Some(0), error: None });
    app.on_job("third", Event::Exit { code: Some(1), error: None });
    app.toasts.clear();
    press(&mut app, "e");
    press(&mut app, "second");
    key(&mut app, KeyCode::Enter);
    assert_eq!(app.toasts.last().unwrap().text, "second is already in the list");
    assert_eq!(read(&servers), text, "a duplicate is not written");
    for _ in 0..6 {
        key(&mut app, KeyCode::Backspace);
    }
    press(&mut app, "fourth");
    key(&mut app, KeyCode::Enter);
    click_hit(&mut app, Hit::Remove(0));
    assert_eq!(read(&servers), "# test\n\nsecond\nfourth\n");
    assert_eq!(app.hosts.len(), 3, "applied on closing, not before");
    draw(&mut app);
    click(&mut app, 0, 0); // outside the panel
    assert!(app.modal.is_none());
    assert_eq!(app.hosts.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["second", "fourth"]);
    assert_eq!(app.hosts[0].sys, "kept");
    assert_eq!(app.hosts[0].status, t3up::model::Status::Ok);
    assert_eq!(app.selected, 0);
    assert_eq!(start_jobs(&mut app), vec![("fourth".into(), Mode::Check)]);

    // Select a row with the arrows, remove it with delete.
    press(&mut app, "e");
    key(&mut app, KeyCode::Up);
    key(&mut app, KeyCode::Up);
    key(&mut app, KeyCode::Delete);
    assert_eq!(read(&servers), "# test\n\nfourth\n");
    key(&mut app, KeyCode::Esc);
    assert_eq!(app.hosts.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["fourth"]);

    // Removing the last one leaves an empty dashboard that does not crash.
    press(&mut app, "e");
    click_hit(&mut app, Hit::Remove(0));
    key(&mut app, KeyCode::Esc);
    assert!(app.hosts.is_empty());
    draw(&mut app);
    // A file edited into an invalid state must never be replaced by a fabricated list.
    let text = "# keep this comment\nfirst\nbad;host\n";
    std::fs::write(&servers, text).unwrap();
    app.open_servers();
    app.on_paste("fourth");
    key(&mut app, KeyCode::Enter);
    assert_eq!(read(&servers), text, "an editor read error must not overwrite the original file");
    assert!(app.toasts.iter().any(|t| t.sev == Sev::Error && t.text.contains("invalid server")));
    key(&mut app, KeyCode::Esc);
    // Non-NotFound read errors also refuse to open the editor.
    std::fs::write(&servers, [0xff]).unwrap();
    app.open_servers();
    assert!(app.modal.is_none());
    assert_eq!(app.toasts.last().unwrap().sev, Sev::Error);
    assert_eq!(std::fs::read(&servers).unwrap(), [0xff]);
    let _ = std::fs::remove_dir_all(&dir);
}
