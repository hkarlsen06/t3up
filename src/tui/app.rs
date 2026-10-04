//! The dashboard's state machine: no terminal, no I/O. Keys, mouse, job events and background
//! results go in; state changes and `Effect`s (things for `tui::run` to do) come out.
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::flow::{self, Flow, Kind};
use super::input::Input;
use super::motion::Motion;
use super::theme::*;
use super::view::{self, status};
use crate::changelog::{self, Release};
use crate::config;
use crate::job::{self, Job};
use crate::model::{
    COMPONENTS, Event, HOST_RE, Host, Mode, Status, StepState, VERSION_RE, compact, desktop_outdated, login_command,
    no_t3, remote_script, short, updates,
};

/// Something for `tui::run` to do.
#[derive(Debug, PartialEq)]
pub enum Effect {
    StartJob(Job),
    /// Hand the real terminal to a command, then come back.
    Terminal {
        command: Vec<String>,
        banner: String,
    },
    FetchLatest,
    RefreshDesktop,
    FetchChangelog {
        component: String,
        from: String,
        to: String,
    },
    UpdateDesktop,
    SaveTools(BTreeMap<String, Vec<String>>),
    /// Run a sign-in or pairing on a server (`ssh -tt`), its output coming back as `Res::FlowOut`.
    StartFlow {
        id: u64,
        host: String,
        command: String,
    },
    /// Type into a running flow.
    FlowInput {
        id: u64,
        text: String,
    },
    StopFlow(u64),
    /// Put text on this machine's clipboard.
    Copy(String),
    /// Open a link in this machine's browser.
    Open(String),
    /// Stop every job and leave.
    Quit,
}

/// A background task's answer.
#[derive(Debug)]
pub enum Res {
    Latest(HashMap<String, String>),
    Desktop(String),
    DesktopSay(String),
    DesktopDone(Result<String, String>),
    Changelog { key: ChangeKey, result: Result<Vec<Release>, String> },
    FlowOut { id: u64, text: String },
    FlowExit { id: u64, code: Option<i32> },
}

/// (component, from, to): what a changelog was fetched for.
pub type ChangeKey = (String, String, String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sev {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub title: String,
    pub text: String,
    pub sev: Sev,
    pub born: Instant,
    pub until: Instant,
}

/// What a click or hover lands on, registered by the renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Card(usize),
    UpdateAll,
    Output,
    /// A row of the open menu, palette or confirm dialog.
    Row(usize),
    Remove(usize),
    Add,
    Suggest(usize),
    /// A button in a sign-in or pairing window.
    Act(Act),
}

/// What a sign-in or pairing window's buttons do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Open,
    CopyLink,
    CopyCode,
    Submit,
    Retry,
    Terminal,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Choice {
    Update(String),
    Check,
    Logs,
    Terminal,
    Pair,
    SignIn(String),
    Changes,
    Desktop,
    DesktopGo,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Section(String),
    Row { choice: Choice, icon: &'static str, label: String, detail: String },
}

#[derive(Debug, Clone)]
pub struct Menu {
    pub heading: Line<'static>,
    pub items: Vec<Item>,
    pub cursor: usize,
    /// The servers a choice applies to; `all` if that is every server (updating T3 on all also updates the desktop app).
    pub hosts: Vec<String>,
    pub all: bool,
}

impl Menu {
    fn new(heading: Line<'static>, items: Vec<Item>, hosts: Vec<String>, all: bool) -> Self {
        let cursor = items.iter().position(|i| matches!(i, Item::Row { .. })).unwrap_or(0);
        Menu { heading, items, cursor, hosts, all }
    }

    fn step(&mut self, by: isize) {
        let n = self.items.len() as isize;
        let mut at = self.cursor as isize;
        loop {
            at += by;
            if !(0..n).contains(&at) {
                return;
            }
            if matches!(self.items[at as usize], Item::Row { .. }) {
                self.cursor = at as usize;
                return;
            }
        }
    }

    fn choice(&self) -> Option<Choice> {
        match self.items.get(self.cursor)? {
            Item::Row { choice, .. } => Some(choice.clone()),
            Item::Section(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    Refresh,
    Update(String),
    Version,
    Servers,
    Desktop,
    Ssh(String),
    Pair(String),
    SignIn(String, String),
    Changes(String),
    Output,
}

#[derive(Debug, Clone)]
pub struct Palette {
    pub input: Input,
    pub items: Vec<(String, Cmd)>,
    pub cursor: usize,
}

fn subsequence(needle: &str, hay: &str) -> bool {
    let mut hay = hay.chars().flat_map(char::to_lowercase);
    needle.chars().flat_map(char::to_lowercase).filter(|c| !c.is_whitespace()).all(|n| hay.any(|h| h == n))
}

impl Palette {
    /// Indices into `items` that match what is typed.
    pub fn matches(&self) -> Vec<usize> {
        (0..self.items.len()).filter(|&i| subsequence(&self.input.value, &self.items[i].0)).collect()
    }
}

#[derive(Debug, Clone)]
pub struct Servers {
    pub text: String,
    pub hosts: Vec<String>,
    pub input: Input,
    /// Hosts from ~/.ssh/config.
    pub ssh: Vec<String>,
    /// The highlighted server in the list; None while typing.
    pub row: Option<usize>,
    pub tab: usize,
}

impl Servers {
    pub fn suggestions(&self) -> Vec<&String> {
        self.ssh.iter().filter(|h| !self.hosts.contains(h)).collect()
    }
}

#[derive(Debug, Clone)]
pub struct Confirm {
    pub names: Vec<String>,
    pub only: String,
    pub desktop: bool,
    /// (host, agents live)
    pub busy: Vec<(String, u32)>,
    /// 0 = Update anyway, 1 = Cancel.
    pub cursor: usize,
}

#[derive(Debug, Clone)]
pub struct Changes {
    pub host: String,
    pub items: Vec<ChangeKey>,
    pub scroll: usize,
}

#[derive(Debug, Clone)]
pub enum Modal {
    Menu(Menu),
    Version(Input),
    Servers(Servers),
    Confirm(Confirm),
    Changes(Changes),
    Help { scroll: usize },
    Palette(Palette),
    Flow(Flow),
}

/// An update of several servers starts with the first alone; the rest wait for it.
#[derive(Debug, Clone)]
struct Canary {
    host: String,
    started: Instant,
    version: String,
    only: String,
}

/// Where the mouse rests: a tooltip shows once it has been still for a moment.
#[derive(Debug, Clone)]
pub struct Hover {
    pub pos: (u16, u16),
    pub hit: Option<Hit>,
    header: bool,
    since: Instant,
    pub shown: bool,
}

pub struct App {
    pub hosts: Vec<Host>,
    pub selected: usize,
    /// Newest release per tool, refreshed with every check.
    pub latest: HashMap<String, String>,
    /// The T3 version updates install; '' = latest nightly.
    pub target: String,
    pub desktop: String,
    pub desktop_updating: bool,
    /// Tools found on any server: each card's slots, in order.
    pub tools: Vec<&'static str>,
    pub show_output: bool,
    /// Output lines scrolled up from the bottom; 0 follows new lines.
    pub out_scroll: usize,
    /// Rows the card grid is scrolled down.
    pub scroll: u16,
    pub modal: Option<Modal>,
    pub toasts: Vec<Toast>,
    /// Servers waiting for the canary.
    pub queued: HashSet<String>,
    canary: Option<Canary>,
    pub changelogs: HashMap<ChangeKey, Result<Vec<Release>, String>>,
    pub size: (u16, u16),
    pub started: Instant,
    pub local: String,
    quit_armed: Option<Instant>,
    logs: PathBuf,
    script: Arc<str>,
    known: BTreeMap<String, Vec<String>>,
    fx: Vec<Effect>,
    pub dirty: bool,
    /// Where the last draw put clickable things, topmost last.
    pub hits: Vec<(Rect, Hit)>,
    pub modal_rect: Rect,
    pub hover: Option<Hover>,
    pub motion: Motion,
    /// Sign-ins waiting for the window: (host, tool).
    flow_queue: VecDeque<(String, String)>,
    next_flow: u64,
}

impl App {
    /// The dashboard on these servers; `known` is what tools.json remembers. Checks every
    /// server at once (read-only); with no servers, opens the editor.
    pub fn new(names: Vec<String>, logs: PathBuf, known: BTreeMap<String, Vec<String>>) -> Self {
        let mut app = App {
            hosts: vec![],
            selected: 0,
            latest: HashMap::new(),
            target: String::new(),
            desktop: String::new(),
            desktop_updating: false,
            tools: vec!["T3"],
            show_output: false,
            out_scroll: 0,
            scroll: 0,
            modal: None,
            toasts: vec![],
            queued: HashSet::new(),
            canary: None,
            changelogs: HashMap::new(),
            size: (100, 30),
            started: Instant::now(),
            local: config::local_name(),
            quit_armed: None,
            logs,
            script: remote_script().into(),
            known,
            fx: vec![],
            dirty: true,
            hits: vec![],
            modal_rect: Rect::default(),
            hover: None,
            motion: Motion::new(false),
            flow_queue: VecDeque::new(),
            next_flow: 0,
        };
        app.set_hosts(names);
        if app.hosts.is_empty() {
            app.open_servers();
        }
        app
    }

    fn invalidate_hits(&mut self) {
        self.hits.clear();
        self.modal_rect = Rect::default();
    }

    fn open_modal(&mut self, modal: Modal) {
        self.invalidate_hits();
        self.modal = Some(modal);
    }

    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.fx)
    }

    pub fn clock(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn host(&self) -> Option<&Host> {
        self.hosts.get(self.selected)
    }

    /// Whether something on screen moves by itself (spinners, elapsed time) and needs redraws.
    pub fn animating(&self) -> bool {
        self.motion.on
            || matches!(self.modal, Some(Modal::Flow(_)))
            || self.hosts.iter().any(Host::running)
            || self.desktop_updating
            || matches!(&self.modal, Some(Modal::Changes(c)) if c.items.iter().any(|k| !self.changelogs.contains_key(k)))
    }

    pub fn toast(&mut self, sev: Sev, title: &str, text: impl Into<String>) {
        let secs = if sev == Sev::Error { 8 } else { 5 };
        self.toast_for(sev, title, text, secs);
    }

    pub fn toast_for(&mut self, sev: Sev, title: &str, text: impl Into<String>, secs: u64) {
        let until = Instant::now() + Duration::from_secs(secs);
        self.toasts.push(Toast { title: title.into(), text: text.into(), sev, born: Instant::now(), until });
        self.toasts.drain(..self.toasts.len().saturating_sub(4));
        self.dirty = true;
    }

    /// Expire toasts. Call on every tick.
    pub fn tick(&mut self) {
        let before = self.toasts.len();
        let now = Instant::now();
        self.toasts.retain(|t| t.until > now);
        self.motion.prune();
        self.dirty |= self.toasts.len() != before;
        if let Some(Modal::Flow(f)) = &self.modal
            && f.kind != Kind::Pair
            && f.state == flow::State::Done
            && f.finished.is_some_and(|t| t.elapsed() > Duration::from_millis(1400))
        {
            self.close_modal();
        }
        if self.modal.is_none()
            && let Some((host, tool)) = self.flow_queue.pop_front()
        {
            self.start_flow(&host, &tool);
        }
        if let Some(h) = self.hover.as_mut().filter(|h| !h.shown && h.since.elapsed() > Duration::from_millis(600)) {
            h.shown = matches!(h.hit, Some(Hit::Card(_))) || (h.header && h.hit.is_none());
            self.dirty |= h.shown;
        }
    }

    // ── servers ────────────────────────────────────────────────────────────

    /// Show these servers, keeping the state of ones already shown; check the new ones.
    pub fn set_hosts(&mut self, names: Vec<String>) {
        let keep = self.host().map(|h| h.name.clone());
        let mut old: HashMap<String, Host> = self.hosts.drain(..).map(|h| (h.name.clone(), h)).collect();
        let mut fresh = vec![];
        for name in &names {
            let host = old.remove(name).unwrap_or_else(|| {
                let mut h = Host::new(name);
                // Start with last run's slots, so cards don't widen as tools report in.
                for tool in self.known.get(name).into_iter().flatten() {
                    if let Some((c, _)) = COMPONENTS.iter().find(|(c, _)| c == tool) {
                        h.installed.insert(c.to_string());
                    }
                }
                fresh.push(name.clone());
                h
            });
            self.hosts.push(host);
        }
        if self.canary.as_ref().is_some_and(|c| !names.contains(&c.host)) {
            self.canary = None;
            self.queued.clear();
            self.toast(
                Sev::Warning,
                "Rollout cancelled",
                "The canary server was removed. The other servers were not updated.",
            );
        }
        self.queued.retain(|q| names.contains(q));
        self.selected = keep.and_then(|k| names.iter().position(|n| *n == k)).unwrap_or(0);
        self.fit();
        self.ensure_visible();
        self.start(fresh, Mode::Check, "all");
    }

    /// A slot per tool any server has, in order.
    fn fit(&mut self) {
        let tools: Vec<&'static str> = COMPONENTS
            .iter()
            .map(|(n, _)| *n)
            .filter(|n| self.hosts.iter().any(|h| h.installed.contains(*n)))
            .collect();
        self.tools = if tools.is_empty() { vec!["T3"] } else { tools };
    }

    pub fn ensure_visible(&mut self) {
        self.scroll = view::scroll_to(self, self.selected);
    }

    // ── jobs ───────────────────────────────────────────────────────────────

    /// Run `mode` on these servers (by name), skipping ones already running.
    pub fn start(&mut self, names: Vec<String>, mode: Mode, only: &str) {
        self.start_version(names, mode, only, &self.target.clone());
    }

    fn start_version(&mut self, names: Vec<String>, mode: Mode, only: &str, version: &str) {
        if names.is_empty() {
            return;
        }
        let running: Vec<&str> = names
            .iter()
            .filter(|n| self.hosts.iter().any(|h| &h.name == *n && h.running()))
            .map(String::as_str)
            .collect();
        if !running.is_empty() {
            let text = format!("Already running: {}", running.join(", "));
            self.toast(Sev::Warning, "Warning", text);
        }
        self.fx.push(Effect::RefreshDesktop);
        self.fx.push(Effect::FetchLatest);
        for name in names {
            self.queued.remove(&name);
            let Some(h) = self.hosts.iter_mut().find(|h| h.name == name) else { continue };
            if h.running() {
                continue;
            }
            h.reset(mode, only);
            self.fx.push(Effect::StartJob(Job {
                host: name,
                mode,
                version: version.to_string(),
                only: only.to_string(),
                logs: self.logs.clone(),
                script: self.script.clone(),
            }));
        }
        self.out_scroll = 0;
        self.dirty = true;
    }

    /// Update `names` (by name). If it would restart T3 on a server with agents running, ask first.
    pub fn begin_update(&mut self, names: Vec<String>, only: &str, desktop: bool) {
        let touches_t3 = only.split(',').any(|o| o == "all" || o == "t3");
        let busy: Vec<(String, u32)> = self
            .hosts
            .iter()
            .filter(|h| names.contains(&h.name))
            .filter_map(|h| h.busy.filter(|&n| n > 0).map(|n| (h.name.clone(), n)))
            .collect();
        if touches_t3 && !busy.is_empty() {
            self.open_modal(Modal::Confirm(Confirm { names, only: only.into(), desktop, busy, cursor: 1 }));
            return;
        }
        self.run_update(names, only, desktop);
    }

    fn run_update(&mut self, mut names: Vec<String>, only: &str, desktop: bool) {
        if names.len() > 1 && self.is_running(&names[0]) {
            self.toast(
                Sev::Warning,
                "Warning",
                format!("Already running: {}. Wait before starting a rollout.", names[0]),
            );
            return;
        }
        if desktop && !self.desktop_updating && !self.outdated_desktop().is_empty() {
            self.desktop_updating = true;
            self.fx.push(Effect::UpdateDesktop);
        }
        if names.len() > 1 {
            // Canary: the first server alone; the rest start when it comes back healthy.
            let first = names.remove(0);
            self.start(vec![first.clone()], Mode::Update, only);
            if let Some(h) = self.hosts.iter().find(|h| h.name == first && h.running() && h.mode == Mode::Update) {
                self.canary =
                    Some(Canary { host: first, started: h.started, version: self.target.clone(), only: only.into() });
                self.queued = names.into_iter().filter(|n| !self.is_running(n)).collect();
            }
        } else {
            self.start(names, Mode::Update, only);
        }
    }

    fn is_running(&self, name: &str) -> bool {
        self.hosts.iter().any(|h| h.name == name && h.running())
    }

    /// A newer T3 than this machine's desktop app: its version, or ''.
    pub fn outdated_desktop(&self) -> String {
        let hosts: Vec<&Host> = self.hosts.iter().collect();
        desktop_outdated(&self.desktop, &hosts, &self.latest, &self.target)
    }

    /// Fold in a job's event.
    pub fn on_job(&mut self, host: &str, event: Event) {
        let Some(i) = self.hosts.iter().position(|h| h.name == host) else { return }; // removed mid-run
        let exit = matches!(event, Event::Exit { .. });
        if let Event::Done(detail) = &event {
            let tool = detail.split(':').next().unwrap_or("");
            self.motion.arrive(host, tool);
        }
        let added = self.hosts[i].apply(event).is_some();
        if added && i == self.selected && self.out_scroll > 0 {
            self.out_scroll += 1;
        }
        self.dirty = true;
        if exit {
            self.finished(i);
        }
    }

    fn finished(&mut self, i: usize) {
        self.fit();
        let h = self.hosts[i].clone();
        let installed: Vec<String> = h.installed.iter().cloned().collect();
        if self.known.get(&h.name) != Some(&installed) {
            self.known.insert(h.name.clone(), installed);
            self.fx.push(Effect::SaveTools(self.known.clone()));
        }
        let canary = self.canary.take_if(|c| c.host == h.name && c.started == h.started && h.mode == Mode::Update);
        let failed = h.status == Status::Failed;
        self.motion.pulse(&h.name, if failed { RED } else { GREEN });
        if let Some(c) = canary {
            if failed {
                self.queued.clear();
                let text = format!("Canary {} failed: {}. The other servers were not updated.", h.name, h.error);
                self.toast(Sev::Error, "Failed", text);
            } else {
                let rest: Vec<String> =
                    self.hosts.iter().map(|x| x.name.clone()).filter(|n| self.queued.contains(n)).collect();
                self.queued.clear();
                self.start_version(rest, Mode::Update, &c.only, &c.version);
            }
        } else if failed {
            self.toast(Sev::Error, "Failed", format!("{}: {}", h.name, h.error));
        }
        if !failed && h.mode == Mode::Update {
            self.toast(Sev::Info, "Done", format!("{} updated in {:.0}s", h.name, h.elapsed.as_secs_f64()));
        }
        if !h.rollback.is_empty() {
            self.toast(Sev::Warning, "Rolled back", format!("{}: T3 {}", h.name, h.rollback));
        }
        // An update that left a tool it was meant to touch signed out: sign in (each checks again when done).
        if h.mode == Mode::Update {
            for name in h.auth.iter().filter(|n| h.picked(n) && login_command(n).is_some()) {
                self.start_flow(&h.name, name);
            }
        }
    }

    /// A background task finished.
    pub fn on_result(&mut self, res: Res) {
        self.dirty = true;
        match res {
            Res::Latest(latest) => {
                if !latest.is_empty() {
                    self.latest = latest;
                }
            }
            Res::Desktop(version) => self.desktop = version,
            Res::DesktopSay(text) => self.toast(Sev::Info, "Desktop", text),
            Res::DesktopDone(result) => {
                self.desktop_updating = false;
                self.fx.push(Effect::RefreshDesktop);
                match result {
                    Ok(version) => {
                        self.toast(Sev::Info, "Desktop up to date", format!("T3 Code desktop {}", short(&version)))
                    }
                    Err(e) => {
                        let text = if e.is_empty() { "Unknown error".to_string() } else { e };
                        self.toast_for(Sev::Error, "Desktop update failed", text, 10);
                    }
                }
            }
            Res::Changelog { key, result } => {
                self.changelogs.insert(key, result);
            }
            Res::FlowOut { id, text } => {
                if let Some(Modal::Flow(f)) = self.modal.as_mut()
                    && f.id == id
                {
                    f.feed(&text);
                }
            }
            Res::FlowExit { id, code } => {
                let Some(Modal::Flow(f)) = self.modal.as_mut() else { return };
                if f.id != id {
                    return;
                }
                f.exit(code);
                let (host, kind, tool, state) = (f.host.clone(), f.kind, f.tool.clone(), f.state.clone());
                if state == flow::State::Done && kind != Kind::Pair {
                    self.toast(Sev::Info, "Signed in", format!("{tool} on {host}"));
                    self.start(vec![host], Mode::Check, "all");
                }
            }
        }
    }

    // ── sign-ins and pairing ───────────────────────────────────────────────

    /// Sign in to `tool` (or pair, for 'T3') on `host` in a window of its own; queued behind an open one.
    pub fn start_flow(&mut self, host: &str, tool: &str) {
        if self.modal.is_some() {
            if !self.flow_queue.iter().any(|(h, t)| h == host && t == tool) {
                self.flow_queue.push_back((host.into(), tool.into()));
            }
            return;
        }
        self.next_flow += 1;
        let f = Flow::new(self.next_flow, host, tool);
        self.fx.push(Effect::StartFlow { id: f.id, host: host.into(), command: f.command() });
        self.open_modal(Modal::Flow(f));
    }

    /// Close the open window; a sign-in still running stops.
    fn close_modal(&mut self) {
        if let Some(Modal::Flow(f)) = self.modal.take()
            && f.state == flow::State::Running
        {
            self.fx.push(Effect::StopFlow(f.id));
        }
        self.invalidate_hits();
        self.dirty = true;
    }

    /// A button (or its key) in a sign-in or pairing window. Returns the window, or None to close it.
    fn act(&mut self, mut f: Flow, act: Act) -> Option<Modal> {
        match act {
            Act::Open => {
                if let Some(url) = &f.url {
                    self.fx.push(Effect::Open(url.clone()));
                }
            }
            Act::CopyLink => {
                if let Some(url) = &f.url {
                    self.fx.push(Effect::Copy(url.clone()));
                    self.toast(Sev::Info, "Copied", "The link is on your clipboard");
                }
            }
            Act::CopyCode => {
                if let Some(code) = &f.code {
                    self.fx.push(Effect::Copy(code.clone()));
                    let what = if f.kind == Kind::Pair { "token" } else { "code" };
                    self.toast(Sev::Info, "Copied", format!("The {what} is on your clipboard"));
                }
            }
            Act::Submit => {
                let text = f.input.value.trim().to_string();
                if f.prompt && !text.is_empty() {
                    self.fx.push(Effect::FlowInput { id: f.id, text: format!("{text}\r") });
                    f.sent += 1;
                    f.prompt = false;
                    f.input = Input::default();
                } else if f.state == flow::State::Done {
                    return None;
                }
            }
            Act::Retry => {
                if f.state == flow::State::Running {
                    self.fx.push(Effect::StopFlow(f.id));
                }
                let (host, tool) = (f.host.clone(), f.tool.clone());
                self.start_flow(&host, &tool);
                return self.modal.take();
            }
            Act::Terminal => {
                if f.state == flow::State::Running {
                    self.fx.push(Effect::StopFlow(f.id));
                }
                if let Some(login) = login_command(&f.tool) {
                    let banner = format!("{}. You come back here when it finishes.", f.title());
                    self.fx.push(Effect::Terminal {
                        command: vec![job::ssh_program(), "-t".into(), f.host.clone(), login],
                        banner,
                    });
                }
                return None;
            }
        }
        Some(Modal::Flow(f))
    }

    // ── actions ────────────────────────────────────────────────────────────

    fn names(&self) -> Vec<String> {
        self.hosts.iter().map(|h| h.name.clone()).collect()
    }

    fn select(&mut self, i: usize) {
        if i < self.hosts.len() && i != self.selected {
            self.selected = i;
            self.out_scroll = 0;
        }
        self.ensure_visible();
        self.dirty = true;
    }

    /// Spatial move: up/down jump a whole row, clamped to the first and last card.
    fn go(&mut self, dx: isize, dy: isize) {
        if self.hosts.is_empty() {
            return;
        }
        let cols = view::geometry(self).cols as isize;
        let next = (self.selected as isize + dx + dy * cols).clamp(0, self.hosts.len() as isize - 1);
        self.select(next as usize);
    }

    pub fn refresh(&mut self) {
        self.start(self.names(), Mode::Check, "all");
    }

    fn detail(&self, name: &str, hosts: &[&Host]) -> String {
        let missing = hosts.iter().filter(|h| matches!(h.steps.get(name), Some((StepState::Skip, _)))).count();
        if name == "T3" {
            let to = if self.target.is_empty() { "latest nightly" } else { &self.target };
            return match missing {
                0 => format!("→ {to}"),
                n if n == hosts.len() => format!("not installed · install {to}"),
                n => format!("→ {to} · installs on {n} missing"),
            };
        }
        match missing {
            0 => "→ latest".into(),
            n if n == hosts.len() => "not installed · install latest".into(),
            n => format!("→ latest · installs on {n} missing"),
        }
    }

    fn update_menu(&mut self, names: Vec<String>, heading: Line<'static>, inspect: Vec<Item>, all: bool) {
        let hosts: Vec<&Host> = self.hosts.iter().filter(|h| names.contains(&h.name)).collect();
        let desktop = all && !self.outdated_desktop().is_empty();
        let row = |choice, label: &str, detail: String| Item::Row { choice, icon: "↑", label: label.into(), detail };
        let t3 = if hosts.iter().any(|h| no_t3(h)) { "installs T3, updates" } else { "T3," };
        let everything = format!("{t3} every installed provider{}", if desktop { ", desktop app" } else { "" });
        let mut items =
            vec![Item::Section("Update".into()), row(Choice::Update("all".into()), "Everything", everything)];
        // Something missing: one row sets the whole machine up (each picked by name, so it's installed).
        let missing = hosts.iter().any(|h| h.steps.values().any(|(state, _)| *state == StepState::Skip));
        if missing {
            let every: Vec<String> = COMPONENTS.iter().map(|(n, _)| n.to_lowercase()).collect();
            let detail = "T3 and every provider, with their own installers".to_string();
            items.push(row(Choice::Update(every.join(",")), "Install everything", detail));
        }
        for (name, label) in COMPONENTS {
            items.push(row(Choice::Update(name.to_lowercase()), label, self.detail(name, &hosts)));
        }
        if desktop {
            let detail = format!("this machine · → {}", compact(&self.outdated_desktop(), &self.desktop));
            items.push(Item::Row { choice: Choice::Desktop, icon: "↑", label: "Desktop app".into(), detail });
        }
        if !inspect.is_empty() {
            items.push(Item::Section("Inspect".into()));
            items.extend(inspect);
        }
        self.open_modal(Modal::Menu(Menu::new(heading, items, names, all)));
    }

    /// Enter: everything you can do with the selected server.
    pub fn open_actions(&mut self) {
        let Some(h) = self.host() else { return };
        let (icon, color, label) = status(h, self.queued.contains(&h.name), self.clock());
        let heading = Line::from(vec![
            Span::styled(h.name.clone(), bold(TEXT)),
            Span::raw("   "),
            Span::styled(format!("{icon} {label}"), fg(color)),
        ]);
        let row = |choice, icon, label: &str, detail: &str| Item::Row {
            choice,
            icon,
            label: label.into(),
            detail: detail.into(),
        };
        let mut inspect = vec![
            row(Choice::Check, "↻", "Check again", "versions and health, changes nothing"),
            row(Choice::Changes, "✦", "What's new", "release notes for what's waiting"),
            row(Choice::Logs, "≡", if self.show_output { "Hide output" } else { "Show output" }, "live log below"),
        ];
        // Tools its last run found signed out, right where you'd look.
        for tool in h.auth.iter().filter(|t| *t != "T3" && login_command(t).is_some()) {
            let label = format!("Sign in to {tool}");
            inspect.push(row(Choice::SignIn(tool.clone()), "→", &label, "not signed in"));
        }
        inspect.push(row(Choice::Pair, "⌁", "Create pairing link", "connect a device, via Tailscale"));
        inspect.push(row(Choice::Terminal, "›", "Terminal", "ssh in · type exit to return"));
        self.update_menu(vec![h.name.clone()], heading, inspect, false);
    }

    pub fn open_update(&mut self) {
        let Some(h) = self.host() else { return };
        let heading = Line::styled(h.name.clone(), bold(TEXT));
        self.update_menu(vec![h.name.clone()], heading, vec![], false);
    }

    pub fn open_update_all(&mut self) {
        if self.hosts.is_empty() {
            return;
        }
        let first = &self.hosts[0].name;
        let mut note = format!("   {}", self.hosts.len());
        if self.hosts.len() > 1 {
            note += &format!(" · {first} first");
        }
        let heading = Line::from(vec![Span::styled("All servers", bold(TEXT)), Span::styled(note, fg(MUTED))]);
        let inspect = vec![Item::Row {
            choice: Choice::Check,
            icon: "↻",
            label: "Check again".into(),
            detail: "versions and health, changes nothing".into(),
        }];
        self.update_menu(self.names(), heading, inspect, true);
    }

    pub fn open_desktop(&mut self) {
        if self.desktop_updating {
            self.toast(Sev::Warning, "Warning", "The desktop app is already updating");
            return;
        }
        let version = if self.desktop.is_empty() { "not detected".to_string() } else { short(&self.desktop) };
        let heading = Line::from(vec![
            Span::styled("T3 Code desktop", bold(TEXT)),
            Span::styled(format!("   {version}"), fg(MUTED)),
        ]);
        let items = vec![
            Item::Section("This machine".into()),
            Item::Row {
                choice: Choice::DesktopGo,
                icon: "↑",
                label: "Quit and update".into(),
                detail: "newest release, then reopens".into(),
            },
        ];
        self.open_modal(Modal::Menu(Menu::new(heading, items, vec![], false)));
    }

    fn update_desktop(&mut self) {
        if self.desktop_updating {
            self.toast(Sev::Warning, "Warning", "The desktop app is already updating");
        } else {
            self.desktop_updating = true;
            self.fx.push(Effect::UpdateDesktop);
        }
    }

    pub fn open_version(&mut self) {
        self.open_modal(Modal::Version(Input::new(&self.target)));
    }

    pub fn open_servers(&mut self) {
        let (text, hosts) = match config::read_servers() {
            Ok(servers) => servers,
            Err(e) => {
                self.toast(Sev::Error, "Cannot edit servers", e);
                return;
            }
        };
        let mut ssh = config::ssh_hosts();
        let mut seen = HashSet::new();
        ssh.retain(|h| seen.insert(h.clone()));
        self.open_modal(Modal::Servers(Servers { text, hosts, input: Input::default(), ssh, row: None, tab: 0 }));
    }

    pub fn toggle_output(&mut self) {
        self.show_output = !self.show_output;
        self.out_scroll = 0;
        self.ensure_visible();
    }

    pub fn open_ssh(&mut self, name: &str) {
        if let Some(i) = self.hosts.iter().position(|h| h.name == name) {
            self.select(i);
            self.fx
                .push(Effect::Terminal { command: vec![job::ssh_program(), name.to_string()], banner: String::new() });
        }
    }

    /// A pairing link for a device, made on the server in a real terminal (see `model::pair_command`).
    pub fn open_pair(&mut self, name: &str) {
        let Some(i) = self.hosts.iter().position(|h| h.name == name) else { return };
        self.select(i);
        self.start_flow(name, "T3");
    }

    /// What's new: the release notes of everything waiting on this server.
    pub fn open_changes(&mut self, name: &str) {
        let Some(h) = self.hosts.iter().find(|h| h.name == name) else { return };
        let mut items = vec![];
        for (component, new) in updates(h, &self.latest, &self.target) {
            if changelog::has_changelog(&component) {
                items.push((component.clone(), h.current[&component].clone(), new));
            }
        }
        for key in &items {
            if !self.changelogs.contains_key(key) {
                self.fx.push(Effect::FetchChangelog {
                    component: key.0.clone(),
                    from: key.1.clone(),
                    to: key.2.clone(),
                });
            }
        }
        self.open_modal(Modal::Changes(Changes { host: name.to_string(), items, scroll: 0 }));
    }

    fn open_palette(&mut self) {
        let mut items = vec![
            ("Refresh".to_string(), Cmd::Refresh),
            ("Update everything on all servers".into(), Cmd::Update("all".into())),
        ];
        for (name, _) in COMPONENTS {
            items.push((format!("Update {name} on all servers"), Cmd::Update(name.to_lowercase())));
        }
        items.push(("Set T3 version".into(), Cmd::Version));
        items.push(("Edit servers".into(), Cmd::Servers));
        items.push(("Update T3 Code desktop".into(), Cmd::Desktop));
        for h in &self.hosts {
            items.push((format!("SSH into {}", h.name), Cmd::Ssh(h.name.clone())));
            items.push((format!("Create pairing link for {}", h.name), Cmd::Pair(h.name.clone())));
            for tool in h.auth.iter().filter(|t| *t != "T3" && login_command(t).is_some()) {
                items.push((format!("Sign in to {tool} on {}", h.name), Cmd::SignIn(h.name.clone(), tool.clone())));
            }
            items.push((format!("What's new on {}", h.name), Cmd::Changes(h.name.clone())));
        }
        items.push(("Toggle output".into(), Cmd::Output));
        self.open_modal(Modal::Palette(Palette { input: Input::default(), items, cursor: 0 }));
    }

    fn run(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Refresh => self.refresh(),
            Cmd::Update(only) => {
                let all = only == "all";
                self.begin_update(self.names(), &only, all);
            }
            Cmd::Version => self.open_version(),
            Cmd::Servers => self.open_servers(),
            Cmd::Desktop => self.open_desktop(),
            Cmd::Ssh(host) => self.open_ssh(&host),
            Cmd::Pair(host) => self.open_pair(&host),
            Cmd::SignIn(host, tool) => self.start_flow(&host, &tool),
            Cmd::Changes(host) => self.open_changes(&host),
            Cmd::Output => self.toggle_output(),
        }
    }

    /// q: quit, but a first press with servers still running only warns.
    pub fn quit(&mut self) {
        let busy = self.hosts.iter().filter(|h| h.running()).count();
        if busy > 0 && self.quit_armed.is_none_or(|t| t.elapsed() > Duration::from_secs(3)) {
            self.quit_armed = Some(Instant::now());
            let text = format!("{busy} server(s) still running. Press q again to stop them and quit.");
            self.toast_for(Sev::Warning, "Warning", text, 3);
            return;
        }
        self.fx.push(Effect::Quit);
    }

    fn choose(&mut self, menu: &Menu, choice: Choice) {
        let hosts = menu.hosts.clone();
        match choice {
            Choice::Update(only) => {
                let desktop = menu.all && only == "all";
                self.begin_update(hosts, &only, desktop);
            }
            Choice::Check => self.start(hosts, Mode::Check, "all"),
            Choice::Logs => self.toggle_output(),
            Choice::Terminal => {
                if let Some(name) = hosts.first() {
                    self.open_ssh(name);
                }
            }
            Choice::Pair => {
                if let Some(name) = hosts.first() {
                    self.open_pair(name);
                }
            }
            Choice::SignIn(tool) => {
                if let Some(name) = hosts.first() {
                    self.start_flow(name, &tool);
                }
            }
            Choice::Changes => {
                if let Some(name) = hosts.first() {
                    self.open_changes(name);
                }
            }
            Choice::Desktop => self.open_desktop(),
            Choice::DesktopGo => self.update_desktop(),
        }
    }

    // ── servers editor ─────────────────────────────────────────────────────

    fn write_servers(&mut self, s: &mut Servers, text: String) {
        if let Err(e) = config::write_servers(&text) {
            self.toast(Sev::Error, "Error", format!("{}: {e}", config::servers_file().display()));
            return;
        }
        s.hosts = config::parse_servers(&text).unwrap_or_default();
        s.text = text;
        s.input = Input::default();
        s.row = s.row.filter(|&r| r < s.hosts.len());
    }

    fn add_server(&mut self, s: &mut Servers, host: &str) {
        let host = host.trim();
        if !HOST_RE.is_match(host) {
            self.toast(Sev::Error, "Error", format!("{host:?} is not an SSH host"));
        } else if s.hosts.iter().any(|h| h == host) {
            self.toast(Sev::Warning, "Warning", format!("{host} is already in the list"));
        } else {
            let text = config::add_server(&s.text, host);
            self.write_servers(s, text);
        }
    }

    fn remove_server(&mut self, s: &mut Servers, i: usize) {
        if let Some(host) = s.hosts.get(i).cloned() {
            let text = config::remove_server(&s.text, &host);
            self.write_servers(s, text);
        }
    }

    /// Closing the editor applies the list.
    fn close_servers(&mut self, s: Servers) {
        if s.hosts != self.names() {
            self.set_hosts(s.hosts);
        }
    }

    fn servers_key(&mut self, mut s: Servers, key: KeyEvent) -> Option<Modal> {
        let n = s.hosts.len();
        match key.code {
            KeyCode::Esc => {
                self.close_servers(s);
                return None;
            }
            KeyCode::Enter if s.row.is_none() => {
                let host = s.input.value.clone();
                self.add_server(&mut s, &host);
            }
            KeyCode::Up => s.row = if n == 0 { None } else { Some(s.row.map_or(n - 1, |r| r.saturating_sub(1))) },
            KeyCode::Down => s.row = s.row.and_then(|r| (r + 1 < n).then_some(r + 1)),
            KeyCode::Delete | KeyCode::Backspace if s.row.is_some() => {
                if let Some(r) = s.row {
                    self.remove_server(&mut s, r);
                }
            }
            KeyCode::Tab | KeyCode::BackTab => {
                let options: Vec<String> = s.suggestions().into_iter().cloned().collect();
                if !options.is_empty() {
                    let step = if key.code == KeyCode::Tab { 1 } else { options.len() - 1 };
                    s.tab = (s.tab + step) % options.len();
                    s.row = None;
                    s.input.set(&options[s.tab]);
                }
            }
            _ => {
                s.row = None;
                s.input.key(key);
            }
        }
        Some(Modal::Servers(s))
    }

    // ── input ──────────────────────────────────────────────────────────────

    pub fn on_paste(&mut self, text: &str) {
        self.invalidate_hits();
        self.dirty = true;
        match &mut self.modal {
            Some(Modal::Version(i)) => i.insert(text.trim()),
            Some(Modal::Servers(s)) => {
                s.row = None;
                s.input.insert(text.trim());
            }
            Some(Modal::Palette(p)) => {
                p.input.insert(text);
                p.cursor = 0;
            }
            Some(Modal::Flow(f)) if f.prompt => f.input.insert(text.trim()),
            _ => {}
        }
    }

    pub fn on_resize(&mut self, w: u16, h: u16) {
        self.invalidate_hits();
        self.size = (w, h);
        self.ensure_visible();
        self.dirty = true;
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        self.invalidate_hits();
        self.dirty = true;
        self.hover = None;
        self.motion.intro = None; // any key skips the intro, and still does what it does
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.fx.push(Effect::Quit);
            return;
        }
        if let Some(modal) = self.modal.take() {
            if let Some(kept) = self.modal_key(modal, key) {
                self.modal = Some(kept);
            }
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('p') if ctrl => self.open_palette(),
            _ if ctrl => {}
            KeyCode::Left => self.go(-1, 0),
            KeyCode::Right => self.go(1, 0),
            KeyCode::Up | KeyCode::Char('k') => self.go(0, -1),
            KeyCode::Down | KeyCode::Char('j') => self.go(0, 1),
            KeyCode::Tab => self.go(1, 0),
            KeyCode::BackTab => self.go(-1, 0),
            KeyCode::PageUp => self.scroll_by(-1, true),
            KeyCode::PageDown => self.scroll_by(1, true),
            KeyCode::Enter => self.open_actions(),
            KeyCode::Char('u') => self.open_update(),
            KeyCode::Char('a') => self.open_update_all(),
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('c') => {
                if let Some(h) = self.host() {
                    let name = h.name.clone();
                    self.open_changes(&name);
                }
            }
            KeyCode::Char('v') => self.open_version(),
            KeyCode::Char('e') => self.open_servers(),
            KeyCode::Char('s') => {
                if let Some(h) = self.host() {
                    let name = h.name.clone();
                    self.open_ssh(&name);
                }
            }
            KeyCode::Char('l') => self.toggle_output(),
            KeyCode::Char('d') => self.open_desktop(),
            KeyCode::Char('?') => self.open_modal(Modal::Help { scroll: 0 }),
            KeyCode::Char('q') => self.quit(),
            _ => {}
        }
    }

    /// Scroll the output panel if it is open, else the card grid. `page`: a screenful, else 3 lines.
    fn scroll_by(&mut self, dir: isize, page: bool) {
        let step = if page { (self.size.1 as usize / 4).max(3) } else { 3 };
        if self.show_output {
            let up = dir < 0;
            self.out_scroll = if up { self.out_scroll + step } else { self.out_scroll.saturating_sub(step) };
        } else {
            let max = view::max_scroll(self);
            let to =
                if dir < 0 { self.scroll.saturating_sub(step as u16) } else { self.scroll.saturating_add(step as u16) };
            self.scroll = to.min(max);
        }
        self.dirty = true;
    }

    fn modal_key(&mut self, modal: Modal, key: KeyEvent) -> Option<Modal> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match modal {
            Modal::Flow(mut f) => {
                // With an input box open, letters type; the shortcuts need ctrl.
                let key_ok = |want: bool| want && (ctrl || !f.prompt);
                let act = match key.code {
                    KeyCode::Esc => {
                        self.modal = Some(Modal::Flow(f));
                        self.close_modal();
                        return None;
                    }
                    KeyCode::Enter => Some(Act::Submit),
                    KeyCode::Char('o') if key_ok(f.url.is_some()) => Some(Act::Open),
                    KeyCode::Char('l') if key_ok(f.url.is_some()) => Some(Act::CopyLink),
                    KeyCode::Char('c') if !ctrl && !f.prompt && f.code.is_some() => Some(Act::CopyCode),
                    KeyCode::Char('r') if key_ok(f.kind == Kind::Pair || matches!(f.state, flow::State::Failed(_))) => {
                        Some(Act::Retry)
                    }
                    KeyCode::Char('t') if key_ok(true) => Some(Act::Terminal),
                    _ => {
                        if f.prompt {
                            f.input.key(key);
                        }
                        None
                    }
                };
                match act {
                    Some(act) => self.act(f, act),
                    None => Some(Modal::Flow(f)),
                }
            }
            Modal::Menu(mut m) => {
                match key.code {
                    KeyCode::Esc => return None,
                    KeyCode::Up | KeyCode::Char('k') => m.step(-1),
                    KeyCode::Down | KeyCode::Char('j') => m.step(1),
                    KeyCode::Enter => {
                        if let Some(choice) = m.choice() {
                            self.choose(&m, choice);
                            return None;
                        }
                    }
                    _ => {}
                }
                Some(Modal::Menu(m))
            }
            Modal::Version(mut input) => match key.code {
                KeyCode::Esc => None,
                KeyCode::Enter => {
                    let value = input.value.trim().to_string();
                    if !value.is_empty() && !VERSION_RE.is_match(&value) {
                        self.toast(Sev::Error, "Error", "Not a valid version");
                        return Some(Modal::Version(input));
                    }
                    let what = if value.is_empty() { "latest nightly".to_string() } else { value.clone() };
                    self.target = value;
                    self.toast(Sev::Info, "Notice", format!("Updates will install T3 {what}"));
                    None
                }
                _ => {
                    input.key(key);
                    Some(Modal::Version(input))
                }
            },
            Modal::Servers(s) => self.servers_key(s, key),
            Modal::Confirm(mut c) => match key.code {
                KeyCode::Esc | KeyCode::Char('n') => None,
                KeyCode::Char('y') => {
                    self.run_update(c.names, &c.only, c.desktop);
                    None
                }
                KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab | KeyCode::Char('h' | 'l') => {
                    c.cursor = 1 - c.cursor;
                    Some(Modal::Confirm(c))
                }
                KeyCode::Enter => {
                    if c.cursor == 0 {
                        self.run_update(c.names, &c.only, c.desktop);
                    }
                    None
                }
                _ => Some(Modal::Confirm(c)),
            },
            Modal::Changes(mut c) => {
                let page = (self.size.1 as usize * 4 / 5).saturating_sub(4).max(1);
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q' | 'c') | KeyCode::Enter => return None,
                    KeyCode::Up | KeyCode::Char('k') => c.scroll = c.scroll.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => c.scroll += 1,
                    KeyCode::PageUp => c.scroll = c.scroll.saturating_sub(page),
                    KeyCode::PageDown | KeyCode::Char(' ') => c.scroll += page,
                    KeyCode::Home | KeyCode::Char('g') => c.scroll = 0,
                    KeyCode::End | KeyCode::Char('G') => c.scroll = usize::MAX / 2,
                    _ => {}
                }
                Some(Modal::Changes(c))
            }
            Modal::Help { mut scroll } => match key.code {
                KeyCode::Esc | KeyCode::Char('q' | '?') | KeyCode::Enter => None,
                KeyCode::Up | KeyCode::Char('k') => Some(Modal::Help { scroll: scroll.saturating_sub(1) }),
                KeyCode::Down | KeyCode::Char('j') => {
                    scroll += 1;
                    Some(Modal::Help { scroll })
                }
                _ => Some(Modal::Help { scroll }),
            },
            Modal::Palette(mut p) => {
                let matches = p.matches();
                match key.code {
                    KeyCode::Esc => return None,
                    KeyCode::Up => p.cursor = p.cursor.saturating_sub(1),
                    KeyCode::Down => p.cursor = (p.cursor + 1).min(matches.len().saturating_sub(1)),
                    KeyCode::Char('n') if ctrl => p.cursor = (p.cursor + 1).min(matches.len().saturating_sub(1)),
                    KeyCode::Char('p') if ctrl => p.cursor = p.cursor.saturating_sub(1),
                    KeyCode::Enter => {
                        if let Some(&i) = matches.get(p.cursor) {
                            let cmd = p.items[i].1.clone();
                            self.run(cmd);
                        }
                        return None;
                    }
                    _ => {
                        p.input.key(key);
                        p.cursor = 0;
                    }
                }
                Some(Modal::Palette(p))
            }
        }
    }

    // ── mouse ──────────────────────────────────────────────────────────────

    fn hit(&self, x: u16, y: u16) -> Option<Hit> {
        self.hits.iter().rev().find(|(r, _)| r.contains((x, y).into())).map(|(_, h)| *h)
    }

    pub fn on_mouse(&mut self, m: MouseEvent) {
        if matches!(m.kind, MouseEventKind::Down(_)) {
            self.motion.intro = None;
        }
        let hit = self.hit(m.column, m.row);
        match m.kind {
            MouseEventKind::Moved => {
                self.hover(hit);
                let header = self.modal.is_none() && m.row < 3;
                let same = self.hover.as_ref().is_some_and(|h| h.hit == hit && h.header == header);
                if !same {
                    self.dirty |= self.hover.as_ref().is_some_and(|h| h.shown);
                    let pos = (m.column, m.row);
                    let shown = false;
                    self.hover =
                        (self.modal.is_none()).then(|| Hover { pos, hit, header, since: Instant::now(), shown });
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let dir = if m.kind == MouseEventKind::ScrollUp { -1 } else { 1 };
                match &mut self.modal {
                    Some(Modal::Changes(c)) => {
                        c.scroll = if dir < 0 { c.scroll.saturating_sub(3) } else { c.scroll + 3 };
                    }
                    Some(Modal::Help { scroll }) => {
                        *scroll = if dir < 0 { scroll.saturating_sub(3) } else { *scroll + 3 };
                    }
                    Some(_) => {}
                    None if hit == Some(Hit::Output) => {
                        let up = dir < 0;
                        self.out_scroll = if up { self.out_scroll + 3 } else { self.out_scroll.saturating_sub(3) };
                    }
                    None => {
                        let max = view::max_scroll(self);
                        let to = if dir < 0 { self.scroll.saturating_sub(3) } else { self.scroll + 3 };
                        self.scroll = to.min(max);
                    }
                }
                self.dirty = true;
            }
            MouseEventKind::Down(MouseButton::Left) => self.click(m.column, m.row, hit),
            _ => {}
        }
    }

    fn hover(&mut self, hit: Option<Hit>) {
        let Some(Hit::Row(i)) = hit else { return };
        match &mut self.modal {
            Some(Modal::Menu(m)) if matches!(m.items.get(i), Some(Item::Row { .. })) && m.cursor != i => {
                m.cursor = i;
                self.dirty = true;
            }
            Some(Modal::Palette(p)) if p.cursor != i => {
                p.cursor = i;
                self.dirty = true;
            }
            Some(Modal::Confirm(c)) if c.cursor != i => {
                c.cursor = i;
                self.dirty = true;
            }
            _ => {}
        }
    }

    fn click(&mut self, x: u16, y: u16, hit: Option<Hit>) {
        // A new modal has no mouse targets until the renderer has drawn it.
        if self.modal.is_some() && self.modal_rect.is_empty() {
            return;
        }
        let outside = !self.modal_rect.contains((x, y).into());
        self.dirty = true;
        self.hover = None;
        let Some(modal) = self.modal.take() else {
            match hit {
                Some(Hit::Card(i)) => {
                    self.select(i);
                    self.open_actions();
                }
                Some(Hit::UpdateAll) => self.open_update_all(),
                _ => {}
            }
            return;
        };
        let kept = match modal {
            Modal::Servers(mut s) => {
                if outside {
                    self.close_servers(s);
                    None
                } else {
                    match hit {
                        Some(Hit::Remove(i)) => self.remove_server(&mut s, i),
                        Some(Hit::Add) => {
                            let host = s.input.value.clone();
                            self.add_server(&mut s, &host);
                        }
                        Some(Hit::Suggest(i)) => {
                            if let Some(host) = s.suggestions().get(i).map(|h| h.to_string()) {
                                self.add_server(&mut s, &host);
                            }
                        }
                        Some(Hit::Row(i)) => s.row = Some(i),
                        _ => {}
                    }
                    Some(Modal::Servers(s))
                }
            }
            Modal::Flow(f) if outside => {
                self.modal = Some(Modal::Flow(f));
                self.close_modal();
                None
            }
            Modal::Flow(f) => match hit {
                Some(Hit::Act(act)) => self.act(f, act),
                _ => Some(Modal::Flow(f)),
            },
            _ if outside => None,
            Modal::Menu(mut m) => match hit {
                Some(Hit::Row(i)) if matches!(m.items.get(i), Some(Item::Row { .. })) => {
                    m.cursor = i;
                    if let Some(choice) = m.choice() {
                        self.choose(&m, choice);
                    }
                    None
                }
                _ => Some(Modal::Menu(m)),
            },
            Modal::Confirm(c) => match hit {
                Some(Hit::Row(i)) => {
                    if i == 0 {
                        self.run_update(c.names, &c.only, c.desktop);
                    }
                    None
                }
                _ => Some(Modal::Confirm(c)),
            },
            Modal::Palette(p) => match hit {
                Some(Hit::Row(i)) => {
                    if let Some(&at) = p.matches().get(i) {
                        self.run(p.items[at].1.clone());
                    }
                    None
                }
                _ => Some(Modal::Palette(p)),
            },
            other => Some(other),
        };
        if let Some(kept) = kept {
            self.modal = Some(kept);
        } else {
            self.invalidate_hits();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::snap::{frame, has};

    fn app(names: &[&str]) -> App {
        let mut a =
            App::new(names.iter().map(|n| n.to_string()).collect(), "/tmp/t3up-test/logs".into(), BTreeMap::new());
        a.take_effects();
        a
    }

    fn press(app: &mut App, keys: &str) {
        for c in keys.chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }

    fn code(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    /// Every host's current run ends healthy.
    fn settle(app: &mut App) {
        for name in app.names() {
            app.on_job(&name, Event::Complete);
            app.on_job(&name, Event::Exit { code: Some(0), error: None });
        }
        app.take_effects();
        app.toasts.clear();
    }

    /// (host, mode, only) of every job the effects start.
    fn jobs(fx: Vec<Effect>) -> Vec<(String, Mode, String)> {
        fx.into_iter()
            .filter_map(|e| match e {
                Effect::StartJob(j) => Some((j.host, j.mode, j.only)),
                _ => None,
            })
            .collect()
    }

    fn done(app: &mut App, host: &str, ok: bool) {
        if ok {
            app.on_job(host, Event::Complete);
        }
        app.on_job(host, Event::Exit { code: Some(if ok { 0 } else { 1 }), error: None });
    }

    fn menu(app: &App) -> &Menu {
        match &app.modal {
            Some(Modal::Menu(m)) => m,
            other => panic!("no menu open: {other:?}"),
        }
    }

    #[test]
    fn launch_checks_every_server_and_never_updates() {
        let mut a = App::new(vec!["a".into(), "b".into()], "/tmp/t3up-test/logs".into(), BTreeMap::new());
        let fx = a.take_effects();
        assert_eq!(jobs(fx), vec![("a".into(), Mode::Check, "all".into()), ("b".into(), Mode::Check, "all".into())]);
        assert!(a.hosts.iter().all(Host::running));
    }

    #[test]
    fn update_through_the_menu_picks_the_component() {
        let mut a = app(&["a", "b"]);
        settle(&mut a);
        press(&mut a, "u");
        assert_eq!(menu(&a).heading.to_string(), "a");
        assert!(
            menu(&a).items.iter().all(|i| !matches!(i, Item::Row { choice: Choice::Check, .. })),
            "u shows only the update section"
        );
        code(&mut a, KeyCode::Down);
        code(&mut a, KeyCode::Enter);
        assert!(a.modal.is_none());
        assert_eq!(jobs(a.take_effects()).last().unwrap(), &("a".to_string(), Mode::Update, "t3".to_string()));
        settle(&mut a);
        press(&mut a, "ujjj");
        code(&mut a, KeyCode::Enter);
        assert_eq!(jobs(a.take_effects()), vec![("a".into(), Mode::Update, "claude".into())]);
        settle(&mut a);
        press(&mut a, "u");
        code(&mut a, KeyCode::Enter);
        assert_eq!(jobs(a.take_effects()), vec![("a".into(), Mode::Update, "all".into())]);
    }

    #[test]
    fn menu_details() {
        let mut a = app(&["a"]);
        settle(&mut a);
        a.target = "0.0.45".into();
        a.hosts[0].steps.insert("Pi".into(), (StepState::Skip, "not installed".into()));
        a.open_actions();
        let details: Vec<String> = menu(&a)
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Row { label, detail, .. } => Some(format!("{label}|{detail}")),
                _ => None,
            })
            .collect();
        assert!(details.contains(&"T3 server|→ 0.0.45".to_string()), "{details:?}");
        assert!(details.contains(&"Pi|not installed · install latest".to_string()));
        assert!(details.contains(&"Codex CLI|→ latest".to_string()));
        assert!(
            details.iter().any(|d| d.starts_with("What's new|"))
                && details.iter().any(|d| d.starts_with("Terminal|ssh in"))
        );
        a.modal = None;
        let mut a = app(&["a", "b"]);
        settle(&mut a);
        a.hosts[1].steps.insert("Pi".into(), (StepState::Skip, "not installed".into()));
        a.open_update_all();
        assert_eq!(menu(&a).heading.to_string(), "All servers   2 · a first");
        assert!(
            menu(&a)
                .items
                .iter()
                .any(|i| matches!(i, Item::Row { detail, .. } if detail == "→ latest · installs on 1 missing"))
        );
    }

    #[test]
    fn signed_out_codex_after_an_update_of_codex_signs_in() {
        let mut a = app(&["box"]);
        settle(&mut a);
        // A check never signs in.
        a.refresh();
        a.take_effects();
        a.on_job("box", Event::Auth("Codex".into()));
        done(&mut a, "box", true);
        assert!(a.take_effects().iter().all(|e| !matches!(e, Effect::Terminal { .. })));
        // An update of T3 alone doesn't either.
        a.start(vec!["box".into()], Mode::Update, "t3");
        a.take_effects();
        a.on_job("box", Event::Auth("Codex".into()));
        done(&mut a, "box", true);
        assert!(a.take_effects().iter().all(|e| !matches!(e, Effect::Terminal { .. })));
        // An update of Codex does, in a window of its own, then checks the server again.
        a.start(vec!["box".into()], Mode::Update, "codex");
        a.take_effects();
        a.on_job("box", Event::Auth("Codex".into()));
        done(&mut a, "box", true);
        let fx = a.take_effects();
        assert!(fx.iter().all(|e| !matches!(e, Effect::Terminal { .. })), "no raw terminal");
        let id = fx
            .iter()
            .find_map(|e| match e {
                Effect::StartFlow { id, host, command } if host == "box" => {
                    assert!(command.ends_with("codex login --device-auth"), "{command}");
                    Some(*id)
                }
                _ => None,
            })
            .expect("a sign-in");
        assert!(matches!(&a.modal, Some(Modal::Flow(f)) if f.tool == "Codex"));
        a.on_result(Res::FlowOut {
            id,
            text: "1. Open https://auth.openai.com/codex/device\r\n2. one-time code\r\n AB12-CD345\r\n".into(),
        });
        press(&mut a, "c");
        assert!(a.take_effects().contains(&Effect::Copy("AB12-CD345".into())));
        a.on_result(Res::FlowExit { id, code: Some(0) });
        assert_eq!(jobs(a.take_effects()), vec![("box".into(), Mode::Check, "all".into())]);
        assert!(a.toasts.iter().any(|t| t.title == "Signed in"));
    }

    #[test]
    fn install_everything_sets_up_a_new_machine() {
        let mut a = app(&["box"]);
        settle(&mut a);
        // Nothing missing: no such row.
        a.open_actions();
        assert!(!menu(&a).items.iter().any(|i| matches!(i, Item::Row { label, .. } if label == "Install everything")));
        a.modal = None;
        // A server missing tools offers it; it picks every component by name, so each is installed.
        a.refresh();
        a.take_effects();
        a.on_job("box", Event::Skip("T3: not installed".into()));
        a.on_job("box", Event::Skip("Codex: not installed".into()));
        done(&mut a, "box", true);
        a.take_effects();
        a.open_actions();
        let at =
            menu(&a).items.iter().position(|i| matches!(i, Item::Row { label, .. } if label == "Install everything"));
        let Some(at) = at else { panic!("no Install everything") };
        if let Some(Modal::Menu(m)) = &mut a.modal {
            m.cursor = at;
        }
        code(&mut a, KeyCode::Enter);
        assert_eq!(
            jobs(a.take_effects()),
            vec![("box".into(), Mode::Update, "t3,codex,claude,opencode,grok,pi".into())]
        );
    }

    #[test]
    fn claude_takes_the_code_in_the_window() {
        let mut a = app(&["box"]);
        settle(&mut a);
        a.start_flow("box", "Claude");
        let id = match a.take_effects().as_slice() {
            [Effect::StartFlow { id, .. }] => *id,
            fx => panic!("{fx:?}"),
        };
        a.on_result(Res::FlowOut {
            id,
            text: "visit: https://claude.com/x?state=1\r\nPaste code here if prompted > ".into(),
        });
        // Letters type into the box, not shortcuts; enter sends it to the tool with a return.
        press(&mut a, "olc");
        code(&mut a, KeyCode::Enter);
        assert_eq!(a.take_effects(), vec![Effect::FlowInput { id, text: "olc\r".into() }]);
        // A second sign-in waits for this window.
        a.start_flow("box", "Codex");
        assert!(a.take_effects().is_empty());
        // Esc closes it and stops the tool; then the queued one opens.
        code(&mut a, KeyCode::Esc);
        assert_eq!(a.take_effects(), vec![Effect::StopFlow(id)]);
        a.tick();
        assert!(matches!(&a.modal, Some(Modal::Flow(f)) if f.tool == "Codex"));
    }

    #[test]
    fn canary_goes_first_then_releases_the_rest() {
        let mut a = app(&["a", "b", "c"]);
        settle(&mut a);
        press(&mut a, "a");
        assert_eq!(menu(&a).heading.to_string(), "All servers   3 · a first");
        code(&mut a, KeyCode::Down); // T3 server
        code(&mut a, KeyCode::Enter);
        assert_eq!(jobs(a.take_effects()), vec![("a".into(), Mode::Update, "t3".into())]);
        assert_eq!(a.queued.len(), 2);
        assert_eq!(status(&a.hosts[1], true, Duration::ZERO).2, "queued");
        done(&mut a, "a", true);
        assert_eq!(
            jobs(a.take_effects()),
            vec![("b".into(), Mode::Update, "t3".into()), ("c".into(), Mode::Update, "t3".into())]
        );
        assert!(a.queued.is_empty());
    }

    #[test]
    fn running_canary_does_not_arm_a_rollout() {
        let mut a = app(&["a", "b"]);
        done(&mut a, "b", true);
        a.take_effects();
        a.begin_update(a.names(), "t3", false);
        assert!(a.canary.is_none() && a.queued.is_empty());
        assert_eq!(a.toasts.last().unwrap().sev, Sev::Warning);
        done(&mut a, "a", true);
        assert!(jobs(a.take_effects()).is_empty());
    }

    #[test]
    fn removing_canary_cancels_the_rollout() {
        let mut a = app(&["a", "b"]);
        settle(&mut a);
        a.begin_update(a.names(), "t3", false);
        a.take_effects();
        a.set_hosts(vec!["b".into()]);
        done(&mut a, "a", false); // removed hosts' events are ignored
        a.set_hosts(vec!["a".into(), "b".into()]);
        a.take_effects();
        done(&mut a, "a", true); // the new host's check cannot release the queue
        assert!(jobs(a.take_effects()).is_empty());
        assert!(a.canary.is_none() && a.queued.is_empty());
    }

    #[test]
    fn another_job_cannot_release_the_canary_queue() {
        let mut a = app(&["a", "b"]);
        settle(&mut a);
        a.begin_update(a.names(), "t3", false);
        a.take_effects();
        a.hosts[0].reset(Mode::Update, "t3");
        done(&mut a, "a", true);
        assert!(jobs(a.take_effects()).is_empty());
        assert!(a.queued.contains("b"));
    }

    #[test]
    fn queued_updates_keep_the_canarys_version_and_components() {
        let mut a = app(&["a", "b"]);
        settle(&mut a);
        a.target = "0.0.45".into();
        a.begin_update(a.names(), "t3,codex", false);
        let launched = a.take_effects();
        assert!(launched.iter().any(|e| matches!(e, Effect::StartJob(j) if j.version == "0.0.45")));
        press(&mut a, "v");
        let Some(Modal::Version(input)) = &mut a.modal else { panic!("no version prompt") };
        input.value = "0.0.46".into();
        code(&mut a, KeyCode::Enter);
        assert_eq!(a.target, "0.0.46");
        done(&mut a, "a", true);
        let fx = a.take_effects();
        let jobs: Vec<_> = fx
            .iter()
            .filter_map(|e| match e {
                Effect::StartJob(j) => Some((j.host.as_str(), j.version.as_str(), j.only.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(jobs, [("b", "0.0.45", "t3,codex")]);
    }

    #[test]
    fn failed_canary_cancels_the_queue() {
        let mut a = app(&["a", "b"]);
        settle(&mut a);
        a.begin_update(a.names(), "all", true);
        assert_eq!(jobs(a.take_effects()).len(), 1);
        a.on_job("a", Event::Output { tag: String::new(), text: "disk full".into() });
        done(&mut a, "a", false);
        assert!(jobs(a.take_effects()).is_empty());
        assert!(a.queued.is_empty() && !a.hosts[1].running());
        let toast = a.toasts.last().unwrap();
        assert_eq!(toast.sev, Sev::Error);
        assert_eq!(toast.text, "Canary a failed: disk full. The other servers were not updated.");
    }

    #[test]
    fn buffered_palette_click_cannot_confirm_a_busy_update() {
        let mut a = app(&["a", "b"]);
        settle(&mut a);
        a.hosts[0].busy = Some(2);
        a.open_palette();
        let Some(Modal::Palette(p)) = &mut a.modal else { panic!("no palette") };
        p.input.set("Update T3");
        frame(&mut a, 100, 30);
        let row = a.hits.iter().find(|(_, h)| *h == Hit::Row(0)).unwrap().0;
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: row.x,
            row: row.y,
            modifiers: KeyModifiers::NONE,
        };
        a.on_mouse(click);
        assert!(matches!(a.modal, Some(Modal::Confirm(_))));
        a.on_mouse(click); // buffered before the confirmation was drawn
        assert!(matches!(a.modal, Some(Modal::Confirm(_))));
        assert!(jobs(a.take_effects()).is_empty());
        frame(&mut a, 100, 30);
        let row = a.hits.iter().find(|(_, h)| *h == Hit::Row(0)).unwrap().0;
        a.on_mouse(MouseEvent { column: row.x, row: row.y, ..click });
        assert_eq!(jobs(a.take_effects()), [("a".into(), Mode::Update, "t3".into())]);
    }

    #[test]
    fn busy_agents_need_a_confirmation_before_t3_restarts() {
        let mut a = app(&["a", "b"]);
        settle(&mut a);
        a.hosts[1].busy = Some(2);
        // Not touching T3: no question.
        a.begin_update(a.names(), "codex", false);
        assert!(a.modal.is_none());
        settle(&mut a);
        a.begin_update(vec!["b".into()], "all", false);
        let Some(Modal::Confirm(c)) = &a.modal else { panic!("no confirm") };
        assert_eq!(c.busy, vec![("b".to_string(), 2)]);
        assert!(jobs(a.take_effects()).is_empty());
        code(&mut a, KeyCode::Enter); // Cancel is the default
        assert!(a.modal.is_none() && jobs(a.take_effects()).is_empty());
        a.begin_update(vec!["b".into()], "t3,codex", false);
        code(&mut a, KeyCode::Left);
        code(&mut a, KeyCode::Enter);
        assert_eq!(jobs(a.take_effects()), vec![("b".into(), Mode::Update, "t3,codex".into())]);
        settle(&mut a);
        a.begin_update(vec!["b".into()], "all", false);
        press(&mut a, "y");
        assert_eq!(jobs(a.take_effects()).len(), 1);
    }

    #[test]
    fn menus_close_by_escape_and_by_clicking_outside() {
        let mut a = app(&["first", "second"]);
        settle(&mut a);
        code(&mut a, KeyCode::Enter);
        assert!(a.modal.is_some());
        code(&mut a, KeyCode::Esc);
        assert!(a.modal.is_none());
        code(&mut a, KeyCode::Enter);
        frame(&mut a, 100, 30);
        let click = |a: &mut App, x, y| {
            a.on_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: x,
                row: y,
                modifiers: KeyModifiers::NONE,
            })
        };
        click(&mut a, 0, 0);
        assert!(a.modal.is_none());
        // A click on a card selects it and opens its actions.
        frame(&mut a, 100, 30);
        let card = a.hits.iter().find(|(_, h)| *h == Hit::Card(1)).unwrap().0;
        click(&mut a, card.x + 3, card.y + 2);
        assert_eq!(a.selected, 1);
        assert_eq!(menu(&a).hosts, vec!["second".to_string()]);
        // A click on a row chooses it; inside the panel but off the rows does nothing.
        frame(&mut a, 100, 30);
        let inside = a.modal_rect;
        click(&mut a, inside.x + 1, inside.y + 1);
        assert!(a.modal.is_some());
        let row = a.hits.iter().find(|(_, h)| *h == Hit::Row(3)).unwrap().0; // Codex
        click(&mut a, row.x + 4, row.y);
        assert_eq!(jobs(a.take_effects()), vec![("second".into(), Mode::Update, "codex".into())]);
        // The Update all button.
        settle(&mut a);
        frame(&mut a, 100, 30);
        let button = a.hits.iter().find(|(_, h)| *h == Hit::UpdateAll).unwrap().0;
        click(&mut a, button.x + 2, button.y + 1);
        assert_eq!(menu(&a).heading.to_string(), "All servers   2 · first first");
    }

    #[test]
    fn hover_moves_the_menu_cursor() {
        let mut a = app(&["a"]);
        a.open_actions();
        frame(&mut a, 100, 30);
        let row = a.hits.iter().find(|(_, h)| *h == Hit::Row(4)).unwrap().0;
        a.on_mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: row.x + 3,
            row: row.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(menu(&a).cursor, 4);
    }

    #[test]
    fn grid_navigation_on_two_columns_and_two_rows() {
        let mut a = app(&["first", "second", "third"]);
        assert_eq!(crate::tui::view::geometry(&a).cols, 2);
        assert_eq!(a.selected, 0);
        code(&mut a, KeyCode::Down);
        assert_eq!(a.selected, 2);
        code(&mut a, KeyCode::Up);
        assert_eq!(a.selected, 0);
        code(&mut a, KeyCode::Right);
        code(&mut a, KeyCode::Down); // nothing below `second`: clamps to the last card
        assert_eq!(a.selected, 2);
        press(&mut a, "k");
        assert_eq!(a.selected, 0);
        code(&mut a, KeyCode::Left);
        assert_eq!(a.selected, 0);
        press(&mut a, "jj");
        assert_eq!(a.selected, 2);
        a.on_resize(60, 24);
        assert_eq!(crate::tui::view::geometry(&a).cols, 1);
        code(&mut a, KeyCode::Up);
        assert_eq!(a.selected, 1);
    }

    #[test]
    fn selection_scrolls_into_view() {
        let mut a = app(&["a", "b", "c", "d", "e", "f"]);
        a.on_resize(60, 20);
        for _ in 0..5 {
            press(&mut a, "j");
        }
        assert_eq!(a.selected, 5);
        assert!(a.scroll > 0);
        let buf = frame(&mut a, 60, 20);
        assert!(has(&buf, "─ f ─") || has(&buf, "╭ f "), "{:?}", crate::tui::snap::text(&buf));
    }

    #[test]
    fn version_prompt_rejects_invalid_input() {
        let mut a = app(&["a"]);
        press(&mut a, "v");
        press(&mut a, "bad;v");
        code(&mut a, KeyCode::Enter);
        assert!(matches!(a.modal, Some(Modal::Version(_))), "stays open");
        assert!(a.target.is_empty());
        assert_eq!(a.toasts.last().unwrap().sev, Sev::Error);
        for _ in 0..5 {
            code(&mut a, KeyCode::Backspace);
        }
        a.on_paste("0.0.46-nightly.20261003.2623");
        code(&mut a, KeyCode::Enter);
        assert!(a.modal.is_none());
        assert_eq!(a.target, "0.0.46-nightly.20261003.2623");
        assert_eq!(a.toasts.last().unwrap().text, "Updates will install T3 0.0.46-nightly.20261003.2623");
        // The next update carries it; a blank value goes back to the latest nightly.
        settle(&mut a);
        a.start(vec!["a".into()], Mode::Update, "t3");
        assert!(
            a.take_effects()
                .iter()
                .any(|e| matches!(e, Effect::StartJob(j) if j.version == "0.0.46-nightly.20261003.2623"))
        );
        settle(&mut a);
        press(&mut a, "v");
        a.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        code(&mut a, KeyCode::Enter);
        assert!(a.target.is_empty());
    }

    #[test]
    fn narrow_header_keeps_the_outdated_desktop() {
        let mut a = app(&["box"]);
        a.desktop = "0.0.46-nightly.20261003.2623".into();
        a.hosts[0].current.insert("T3".into(), "0.0.46-nightly.20261003.2632".into());
        a.local = "Ganz-Harbour".into();
        let (left, _) = crate::tui::view::header_text(&a, 47);
        assert!(left.to_string().contains("update desktop → #2632"), "{left}");
        let (left, _) = crate::tui::view::header_text(&a, 200);
        assert!(
            left.to_string().contains("On Ganz-Harbour") && left.to_string().contains("→ #2632 · d to update"),
            "{left}"
        );
        // Even when the counts take the room.
        a.hosts[0].status = Status::Failed;
        let (left, right) = crate::tui::view::header_text(&a, 33);
        assert!(left.to_string().contains("update desktop → #2632") && right.width() == 0, "{left} | {right}");
        a.desktop_updating = true;
        assert!(crate::tui::view::header_text(&a, 200).0.to_string().contains("updating desktop…"));
    }

    #[test]
    fn slots_are_remembered_from_tools_json() {
        let known =
            BTreeMap::from([("second".to_string(), vec!["Codex".to_string(), "T3".to_string(), "Nope".to_string()])]);
        let a = App::new(vec!["second".into()], "/tmp/t3up-test/logs".into(), known);
        assert_eq!(a.tools, vec!["T3", "Codex"]);
    }

    #[test]
    fn a_finished_job_saves_the_tools_it_found() {
        let mut a = app(&["second"]);
        a.on_job("second", Event::Done("Codex: codex-cli 1.0.0".into()));
        a.on_job("second", Event::Skip("Pi: not installed".into()));
        done(&mut a, "second", true);
        let saved = a.take_effects().into_iter().find_map(|e| match e {
            Effect::SaveTools(t) => Some(t),
            _ => None,
        });
        assert_eq!(saved.unwrap()["second"], vec!["Codex", "T3"]);
        assert_eq!(a.tools, vec!["T3", "Codex"]);
        assert_eq!(a.toasts.len(), 0, "a healthy check says nothing");
    }

    #[test]
    fn failures_and_updates_toast() {
        let mut a = app(&["a", "b"]);
        a.on_job("a", Event::Output { tag: String::new(), text: "Connection refused".into() });
        done(&mut a, "a", false);
        assert_eq!((a.toasts[0].sev, a.toasts[0].text.as_str()), (Sev::Error, "a: Connection refused"));
        a.start(vec!["b".into()], Mode::Update, "all");
        a.on_job("b", Event::Rollback("T3: 0.0.47 -> 0.0.46".into()));
        done(&mut a, "b", false);
        assert!(a.toasts.iter().any(|t| t.sev == Sev::Warning && t.title == "Rolled back"));
        settle(&mut a);
        a.start(vec!["a".into()], Mode::Update, "codex");
        done(&mut a, "a", true);
        assert!(a.toasts.last().unwrap().text.starts_with("a updated in "));
        // Events of a server that was removed mid-run are ignored.
        a.on_job("gone", Event::Complete);
    }

    #[test]
    fn running_servers_are_not_started_again() {
        let mut a = app(&["a"]);
        a.start(vec!["a".into()], Mode::Check, "all");
        assert_eq!(a.toasts.last().unwrap().text, "Already running: a");
        assert!(jobs(a.take_effects()).is_empty());
    }

    #[test]
    fn quit_needs_a_second_press_while_jobs_run() {
        let mut a = app(&["a"]);
        press(&mut a, "q");
        assert!(a.take_effects().is_empty());
        assert_eq!(a.toasts.last().unwrap().text, "1 server(s) still running. Press q again to stop them and quit.");
        press(&mut a, "q");
        assert!(matches!(a.take_effects()[..], [Effect::Quit]));
        let mut a = app(&["a"]);
        a.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(matches!(a.take_effects()[..], [Effect::Quit]));
        settle(&mut a);
        press(&mut a, "q");
        assert!(matches!(a.take_effects()[..], [Effect::Quit]));
    }

    #[test]
    fn ssh_hands_over_the_terminal() {
        let mut a = app(&["a", "b"]);
        code(&mut a, KeyCode::Right);
        press(&mut a, "s");
        let fx = a.take_effects();
        let Some(Effect::Terminal { command, banner }) = fx.first() else { panic!("{fx:?}") };
        assert_eq!(command[1..], ["b"]);
        assert!(banner.is_empty());
    }

    #[test]
    fn with_no_servers_host_actions_do_nothing() {
        let mut a = app(&["a"]);
        a.set_hosts(vec![]);
        for k in ["j", "u", "a", "r", "c", "s", "l"] {
            press(&mut a, k);
        }
        code(&mut a, KeyCode::Enter);
        assert!(
            a.modal.is_none()
                && a.take_effects().iter().all(|e| !matches!(e, Effect::StartJob(_) | Effect::Terminal { .. }))
        );
        frame(&mut a, 100, 30);
    }

    #[test]
    fn applying_a_new_server_list_keeps_state_of_kept_hosts() {
        let mut a = app(&["first", "second", "third"]);
        settle(&mut a);
        a.hosts[1].sys = "kept".into();
        a.selected = 1;
        a.set_hosts(vec!["second".into(), "fourth".into()]);
        assert_eq!(a.names(), ["second", "fourth"]);
        assert_eq!(a.hosts[0].sys, "kept");
        assert_eq!(a.hosts[0].status, Status::Ok);
        assert_eq!(a.selected, 0, "still on `second`");
        assert_eq!(jobs(a.take_effects()), vec![("fourth".into(), Mode::Check, "all".into())]);
    }

    #[test]
    fn palette_filters_by_subsequence_and_runs() {
        let mut a = app(&["alpha", "beta"]);
        settle(&mut a);
        a.on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        press(&mut a, "sshbe");
        let Some(Modal::Palette(p)) = &a.modal else { panic!() };
        let hits: Vec<&str> = p.matches().iter().map(|&i| p.items[i].0.as_str()).collect();
        assert_eq!(hits, ["SSH into beta"]);
        code(&mut a, KeyCode::Enter);
        assert!(matches!(&a.take_effects()[..], [Effect::Terminal { command, .. }] if command[1] == "beta"));
        a.on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        press(&mut a, "upd codex");
        code(&mut a, KeyCode::Enter);
        assert_eq!(jobs(a.take_effects())[0], ("alpha".into(), Mode::Update, "codex".into()), "the canary goes first");
    }

    #[test]
    fn whats_new_fetches_once_per_update() {
        let mut a = app(&["box"]);
        settle(&mut a);
        press(&mut a, "c");
        assert!(matches!(&a.modal, Some(Modal::Changes(c)) if c.items.is_empty()));
        assert!(has(&frame(&mut a, 100, 30), "Everything on box is up to date."));
        a.modal = None;
        a.latest.insert("T3".into(), "0.0.46-nightly.20261003.2648".into());
        a.hosts[0].current.insert("T3".into(), "0.0.46-nightly.20261003.2623".into());
        press(&mut a, "c");
        let fx = a.take_effects();
        assert!(
            fx.iter().any(|e| matches!(e, Effect::FetchChangelog { component, .. } if component == "T3")),
            "{fx:?}"
        );
        assert!(has(&frame(&mut a, 100, 30), "Loading release notes"));
        let key =
            ("T3".to_string(), "0.0.46-nightly.20261003.2623".to_string(), "0.0.46-nightly.20261003.2648".to_string());
        let release = Release { tag: "v1".into(), date: "2026-10-04".into(), notes: vec!["fix(web): panels".into()] };
        a.on_result(Res::Changelog { key, result: Ok(vec![release]) });
        let buf = frame(&mut a, 100, 30);
        assert!(has(&buf, "T3  0.0.46 #2623 → #2648 · 1 release") && has(&buf, "fix(web): panels"));
        a.modal = None;
        press(&mut a, "c");
        assert!(a.take_effects().iter().all(|e| !matches!(e, Effect::FetchChangelog { .. })), "cached");
    }

    #[test]
    fn desktop_update_reports_progress() {
        let mut a = app(&["a"]);
        press(&mut a, "d");
        assert_eq!(menu(&a).heading.to_string(), "T3 Code desktop   not detected");
        code(&mut a, KeyCode::Enter);
        assert!(matches!(&a.take_effects()[..], [Effect::UpdateDesktop]));
        assert!(a.desktop_updating);
        press(&mut a, "d");
        assert_eq!(a.toasts.last().unwrap().text, "The desktop app is already updating");
        a.on_result(Res::DesktopSay("Downloading".into()));
        a.on_result(Res::DesktopDone(Ok("0.0.46-nightly.20261003.2632".into())));
        assert!(!a.desktop_updating && a.toasts.last().unwrap().text == "T3 Code desktop 0.0.46-nightly");
        a.on_result(Res::DesktopDone(Err("no space".into())));
        assert_eq!(a.toasts.last().unwrap().sev, Sev::Error);
    }

    #[test]
    fn updating_everything_on_all_servers_also_updates_an_outdated_desktop() {
        let mut a = app(&["a", "b"]);
        settle(&mut a);
        a.desktop = "0.0.46-nightly.20261003.2623".into();
        a.hosts[0].current.insert("T3".into(), "0.0.46-nightly.20261003.2632".into());
        press(&mut a, "a");
        assert!(
            menu(&a).items.iter().any(
                |i| matches!(i, Item::Row { detail, .. } if detail == "T3, every installed provider, desktop app")
            )
        );
        code(&mut a, KeyCode::Enter);
        let fx = a.take_effects();
        assert!(fx.iter().any(|e| matches!(e, Effect::UpdateDesktop)));
        assert!(a.desktop_updating);
    }

    #[test]
    fn output_scrolls_and_follows() {
        let mut a = app(&["a"]);
        a.toggle_output();
        for n in 0..50 {
            a.on_job("a", Event::Output { tag: String::new(), text: format!("line {n}") });
        }
        let buf = frame(&mut a, 100, 30);
        assert!(has(&buf, "line 49") && !has(&buf, "line 3 "));
        code(&mut a, KeyCode::PageUp);
        assert!(a.out_scroll > 0);
        a.on_job("a", Event::Output { tag: String::new(), text: "line 50".into() });
        assert!(a.out_scroll > 3, "stays put while reading back");
        a.out_scroll = 0;
        assert!(has(&frame(&mut a, 100, 30), "line 50"));
    }
}
