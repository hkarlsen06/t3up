//! What t3up knows about each server, and the pure logic over it: components, the events a
//! remote run reports, how they change a host, and how versions compare and print.
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;

/// What each server runs, in card order: step name and menu label. The `--only` key and the
/// command are the lowercased name. Adding one: a `<name>_step` in remote.sh, `names` there, and here.
/// T3 Code's other providers need nothing here: Cursor and Antigravity ship inside T3.
pub const COMPONENTS: &[(&str, &str)] = &[
    ("T3", "T3 server"),
    ("Codex", "Codex CLI"),
    ("Claude", "Claude Code"),
    ("OpenCode", "OpenCode"),
    ("Grok", "Grok CLI"),
    ("Pi", "Pi"),
];

pub fn is_component(name: &str) -> bool {
    COMPONENTS.iter().any(|(n, _)| *n == name)
}

/// Position in card order; unknown names sort last.
pub fn component_index(name: &str) -> usize {
    COMPONENTS.iter().position(|(n, _)| *n == name).unwrap_or(COMPONENTS.len())
}

/// Non-interactive SSH skips shell rc files: add the usual per-user and Homebrew bin dirs.
/// One line, so sign-in commands can start with it too. ponytail: nvm picks highest by name, not semver.
pub const PATH_SETUP: &str = r#"for dir in "$HOME"/.nvm/versions/node/*/bin; do if [ -d "$dir" ]; then PATH="$dir:$PATH"; fi; done; PATH="$HOME/.local/bin:$HOME/.npm-global/bin:$HOME/.bun/bin:$HOME/.opencode/bin:$HOME/.volta/bin:$HOME/.local/share/pnpm:/usr/local/bin:/opt/homebrew/bin:$PATH"; export PATH"#;

/// The POSIX sh script each server runs, piped to `ssh HOST sh -s -- MODE VERSION ONLY`.
pub fn remote_script() -> String {
    format!("set -eu\n{PATH_SETUP}\n{}", include_str!("remote.sh"))
}

/// Interactive sign-in per tool: each prints a link or code to finish in your browser.
/// T3 itself pairs a new server with your app: `t3 pair` prints a code and exits, so it waits for Enter.
pub const LOGIN: &[(&str, &str)] = &[
    ("Codex", "codex login --device-auth"),
    ("Claude", "claude auth login"),
    ("Grok", "grok login"),
    ("T3", "t3 pair && printf '\\nPress Enter to go back to t3up. ' && read -r _"),
];

/// What a reported `auth` asks of you: T3 pairs a new server, a tool signs in.
pub fn sign_in_what(name: &str) -> &'static str {
    if name == "T3" { "needs pairing with your T3 Code app" } else { "not signed in" }
}

/// Whether `h`'s last run found no T3 on it.
pub fn no_t3(h: &Host) -> bool {
    matches!(h.steps.get("T3"), Some((StepState::Skip, _)))
}

/// The remote command that signs in to a tool, for `ssh -t HOST <command>`.
pub fn login_command(name: &str) -> Option<String> {
    LOGIN.iter().find(|(n, _)| *n == name).map(|(_, c)| format!("{PATH_SETUP}; {c}"))
}

pub static VERSION_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9][a-zA-Z0-9.+-]*$").unwrap());
pub static HOST_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9_][a-zA-Z0-9_.@:-]*$").unwrap());
/// A version inside `--version` output.
static RAW_VERSION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+\.\d+\.\d+(?:[-+][a-zA-Z0-9.+-]+)?").unwrap());
static NIGHTLY_TAIL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(-nightly)\.\d{8}\.\d+$").unwrap());
static NIGHTLY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(.*-nightly)\.(\d{8})\.(\d+)$").unwrap());
static ANSI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]").unwrap());
static DIGITS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+").unwrap());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Check,
    Update,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Check => "check",
            Mode::Update => "update",
        }
    }
}

/// What a remote run reports, in order. `job::run_job` sends these; `Host::apply` folds them in.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The job started and writes its raw output to this log.
    Start { log: PathBuf },
    /// A line of output. `tag` is the step that printed it ('Codex', 'T3'…), '' if untagged.
    Output { tag: String, text: String },
    /// `@@t3up begin STEP`: a step started.
    Begin(String),
    /// `@@t3up done DETAIL`, e.g. 'Codex: codex-cli 1.0.0 -> codex-cli 1.1.0' or 'Health: OK'.
    Done(String),
    /// `@@t3up skip DETAIL`, e.g. 'Pi: not installed'.
    Skip(String),
    /// `@@t3up fail STEP`.
    Fail(String),
    /// `@@t3up auth NAME`: that tool is not signed in.
    Auth(String),
    /// `@@t3up version V`: the T3 server version.
    Version(String),
    /// `@@t3up busy N`: agent processes running under the T3 server right now.
    Busy(u32),
    /// `@@t3up sys TEXT`: one line about the machine, e.g. 'load 0.24 · disk 21% · up 1d 5h'.
    Sys(String),
    /// `@@t3up rollback DETAIL`, e.g. 'T3: 0.0.47 -> 0.0.46': health failed after an update, so
    /// T3 was put back. The run still fails.
    Rollback(String),
    /// `@@t3up complete OK`: the script reached its end.
    Complete,
    /// ssh exited (`code` None if it was killed or never started); `error` if it could not run.
    Exit { code: Option<i32>, error: Option<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Idle,
    Running,
    Ok,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    Begin,
    Done,
    Skip,
    Fail,
}

pub const MAX_LINES: usize = 2000;

#[derive(Debug, Clone)]
pub struct Host {
    pub name: String,
    /// Display lines across runs, newest last, at most MAX_LINES.
    pub lines: VecDeque<String>,
    /// Components seen on this server. Survives runs, so a check only shows tiles for tools seen before.
    pub installed: BTreeSet<String>,
    pub mode: Mode,
    pub only: String,
    pub status: Status,
    /// Step name -> (state, display value), e.g. 'Codex' -> (Done, '0.160.0 → 0.161.0').
    pub steps: HashMap<String, (StepState, String)>,
    /// Tools reported as not signed in.
    pub auth: Vec<String>,
    /// Tool -> full version it reported, e.g. 'T3' -> '0.0.46-nightly.20261003.2623'.
    pub current: HashMap<String, String>,
    pub error: String,
    /// The T3 server version, '—' until reported.
    pub version: String,
    pub log: Option<PathBuf>,
    pub started: Instant,
    pub elapsed: Duration,
    /// Agent processes running under T3 at the last report; None if never reported.
    pub busy: Option<u32>,
    /// One line about the machine from the last run, '' if none.
    pub sys: String,
    /// 'a → b' if the last run rolled T3 back, else ''.
    pub rollback: String,
    complete: bool,
    /// Latest output line per step tag ('' = any).
    last: HashMap<String, String>,
}

impl Host {
    pub fn new(name: &str) -> Self {
        Host {
            name: name.to_string(),
            lines: VecDeque::new(),
            installed: BTreeSet::from(["T3".to_string()]),
            mode: Mode::Check,
            only: "all".into(),
            status: Status::Idle,
            steps: HashMap::new(),
            auth: vec![],
            current: HashMap::new(),
            error: String::new(),
            version: "—".into(),
            log: None,
            started: Instant::now(),
            elapsed: Duration::ZERO,
            busy: None,
            sys: String::new(),
            rollback: String::new(),
            complete: false,
            last: HashMap::new(),
        }
    }

    /// Start a new run: clears what the last run reported (not `installed`, `busy`, `sys` or
    /// `lines`) and adds a header line. Returns the header.
    pub fn reset(&mut self, mode: Mode, only: &str) -> String {
        self.mode = mode;
        self.only = only.to_string();
        self.status = Status::Running;
        self.steps.clear();
        self.auth.clear();
        self.current.clear();
        self.error.clear();
        self.version = "—".into();
        self.log = None;
        self.started = Instant::now();
        self.elapsed = Duration::ZERO;
        self.rollback.clear();
        self.complete = false;
        self.last.clear();
        let what = if mode == Mode::Update { format!("update {only}") } else { "check".into() };
        let header = format!("── {what} · {} ──", chrono::Local::now().format("%H:%M:%S"));
        self.push(header.clone());
        header
    }

    pub fn running(&self) -> bool {
        self.status == Status::Running
    }

    /// Seconds this run took, or has taken so far.
    pub fn took(&self) -> Duration {
        if self.running() { self.started.elapsed() } else { self.elapsed }
    }

    fn push(&mut self, line: String) {
        if self.lines.len() >= MAX_LINES {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    /// Fold one event into the host. Returns the display line it added, if any.
    pub fn apply(&mut self, event: Event) -> Option<String> {
        let line = match event {
            Event::Start { log } => {
                self.log = Some(log);
                return None;
            }
            Event::Output { tag, text } => {
                if text.is_empty() {
                    return None;
                }
                self.last.insert(tag.clone(), text.clone());
                self.last.insert(String::new(), text.clone());
                if tag.is_empty() { text } else { format!("{tag}: {text}") }
            }
            Event::Begin(step) => {
                self.steps.insert(step, (StepState::Begin, String::new()));
                return None;
            }
            Event::Done(detail) => self.finish(StepState::Done, &detail),
            Event::Skip(detail) => self.finish(StepState::Skip, &detail),
            Event::Fail(detail) => self.finish(StepState::Fail, &detail),
            Event::Auth(name) => {
                let line = format!("! {name}  {}", sign_in_what(&name));
                self.auth.push(name);
                line
            }
            Event::Version(version) => {
                self.current.insert("T3".into(), version.clone());
                self.version = version;
                return None;
            }
            Event::Busy(n) => {
                self.busy = Some(n);
                return None;
            }
            Event::Sys(text) => {
                self.sys = text;
                return None;
            }
            Event::Rollback(detail) => {
                self.rollback = version_text(&detail);
                format!("↩ T3 rolled back  {}", self.rollback)
            }
            Event::Complete => {
                self.complete = true;
                return None;
            }
            Event::Exit { code, error } => {
                if let Some(error) = error {
                    self.last.insert(String::new(), error);
                }
                let ok = code == Some(0) && self.complete;
                self.status = if ok { Status::Ok } else { Status::Failed };
                self.elapsed = self.started.elapsed();
                if !ok && self.error.is_empty() {
                    self.error = self.last.get("").cloned().unwrap_or_else(|| "Failed".into());
                }
                return None;
            }
        };
        self.push(line.clone());
        Some(line)
    }

    fn finish(&mut self, state: StepState, detail: &str) -> String {
        let step = if detail.starts_with("Health") { "Health" } else { detail.split(':').next().unwrap_or(detail) };
        let step = step.to_string();
        let value = match state {
            StepState::Fail => self.last.get(&step).cloned().unwrap_or_else(|| "Failed".into()),
            StepState::Done if step == "Health" => "Online".into(),
            _ => version_text(detail),
        };
        if state == StepState::Fail {
            self.error = format!("{step}: {value}");
        }
        if is_component(&step) {
            if state == StepState::Skip {
                self.installed.remove(&step);
            } else {
                self.installed.insert(step.clone());
            }
        }
        if state == StepState::Done
            && let Some(found) = RAW_VERSION.find_iter(detail).last()
        {
            self.current.insert(step.clone(), found.as_str().to_string());
        }
        let mark = match state {
            StepState::Done => "✓",
            StepState::Skip => "–",
            _ => "✗",
        };
        let line = format!("{mark} {step}  {value}");
        self.steps.insert(step, (state, value));
        line
    }

    /// Whether the components picked for this run include `name` ('all' picks every one).
    pub fn picked(&self, name: &str) -> bool {
        self.only.split(',').any(|o| o == "all" || o == name.to_lowercase())
    }
}

/// '0.0.46-nightly.20261003.2632' -> [0, 0, 46, 20261003, 2632], to compare versions.
pub fn numbers(version: &str) -> Vec<u64> {
    DIGITS.find_iter(version).filter_map(|m| m.as_str().parse().ok()).collect()
}

/// Whether `a` is a newer version than `b`.
pub fn newer(a: &str, b: &str) -> bool {
    numbers(a) > numbers(b)
}

/// '0.0.46-nightly.20261003.2623' -> '0.0.46-nightly': date and build number are noise.
pub fn short(version: &str) -> String {
    NIGHTLY_TAIL.replace(version, "$1").into_owned()
}

/// Strip ANSI escapes and control characters, then surrounding whitespace.
pub fn clean(text: &str) -> String {
    let text = ANSI.replace_all(text, "");
    text.chars().filter(|&c| c >= ' ' || c == '\t').collect::<String>().trim().to_string()
}

/// 'Codex: codex-cli 1.0.0 -> codex-cli 1.1.0' -> '1.0.0 → 1.1.0'; unchanged -> '1.0.0'.
pub fn version_text(detail: &str) -> String {
    let versions: Vec<&str> = RAW_VERSION.find_iter(detail).map(|m| m.as_str()).collect();
    if versions.len() == 1 && detail.contains(": none -> ") {
        return format!("new → {}", short(versions[0]));
    }
    if versions.len() < 2 {
        return match versions.first() {
            Some(v) => short(v),
            None => detail.split_once(": ").map_or(detail, |(_, rest)| rest).to_string(),
        };
    }
    let (old, new) = (versions[0], versions[versions.len() - 1]);
    if old == new {
        return short(new);
    }
    let (Some(a), Some(b)) = (NIGHTLY.captures(old), NIGHTLY.captures(new)) else {
        return format!("{} → {}", short(old), short(new));
    };
    if short(old) != short(new) {
        return format!("{} → {}", short(old), short(new));
    }
    // Same version, different nightly: the digits from the first one that changed, '…23 → …32'.
    let (x, y) = if a[3] != b[3] { (&a[3], &b[3]) } else { (&a[2], &b[2]) };
    let same = if x.len() == y.len() { x.bytes().zip(y.bytes()).take_while(|(p, q)| p == q).count() } else { 0 };
    format!("{} (…{} → …{})", &a[1], &x[same..], &y[same..])
}

/// How a tile names an available version: '0.0.47', or '#2632' for a new build of the same nightly.
pub fn compact(new: &str, current: &str) -> String {
    if short(new) == short(current) && new != current {
        return format!("#{}", new.rsplit('.').next().unwrap_or(new));
    }
    short(new).replace("-nightly", "")
}

/// {tool: newer version} for what `h` last reported, in card order. A pinned T3 `target` counts
/// as newer whenever it differs. `latest` comes from `registry::fetch_latest`.
pub fn updates(h: &Host, latest: &HashMap<String, String>, target: &str) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = h
        .current
        .iter()
        .filter_map(|(name, current)| {
            let key = if name == "OpenCode" && current.starts_with("2.") { "OpenCode 2" } else { name.as_str() };
            let new = if name == "T3" && !target.is_empty() {
                (target != current).then(|| target.to_string())
            } else {
                latest.get(key).filter(|new| newer(new, current)).cloned()
            };
            new.map(|new| (name.clone(), new))
        })
        .collect();
    found.sort_by_key(|(name, _)| component_index(name));
    found
}

/// The newest T3 a server runs, if this machine's desktop app is older: update the app. '' if not.
pub fn desktop_behind(desktop: &str, hosts: &[&Host]) -> String {
    let newest = hosts.iter().filter_map(|h| h.current.get("T3")).max_by(|a, b| numbers(a).cmp(&numbers(b)));
    match newest {
        Some(n) if !desktop.is_empty() && newer(n, desktop) => n.clone(),
        _ => String::new(),
    }
}

/// A newer T3 than this machine's desktop app: what a server runs or, when no version is pinned,
/// the newest nightly. '' when the app is up to date or not installed.
pub fn desktop_outdated(desktop: &str, hosts: &[&Host], latest: &HashMap<String, String>, target: &str) -> String {
    let mut best = desktop_behind(desktop, hosts);
    if let Some(l) = latest.get("T3")
        && !desktop.is_empty()
        && target.is_empty()
        && newer(l, desktop)
        && (best.is_empty() || newer(l, &best))
    {
        best = l.clone();
    }
    best
}

/// Every tool's full version, which tiles shorten, and the full version of any update.
pub fn versions_detail(h: &Host, new: &[(String, String)]) -> Vec<String> {
    let mut names: Vec<&String> = h.current.keys().collect();
    names.sort_by_key(|n| component_index(n));
    let width = names.iter().map(|n| n.len()).max().unwrap_or(0);
    names
        .into_iter()
        .map(|name| {
            let mut line = format!("{name:<width$}  {}", h.current[name]);
            if let Some((_, v)) = new.iter().find(|(n, _)| n == name) {
                line += &format!("  →  {v}");
            }
            line
        })
        .collect()
}

/// 'codex,claude' -> Ok(itself) if every name is a component (or 'all').
pub fn parse_only(value: &str) -> Result<String, String> {
    let ok = value.split(',').all(|v| v == "all" || COMPONENTS.iter().any(|(n, _)| n.to_lowercase() == v));
    if ok {
        Ok(value.to_string())
    } else {
        let names: Vec<String> = COMPONENTS.iter().map(|(n, _)| n.to_lowercase()).collect();
        Err(format!("choose from all, {}", names.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_text_cases() {
        assert_eq!(version_text("Codex: codex-cli 0.160.0 -> codex-cli 0.161.0"), "0.160.0 → 0.161.0");
        assert_eq!(version_text("Claude: 2.1.288 (Claude Code) -> 2.1.288 (Claude Code)"), "2.1.288");
        let n = "T3: t3 0.0.46-nightly.20261003.";
        assert_eq!(
            version_text(&format!("{n}2623 -> t3 0.0.47-nightly.20261004.2701")),
            "0.0.46-nightly → 0.0.47-nightly"
        );
        assert_eq!(version_text(&format!("{n}2623 -> t3 0.0.46-nightly.20261003.2633")), "0.0.46-nightly (…23 → …33)");
        assert_eq!(version_text(&format!("{n}2623 -> t3 0.0.46-nightly.20261004.2624")), "0.0.46-nightly (…3 → …4)");
        assert_eq!(version_text(&format!("{n}2623")), "0.0.46-nightly");
        assert_eq!(version_text(&format!("{n}2623 -> t3 0.0.46-nightly.20261003.2632")), "0.0.46-nightly (…23 → …32)");
        assert_eq!(version_text("Codex: none -> codex-cli 0.2.0"), "new → 0.2.0");
        assert_eq!(version_text("Pi: not installed"), "not installed");
    }

    #[test]
    fn compact_and_short() {
        assert_eq!(compact("0.0.46-nightly.20261003.2632", "0.0.46-nightly.20261003.2623"), "#2632");
        assert_eq!(compact("0.0.47-nightly.20261004.2701", "0.0.46-nightly.20261003.2623"), "0.0.47");
        assert_eq!(short("0.0.46-nightly.20261003.2623"), "0.0.46-nightly");
        assert_eq!(clean("\x1b[32m ok \x1b[0m\x07"), "ok");
    }

    fn box_host() -> Host {
        let mut h = Host::new("box");
        for (k, v) in [("Codex", "1.2.0"), ("T3", "0.0.46-nightly.20261003.2632"), ("OpenCode", "2.0.1")] {
            h.current.insert(k.into(), v.into());
        }
        h
    }

    #[test]
    fn updates_and_desktop() {
        let h = box_host();
        let latest: HashMap<String, String> = [
            ("Codex", "1.2.0"),
            ("T3", "0.0.46-nightly.20261003.2632"),
            ("OpenCode", "1.18.34"),
            ("OpenCode 2", "2.0.22"),
        ]
        .into_iter()
        .map(|(a, b)| (a.into(), b.into()))
        .collect();
        assert_eq!(updates(&h, &latest, ""), vec![("OpenCode".into(), "2.0.22".into())]);
        assert_eq!(
            updates(&h, &latest, "0.0.45"),
            vec![("T3".into(), "0.0.45".into()), ("OpenCode".into(), "2.0.22".into())]
        );
        assert_eq!(desktop_behind("0.0.46-nightly.20261003.2623", &[&h]), "0.0.46-nightly.20261003.2632");
        assert_eq!(desktop_behind("0.0.46-nightly.20261003.2632", &[&h]), "");
        assert_eq!(desktop_behind("", &[&h]), "");
        let newest: HashMap<String, String> = [("T3".into(), "0.0.46-nightly.20261003.2640".into())].into();
        assert_eq!(
            desktop_outdated("0.0.46-nightly.20261003.2632", &[&h], &newest, ""),
            "0.0.46-nightly.20261003.2640"
        );
        assert_eq!(desktop_outdated("0.0.46-nightly.20261003.2632", &[&h], &newest, "0.0.45"), "");
        assert_eq!(
            desktop_outdated("0.0.46-nightly.20261003.2623", &[&h], &HashMap::new(), ""),
            "0.0.46-nightly.20261003.2632"
        );
        assert_eq!(
            versions_detail(&h, &[("OpenCode".into(), "2.0.22".into())]),
            vec!["T3        0.0.46-nightly.20261003.2632", "Codex     1.2.0", "OpenCode  2.0.1  →  2.0.22"]
        );
    }

    #[test]
    fn apply_events() {
        let mut h = Host::new("box");
        h.reset(Mode::Update, "codex");
        h.apply(Event::Begin("Codex".into()));
        assert_eq!(
            h.apply(Event::Output { tag: "Codex".into(), text: "added 1 package".into() }).unwrap(),
            "Codex: added 1 package"
        );
        h.apply(Event::Done("Codex: codex-cli 1.0.0 -> codex-cli 1.1.0".into()));
        assert_eq!(h.steps["Codex"], (StepState::Done, "1.0.0 → 1.1.0".into()));
        assert_eq!(h.current["Codex"], "1.1.0");
        assert!(h.installed.contains("Codex"));
        h.apply(Event::Skip("Pi: not installed".into()));
        assert!(!h.installed.contains("Pi"));
        h.apply(Event::Output { tag: "Claude".into(), text: "disk full".into() });
        h.apply(Event::Fail("Claude".into()));
        assert_eq!(h.error, "Claude: disk full");
        h.apply(Event::Done("Health: OK".into()));
        assert_eq!(h.steps["Health"].1, "Online");
        h.apply(Event::Exit { code: Some(1), error: None });
        assert_eq!(h.status, Status::Failed);
        assert_eq!(h.error, "Claude: disk full");

        let mut h = Host::new("box");
        h.reset(Mode::Check, "all");
        h.apply(Event::Complete);
        h.apply(Event::Exit { code: Some(0), error: None });
        assert_eq!(h.status, Status::Ok);
        h.reset(Mode::Check, "all");
        h.apply(Event::Output { tag: String::new(), text: "Connection refused".into() });
        h.apply(Event::Exit { code: Some(255), error: None });
        assert_eq!((h.status, h.error.as_str()), (Status::Failed, "Connection refused"));
        assert!(h.picked("t3"));
        h.only = "codex".into();
        assert!(h.picked("Codex") && !h.picked("T3"));
    }

    #[test]
    fn only_values() {
        assert!(parse_only("codex,claude").is_ok());
        assert!(parse_only("all,pi").is_ok());
        assert!(parse_only("cursor").is_err());
    }
}
