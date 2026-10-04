//! The full-screen dashboard: terminal setup, the event loop, and the things `App` asks for.
pub mod app;
pub mod input;
pub mod logos;
pub mod motion;
#[cfg(test)]
mod snap;
pub mod theme;
pub mod view;

use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event as Input,
};
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::{cursor, execute, queue};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui_image::picker::Picker;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval};

use crate::model::Event;
use crate::{changelog, config, desktop, job, registry};
use app::{App, Effect, Res};
use logos::Logos;

type Term = Terminal<CrosstermBackend<Stdout>>;

fn enter() -> io::Result<()> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste, cursor::Hide)
}

fn mouse(on: bool) -> io::Result<()> {
    if on { execute!(io::stdout(), EnableMouseCapture) } else { execute!(io::stdout(), DisableMouseCapture) }
}

/// Give the terminal back as it was. Safe to call twice.
fn leave() {
    let _ = mouse(false);
    let _ = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen, cursor::Show);
    let _ = disable_raw_mode();
}

/// Restores the terminal on every way out of `run`.
struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        leave();
    }
}

/// A panic on the main thread must not leave the terminal raw. One anywhere else (a worker, a
/// blocking task) must not scribble over the screen: it goes to a file.
fn panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() == Some("main") {
            leave();
            default(info);
        } else {
            let _ = std::fs::create_dir_all(config::state_dir());
            if let Ok(mut f) =
                std::fs::OpenOptions::new().create(true).append(true).open(config::state_dir().join("panic.log"))
            {
                let _ = writeln!(f, "{info}");
            }
        }
    }));
}

/// Draw a frame. The logos go first in the same synchronized update when `prime`: a graphics
/// protocol sends an image's data with the first cell drawn, which the real frame then overwrites.
fn present(term: &mut Term, app: &mut App, logos: &Logos, prime: bool) -> io::Result<()> {
    queue!(term.backend_mut(), BeginSynchronizedUpdate)?;
    if prime {
        term.draw(|f| logos.prime(f.buffer_mut()))?;
    }
    term.draw(|f| view::draw(f, app, logos))?;
    execute!(term.backend_mut(), EndSynchronizedUpdate)?;
    app.dirty = false;
    Ok(())
}

/// Hand the real terminal to a command; jobs keep running meanwhile and their events wait in the channel.
async fn handoff(
    term: &mut Term,
    picker: &Picker,
    logos: &mut Logos,
    app: &mut App,
    command: &[String],
    banner: &str,
    signals: &mut Signals,
) -> io::Result<bool> {
    leave();
    if !banner.is_empty() {
        println!("\n  {banner}\n");
    }
    let Some((program, args)) = command.split_first() else { return Ok(false) };
    let result = match tokio::process::Command::new(program).args(args).kill_on_drop(true).spawn() {
        Ok(mut child) => tokio::select! {
            result = child.wait() => result,
            _ = signals.next() => {
                child.kill().await?; // kills and reaps the handoff child
                return Ok(true);
            }
        },
        Err(e) => Err(e),
    };
    if let Err(e) = result {
        eprintln!("t3up: {program}: {e}");
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(2)) => {},
            _ = signals.next() => return Ok(true),
        }
    }
    enter()?;
    mouse(true)?;
    // The alternate screen starts empty and forgets images: draw everything again, logos included.
    *logos = Logos::new(picker);
    // Not `clear()`: it asks the terminal where the cursor is and waits for the answer.
    let size = term.size()?;
    term.resize(Rect::new(0, 0, size.width, size.height))?;
    app.on_resize(size.width, size.height);
    present(term, app, logos, true)?;
    Ok(false)
}

/// Run a blocking function and send what `done` makes of its result (None if it panicked).
fn blocking<T: Send + 'static>(
    tx: &UnboundedSender<Res>,
    work: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(Option<T>) -> Res + Send + 'static,
) {
    let tx = tx.clone();
    tokio::spawn(async move {
        let out = tokio::task::spawn_blocking(work).await.ok();
        let _ = tx.send(done(out));
    });
}

fn run_effect(
    effect: Effect,
    jobs: &mut Vec<JoinHandle<()>>,
    job_tx: &UnboundedSender<(String, Event)>,
    res_tx: &UnboundedSender<Res>,
    logs: &Path,
) {
    match effect {
        Effect::StartJob(j) => {
            jobs.retain(|h| !h.is_finished());
            jobs.push(tokio::spawn(job::run_job(j, job_tx.clone())));
        }
        Effect::FetchLatest => blocking(res_tx, registry::fetch_latest, |l| Res::Latest(l.unwrap_or_default())),
        Effect::RefreshDesktop => blocking(res_tx, desktop::desktop_version, |v| Res::Desktop(v.unwrap_or_default())),
        Effect::FetchChangelog { component, from, to } => {
            let key = (component.clone(), from.clone(), to.clone());
            blocking(
                res_tx,
                move || changelog::changelog(&component, &from, &to),
                move |r| Res::Changelog { key, result: r.unwrap_or_else(|| Err("internal error".into())) },
            );
        }
        Effect::UpdateDesktop => {
            let say = res_tx.clone();
            blocking(
                res_tx,
                move || desktop::update_desktop(&|text| drop(say.send(Res::DesktopSay(text.to_string())))),
                |r| Res::DesktopDone(r.unwrap_or_else(|| Err("internal error".into()))),
            );
        }
        Effect::SaveTools(tools) => config::save_known_tools(logs.parent().unwrap_or(logs), &tools),
        Effect::Terminal { .. } | Effect::Quit => {}
    }
}

/// Open the dashboard on these servers; returns when the user quits.
pub async fn run(hosts: Vec<String>, logs: PathBuf) -> anyhow::Result<()> {
    panic_hook();
    let known = config::known_tools(logs.parent().unwrap_or(&logs));
    enter()?;
    let _restore = Restore;
    // Ask the terminal what graphics it has, before reading any input.
    let picker = if std::env::var_os("T3UP_NO_IMAGES").is_some() {
        Picker::halfblocks()
    } else {
        Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks())
    };
    enable_raw_mode()?; // the query turns raw mode off when it is done
    mouse(true)?;
    let mut logos = Logos::new(&picker);
    let mut term: Term = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let (job_tx, mut job_rx) = unbounded_channel::<(String, Event)>();
    let (res_tx, mut res_rx) = unbounded_channel::<Res>();
    let mut jobs: Vec<JoinHandle<()>> = vec![];
    let mut app = App::new(hosts, logs.clone(), known);
    app.motion = motion::Motion::new(std::env::var_os("T3UP_NO_MOTION").is_none());
    app.desktop = tokio::task::spawn_blocking(desktop::desktop_version).await.unwrap_or_default();
    let size = term.size()?;
    app.on_resize(size.width, size.height);
    present(&mut term, &mut app, &logos, true)?;

    let mut tick = interval(Duration::from_millis(25));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut signals = Signals::new();
    let mut frame = 0u128;
    'main: loop {
        tokio::select! {
            Some((host, event)) = job_rx.recv() => {
                app.on_job(&host, event);
                while let Ok((host, event)) = job_rx.try_recv() {
                    app.on_job(&host, event);
                }
            }
            Some(res) = res_rx.recv() => app.on_result(res),
            _ = signals.next() => break,
            _ = tick.tick() => {
                while event::poll(Duration::ZERO)? {
                    match event::read()? {
                        Input::Key(k) => app.on_key(k),
                        Input::Mouse(m) => app.on_mouse(m),
                        Input::Paste(text) => app.on_paste(&text),
                        Input::Resize(w, h) => app.on_resize(w, h),
                        _ => {}
                    }
                }
                app.tick();
                let step = if app.motion.busy() || app.hosts.iter().any(|h| h.running()) && app.motion.on {
                    33
                } else if app.motion.on {
                    66
                } else {
                    100
                };
                let now = app.clock().as_millis() / step;
                if app.animating() && now != frame {
                    app.dirty = true;
                }
                frame = now;
            }
        }
        for effect in app.take_effects() {
            match effect {
                Effect::Quit => break 'main,
                Effect::Terminal { command, banner } => {
                    if handoff(&mut term, &picker, &mut logos, &mut app, &command, &banner, &mut signals).await? {
                        break 'main;
                    }
                }
                other => run_effect(other, &mut jobs, &job_tx, &res_tx, &logs),
            }
        }
        if app.dirty {
            present(&mut term, &mut app, &logos, false)?;
        }
    }
    for job in &jobs {
        job.abort();
    }
    Ok(())
}

/// SIGTERM and SIGHUP end the dashboard like q does, with the terminal restored.
struct Signals {
    #[cfg(unix)]
    streams: [tokio::signal::unix::Signal; 2],
}

impl Signals {
    fn new() -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let make = |kind| signal(kind).expect("signal handler");
            Signals { streams: [make(SignalKind::terminate()), make(SignalKind::hangup())] }
        }
        #[cfg(not(unix))]
        Signals {}
    }

    async fn next(&mut self) {
        #[cfg(unix)]
        {
            let [a, b] = &mut self.streams;
            tokio::select! { _ = a.recv() => {}, _ = b.recv() => {} }
        }
        #[cfg(not(unix))]
        std::future::pending::<()>().await
    }
}
