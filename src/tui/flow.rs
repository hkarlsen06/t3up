//! Sign-ins and pairing, shown in the dashboard instead of a raw terminal. A flow runs one remote
//! command over `ssh -tt`; its output is read for what you need (a link, a code, a prompt) and the
//! modal shows just that, with copy and open, and an input box when the tool asks for something.
use std::sync::LazyLock;
use std::time::Instant;

use regex::Regex;

use super::app::Act;
use super::input::Input;
use crate::model::{PATH_SETUP, pair_command};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `t3 pair`: a QR code, link and token, then it exits.
    Pair,
    /// `codex login --device-auth`: a link and a one-time code; finishes when you approve in the browser.
    Codex,
    /// `claude auth login`: a link; the page gives you a code to paste back.
    Claude,
    /// Anything else that signs in: its output, any link, and an input box.
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub enum State {
    Running,
    /// Finished well: signed in, or the pairing link is ready.
    Done,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct Flow {
    pub id: u64,
    pub host: String,
    /// The tool it signs in to ('Claude'), or 'T3' for pairing.
    pub tool: String,
    pub kind: Kind,
    /// Everything it printed, without escapes.
    pub output: String,
    pub url: Option<String>,
    /// Codex's one-time code, or the pairing token.
    pub code: Option<String>,
    /// When a pairing link expires (as printed: an ISO time).
    pub expires: Option<String>,
    /// Pairing went over Tailscale (else it only works on the local network).
    pub tailscale: bool,
    /// The tool is waiting for you to type something.
    pub prompt: bool,
    pub input: Input,
    /// Times a code was sent; a prompt after one means it was wrong.
    pub sent: usize,
    pub state: State,
    pub started: Instant,
    pub finished: Option<Instant>,
    /// The last copy button pressed, and when: it says "Copied" for a moment.
    pub copied: Option<(Act, Instant)>,
}

static ANSI: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[()][A-Z0-9]").unwrap()
});
static URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://[^\s<>]+").unwrap());
static PAIR_URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"Pairing URL:\s*(\S+)").unwrap());
static TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"Token:\s*(\S+)").unwrap());
static EXPIRES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"Expires:\s*(\S+)").unwrap());
static DEVICE_CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b([A-Z0-9]{4}-[A-Z0-9]{4,6})\b").unwrap());

/// Text without terminal escapes and carriage returns.
pub fn plain(text: &str) -> String {
    ANSI.replace_all(text, "").replace('\r', "")
}

impl Flow {
    pub fn new(id: u64, host: &str, tool: &str) -> Self {
        let kind = match tool {
            "T3" => Kind::Pair,
            "Codex" => Kind::Codex,
            "Claude" => Kind::Claude,
            _ => Kind::Other,
        };
        Flow {
            id,
            host: host.into(),
            tool: tool.into(),
            kind,
            output: String::new(),
            url: None,
            code: None,
            expires: None,
            tailscale: false,
            prompt: false,
            input: Input::default(),
            sent: 0,
            state: State::Running,
            started: Instant::now(),
            finished: None,
            copied: None,
        }
    }

    /// The remote command: wide enough that a long link never wraps, then the tool.
    pub fn command(&self) -> String {
        let tool = match self.kind {
            Kind::Pair => pair_command(false),
            Kind::Codex => "codex login --device-auth".into(),
            Kind::Claude => "claude auth login".into(),
            Kind::Other => format!("{} login", self.tool.to_lowercase()),
        };
        format!("{PATH_SETUP}; stty cols 4000 2>/dev/null; {tool}")
    }

    /// The title of its modal.
    pub fn title(&self) -> String {
        match self.kind {
            Kind::Pair => format!("Pair a device with {}", self.host),
            _ => format!("Sign in to {} on {}", self.tool, self.host),
        }
    }

    /// Read more of what it printed.
    pub fn feed(&mut self, chunk: &str) {
        self.output.push_str(&plain(chunk));
        let out = &self.output;
        match self.kind {
            Kind::Pair => {
                self.url = PAIR_URL.captures(out).map(|c| c[1].to_string());
                self.code = TOKEN.captures(out).map(|c| c[1].to_string());
                self.expires = EXPIRES.captures(out).map(|c| c[1].to_string());
                self.tailscale = out.contains("@@pair tailscale");
            }
            Kind::Codex => {
                self.url = URL.find(out).map(|m| m.as_str().to_string());
                let after = out.find("one-time code").map_or("", |i| &out[i..]);
                self.code = DEVICE_CODE.captures(after).map(|c| c[1].to_string());
            }
            Kind::Claude | Kind::Other => {
                self.url = URL.find(out).map(|m| m.as_str().trim_end_matches(['.', ',', ')']).to_string());
            }
        }
        // Waiting for input: the last thing printed is a prompt, not a finished line.
        let tail = out.trim_end_matches([' ', '\t']);
        self.prompt = match self.kind {
            Kind::Claude => tail.ends_with('>') || tail.to_lowercase().ends_with("paste code here if prompted"),
            Kind::Other => !tail.ends_with('\n') && (tail.ends_with(':') || tail.ends_with('>') || tail.ends_with('?')),
            _ => false,
        };
    }

    /// The command ended with `code`.
    pub fn exit(&mut self, code: Option<i32>) {
        self.prompt = false;
        self.finished = Some(Instant::now());
        self.state = match (code, self.kind) {
            (Some(0), Kind::Pair) if self.url.is_none() => State::Failed(self.last_words()),
            (Some(0), _) => State::Done,
            _ => State::Failed(self.last_words()),
        };
    }

    /// The last line it printed that says something, for a failure message.
    pub fn last_words(&self) -> String {
        self.output
            .lines()
            .map(str::trim)
            .rfind(|l| !l.is_empty() && !l.starts_with("Connection to ") && !l.starts_with("@@"))
            .unwrap_or("It stopped without saying why")
            .to_string()
    }

    /// The last few lines of output, for tools t3up doesn't know.
    pub fn tail(&self, n: usize) -> Vec<String> {
        let lines: Vec<&str> =
            self.output.lines().map(str::trim_end).filter(|l| !l.is_empty() && !l.starts_with("@@")).collect();
        lines[lines.len().saturating_sub(n)..].iter().map(|l| l.to_string()).collect()
    }

    /// The copy button that should say "Copied" right now.
    pub fn copied_now(&self) -> Option<Act> {
        self.copied.filter(|(_, at)| at.elapsed().as_millis() < 1600).map(|(act, _)| act)
    }

    /// A wrong code: the tool asked again after one was sent.
    pub fn retry(&self) -> bool {
        self.prompt && self.sent > 0
    }
}

/// A QR code for `text` as rows of dark (true) and light modules, with a quiet zone of `quiet`.
pub fn qr(text: &str, quiet: usize) -> Option<Vec<Vec<bool>>> {
    use qrcode::{Color, EcLevel, QrCode};
    let code = QrCode::with_error_correction_level(text.as_bytes(), EcLevel::L).ok()?;
    let width = code.width();
    let colors = code.to_colors();
    let size = width + 2 * quiet;
    let mut rows = vec![vec![false; size]; size];
    for (i, color) in colors.iter().enumerate() {
        rows[i / width + quiet][i % width + quiet] = *color == Color::Dark;
    }
    Some(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODEX: &str = "\r\nWelcome to Codex [v0.160.0]\r\n\x1b[90mOpenAI's command-line coding agent\x1b[0m\r\n\r\n\
        Follow these steps to sign in with ChatGPT using device code authorization:\r\n\r\n\
        1. Open this link in your browser and sign in to your account\r\n   \x1b[94mhttps://auth.openai.com/codex/device\x1b[0m\r\n\r\n\
        2. Enter this one-time code \x1b[90m(expires in 15 minutes)\x1b[0m\r\n   \x1b[94mAB12-CD345\x1b[0m\r\n";

    #[test]
    fn codex_link_and_code() {
        let mut f = Flow::new(1, "one-s", "Codex");
        f.feed(&CODEX[..60]);
        assert_eq!(f.code, None);
        f.feed(&CODEX[60..]);
        assert_eq!(f.url.as_deref(), Some("https://auth.openai.com/codex/device"));
        assert_eq!(f.code.as_deref(), Some("AB12-CD345"));
        assert!(!f.prompt);
        f.exit(Some(0));
        assert_eq!(f.state, State::Done);
    }

    #[test]
    fn claude_link_then_prompt() {
        let mut f = Flow::new(2, "one-s", "Claude");
        f.feed("Opening browser to sign in…\r\nIf the browser didn't open, visit: \x1b]8;;https://x\x07");
        f.feed("https://claude.com/cai/oauth/authorize?code=true&client_id=abc&state=xyz\x1b]8;;\x07\r\n");
        assert!(!f.prompt);
        f.feed("Paste code here if prompted > ");
        assert_eq!(f.url.as_deref(), Some("https://claude.com/cai/oauth/authorize?code=true&client_id=abc&state=xyz"));
        assert!(f.prompt && !f.retry());
        f.sent = 1;
        f.feed("bad\r\nInvalid code. Paste code here if prompted > ");
        assert!(f.retry());
        f.exit(Some(1));
        assert!(matches!(f.state, State::Failed(_)));
    }

    #[test]
    fn pairing_link_token_and_qr() {
        let mut f = Flow::new(3, "one-s", "T3");
        f.feed("@@pair tailscale\r\nPairing with one-s (https://one-s.tail1.ts.net).\r\n\r\n  █▀▀▀▀▀█ ▀▄▄\r\n\r\n");
        f.feed("Pairing URL: https://one-s.tail1.ts.net/pair#token=ABCDEF\r\nToken: ABCDEF\r\nExpires: 2026-10-04T17:10:49.649Z\r\n");
        f.exit(Some(0));
        assert_eq!(f.url.as_deref(), Some("https://one-s.tail1.ts.net/pair#token=ABCDEF"));
        assert_eq!((f.code.as_deref(), f.expires.as_deref()), (Some("ABCDEF"), Some("2026-10-04T17:10:49.649Z")));
        assert!(f.tailscale);
        assert_eq!(f.state, State::Done);
        let q = qr(f.url.as_deref().unwrap(), 1).unwrap();
        assert!(q.len() >= 23 && q.iter().all(|r| r.len() == q.len()) && !q[0][0] && q[1][1], "dark finder corner");
        // No link printed is a failure, with what it said.
        let mut f = Flow::new(4, "x", "T3");
        f.feed("@@pair local\r\nCould not reach the server\r\nConnection to x closed.\r\n");
        f.exit(Some(0));
        assert_eq!(f.state, State::Failed("Could not reach the server".into()));
    }

    #[test]
    fn other_tools_prompt_on_a_trailing_colon() {
        let mut f = Flow::new(5, "box", "Grok");
        f.feed("Visit https://x.ai/device to continue.\r\nEnter code:");
        assert_eq!(f.url.as_deref(), Some("https://x.ai/device"));
        assert!(f.prompt);
        assert_eq!(f.tail(1), ["Enter code:"]);
    }
}
