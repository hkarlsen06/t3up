//! All environment changes stay in this one test; SSH always runs the script under dash.
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};
use t3up::{
    job::{self, Job},
    model::{self, Event, Host, Mode, Status, StepState},
};

struct Sandbox(PathBuf);
impl Sandbox {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "t3up-remote-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(fs::canonicalize(path).unwrap())
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn tool(home: &Path, path: &str, body: &str) {
    let path = home.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, format!("#!/bin/dash\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}
fn remove(path: &Path) {
    if path.exists() || path.is_symlink() {
        fs::remove_file(path).unwrap();
    }
}
fn link(home: &Path, name: &str, path: &str) {
    let target = home.join(format!(".local/bin/{name}"));
    remove(&target);
    symlink(home.join(path), target).unwrap();
}
const CODEX: &str = ".local/lib/node_modules/@openai/codex/bin/codex.js";
const PI: &str = ".local/lib/node_modules/@earendil-works/pi-coding-agent/dist/cli.js";
const CLAUDE: &str = ".local/share/claude/versions/1/claude";
// Where the official installers put Codex and Pi: their updaters own these.
const CODEX_STANDALONE: &str = ".codex/packages/standalone/current/codex";
const PI_MANAGED: &str = ".pi/agent/install/bin/pi";

fn reset(home: &Path) {
    for (name, version) in
        [("codex", "0.1.0"), ("claude", "1.0.0"), ("grok", "0.3.0"), ("pi", "0.5.0"), ("t3", "0.0.1")]
    {
        fs::write(home.join(name), version).unwrap();
    }
    for name in [
        "t3-calls",
        "claude-broken",
        "npm-calls",
        ".local/bin/pi",
        ".local/share/pnpm/pi",
        "bad-health",
        "fast-health",
        "ps-output",
        "update-calls",
        "rollback-broken",
        "system-service",
        "always-bad-health",
        "codex-noninteractive",
        "curl-calls",
        ".local/bin/node",
    ] {
        remove(&home.join(name));
    }
    for (name, path) in [("codex", CODEX), ("claude", CLAUDE)] {
        fs::write(home.join(format!("{name}-auth")), "").unwrap();
        link(home, name, path);
    }
    fs::write(home.join("grok-auth"), "").unwrap();
}
async fn remote(home: &Path, mode: Mode, only: &str) -> (Host, Vec<Event>) {
    let script = model::remote_script().replace(":/usr/local/bin:/opt/homebrew/bin:", ":");
    assert!(!script.contains("/usr/local/bin") && !script.contains("/opt/homebrew/bin"));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    job::run_job(
        Job {
            host: "box".into(),
            mode,
            version: "".into(),
            only: only.into(),
            logs: home.join("logs"),
            script: Arc::from(script),
        },
        tx,
    )
    .await;
    let mut h = Host::new("box");
    h.reset(mode, only);
    let mut events = vec![];
    while let Some((_, e)) = rx.recv().await {
        h.apply(e.clone());
        events.push(e);
    }
    assert!(matches!(events.first(), Some(Event::Start { .. })));
    assert!(matches!(events.last(), Some(Event::Exit { .. })));
    (h, events)
}
fn value(h: &Host, step: &str) -> String {
    h.steps[step].1.clone()
}
fn ok(h: &Host) {
    assert_eq!(h.status, Status::Ok, "{}: {:?}", h.error, h.lines);
}

#[tokio::test]
async fn remote_suite() {
    let root = Sandbox::new();
    let home = root.0.join("home");
    fs::create_dir_all(home.join(".local/bin")).unwrap();
    tool(
        &root.0,
        "ssh",
        r#"while [ "$1" = -o ]; do shift 2; done
shift
command=$1
PATH=/usr/bin:/bin; export PATH
eval "set -- $command"
shift
exec /bin/dash "$@""#,
    );
    // This binary has a single test and no other environment users.
    unsafe {
        std::env::set_var("T3UP_SSH", root.0.join("ssh"));
        std::env::set_var("HOME", &home);
    }
    tool(
        &home,
        CODEX,
        r#"case $1 in --version) echo "codex-cli $(cat "$HOME/codex")";; login) [ -f "$HOME/codex-auth" ];; esac"#,
    );
    tool(&home, PI, r#"cat "$HOME/pi""#);
    tool(
        &home,
        CLAUDE,
        r#"case $1 in
--version) echo "$(cat "$HOME/claude") (Claude Code)";;
update) sleep 1; [ -f "$HOME/claude-broken" ] && { echo 'disk full' >&2; exit 3; }; echo 2.0.0 > "$HOME/claude";;
auth) [ -f "$HOME/claude-auth" ] && echo '{ "loggedIn": true }' || echo '{ "loggedIn": false }';;
esac"#,
    );
    tool(
        &home,
        ".local/bin/grok",
        r#"case $1 in --version) echo "grok $(cat "$HOME/grok")";; update) echo 0.4.0 > "$HOME/grok";; models) [ -f "$HOME/grok-auth" ] && echo 'You are logged in with grok.com.' || echo 'Not logged in';; esac"#,
    );
    tool(
        &home,
        ".local/bin/npm",
        &format!(
            r#"[ "$1" != prefix ] || {{ echo "$HOME/.local"; exit; }}
echo "$*" >> "$HOME/npm-calls"
mkdir "$HOME/npm-active" || {{ echo 'overlapping npm installs' >&2; exit 1; }}
trap 'rmdir "$HOME/npm-active"' EXIT
sleep 1
head -c 100000 /dev/zero | tr '\0' x; echo; echo 'added 1 package'
case "$*" in
uninstall*@openai/codex*) rm -f "$HOME/.local/bin/codex";;
uninstall*pi-coding-agent*) rm -f "$HOME/.local/bin/pi";;
*@openai/codex@*) echo 0.2.0 > "$HOME/codex"; ln -sf "$HOME/{CODEX}" "$HOME/.local/bin/codex";;
*pi-coding-agent@*) echo 0.6.0 > "$HOME/pi"; ln -sf "$HOME/{PI}" "$HOME/.local/bin/pi";;
*t3@nightly*) echo 0.0.2 > "$HOME/t3";;
*t3@*) echo "${{*##*t3@}}" > "$HOME/t3";;
esac"#
        ),
    );
    tool(
        &home,
        ".local/bin/t3",
        r#"case $1 in
--version) echo x >> "$HOME/t3-calls"; echo "t3 v$(cat "$HOME/t3")";;
update) echo "$*" >> "$HOME/update-calls"; sleep 1
  if [ "${4:-}" = 0.0.1 ]; then
    [ ! -f "$HOME/rollback-broken" ] || { echo 'cannot reinstall old T3' >&2; exit 3; }
    echo 0.0.1 > "$HOME/t3"
  else echo 0.0.2 > "$HOME/t3"; fi;;
service) echo "$*" >> "$HOME/service-calls";;
esac"#,
    );
    // curl serves each official installer (as a fake that installs where the real one does), and
    // nodejs.org's checksums and archive from $HOME/node-dist.
    tool(
        &home,
        ".local/bin/curl",
        &format!(
            r#"url= out=
while [ $# -gt 0 ]; do case $1 in -o) out=$2; shift 2 ;; --max-time) shift 2 ;; -*) shift ;; *) url=$1; shift ;; esac; done
case $url in *localhost*|*127.0.0.1*)
  [ ! -f "$HOME/always-bad-health" ] || exit 1
  [ ! -f "$HOME/bad-health" ] || [ "$(cat "$HOME/t3")" = 0.0.1 ]; exit $? ;;
esac
echo "$url" >> "$HOME/curl-calls"
case $url in
*t3.codes/install.sh) printf '%s\n' 'echo "${{T3CODE_CHANNEL:-}}${{T3CODE_VERSION:-}}" > "$HOME/t3-installed"' \
    'cp "$HOME/t3-shelved" "$HOME/.local/bin/t3"; echo 0.0.3 > "$HOME/t3"' > "$out" ;;
*chatgpt.com/codex/install.sh) printf '%s\n' 'echo "$NON_INTERACTIVE" > "$HOME/codex-noninteractive"' \
    'ln -sf "$HOME/{CODEX_STANDALONE}" "$HOME/.local/bin/codex"; echo 0.2.0 > "$HOME/codex"' > "$out" ;;
*pi.dev/install.sh) printf '%s\n' 'command -v npm >/dev/null && node -e 1 || {{ echo "Node.js 22.19.0 or newer is required" >&2; exit 1; }}' \
    'ln -sf "$HOME/{PI_MANAGED}" "$HOME/.local/bin/pi"; echo 0.6.0 > "$HOME/pi"' > "$out" ;;
*/SHASUMS256.txt) cp "$HOME/node-dist/SHASUMS256.txt" "$out" ;;
*.tar.gz) cp "$HOME/node-dist/${{url##*/}}" "$out" ;;
*) printf '%s\n' 'ln -sf "$HOME/{CLAUDE}" "$HOME/.local/bin/claude"' 'echo 2.0.0 > "$HOME/claude"' > "$out" ;;
esac"#
        ),
    );
    tool(
        &home,
        CODEX_STANDALONE,
        r#"case $1 in --version) echo "codex-cli $(cat "$HOME/codex")";; login) [ -f "$HOME/codex-auth" ];; esac"#,
    );
    tool(&home, PI_MANAGED, r#"cat "$HOME/pi""#);
    tool(
        &home,
        ".local/bin/systemctl",
        r#"case "$*" in *t3code.service*) [ -f "$HOME/user-service" ];; *) [ -f "$HOME/system-service" ];; esac"#,
    );
    tool(
        &home,
        ".local/bin/loginctl",
        r#"case $1 in show-user) echo "Linger=$(cat "$HOME/linger" 2>/dev/null || echo no)";; enable-linger) echo yes > "$HOME/linger";; esac"#,
    );
    tool(&home, ".local/bin/sudo", r#"shift; exec "$@""#);
    tool(&home, ".local/bin/ps", r#"cat "$HOME/ps-output" 2>/dev/null"#);
    tool(
        &home,
        ".local/bin/sleep",
        r#"[ ! -f "$HOME/fast-health" ] || exit 0
exec /bin/sleep "$@""#,
    );

    reset(&home);
    let (h, events) = remote(&home, Mode::Check, "all").await;
    ok(&h);
    assert_eq!(fs::read_to_string(home.join("t3-calls")).unwrap().matches('x').count(), 1);
    assert_eq!(
        [value(&h, "Codex"), value(&h, "Claude"), value(&h, "Grok"), value(&h, "T3")],
        ["0.1.0", "1.0.0", "0.3.0", "0.0.1"]
    );
    assert_eq!(h.steps["OpenCode"], (StepState::Skip, "not installed".into()));
    assert_eq!(h.steps["Pi"], h.steps["OpenCode"]);
    assert_eq!(h.busy, Some(0));
    assert!(events.iter().any(|e| matches!(e, Event::Sys(s) if s.contains("disk ") && s.contains("cpus "))));

    reset(&home);
    let started = Instant::now();
    let (h, _) = remote(&home, Mode::Update, "all").await;
    ok(&h);
    assert!(started.elapsed().as_secs_f64() < 2.5, "components must run in parallel");
    assert_eq!(h.steps["Pi"].0, StepState::Skip);
    assert_eq!(
        [value(&h, "Codex"), value(&h, "Claude"), value(&h, "Grok"), value(&h, "T3")],
        ["0.1.0 → 0.2.0", "1.0.0 → 2.0.0", "0.3.0 → 0.4.0", "0.0.1 → 0.0.2"]
    );
    assert_eq!(
        fs::read_to_string(home.join("npm-calls")).unwrap(),
        format!("install -g --prefix {} @openai/codex@latest\n", home.join(".local").display())
    );
    assert!(h.lines.iter().any(|l| l == "Codex: added 1 package"));
    assert!(h.lines.iter().all(|l| l.len() < 4000));

    reset(&home);
    let (h, _) = remote(&home, Mode::Update, "codex").await;
    ok(&h);
    assert_eq!([value(&h, "Codex"), value(&h, "Claude"), value(&h, "T3")], ["0.1.0 → 0.2.0", "1.0.0", "0.0.1"]);

    reset(&home);
    fs::write(home.join("claude-broken"), "").unwrap();
    let (h, _) = remote(&home, Mode::Update, "all").await;
    assert_eq!(h.status, Status::Failed);
    assert_eq!(h.steps["Claude"], (StepState::Fail, "disk full".into()));
    assert_eq!(h.steps["Health"].0, StepState::Done, "provider failure must not hide T3 health");
    assert_eq!(h.steps["Codex"].0, StepState::Done);
    assert_eq!(h.steps["T3"].0, StepState::Done);
    assert_eq!(h.error, "Claude: disk full");

    reset(&home);
    for name in ["codex", "claude"] {
        remove(&home.join(format!(".local/bin/{name}")));
    }
    let (h, _) = remote(&home, Mode::Check, "all").await;
    ok(&h);
    assert_eq!(h.steps["Codex"].0, StepState::Skip);
    assert_eq!(h.steps["Claude"].0, StepState::Skip);

    // No T3 at all: a check says so instead of failing, and runs no health check.
    reset(&home);
    fs::rename(home.join(".local/bin/t3"), home.join("t3-shelved")).unwrap();
    let (h, _) = remote(&home, Mode::Check, "all").await;
    ok(&h);
    assert_eq!(h.steps["T3"], (StepState::Skip, "not installed".into()));
    assert!(!h.steps.contains_key("Health"), "no T3, nothing to answer on 3773");
    // Updating it installs T3 the official way, on the nightly train, as a service with lingering,
    // then asks to pair the new server.
    let (h, _) = remote(&home, Mode::Update, "t3").await;
    ok(&h);
    assert_eq!(value(&h, "T3"), "new → 0.0.3");
    assert_eq!(fs::read_to_string(home.join("t3-installed")).unwrap(), "nightly\n");
    assert_eq!(fs::read_to_string(home.join("service-calls")).unwrap(), "service install\n");
    assert_eq!(fs::read_to_string(home.join("linger")).unwrap(), "yes\n");
    assert_eq!(h.auth, ["T3"]);
    assert_eq!(h.steps["Health"].0, StepState::Done);
    for name in ["t3-installed", "service-calls", "linger"] {
        remove(&home.join(name));
    }
    // A service-managed T3 is found from its state file without node (the installer brings none).
    reset(&home);
    tool(&home, ".t3/runtime/versions/0.0.9/t3", r#"echo "t3 v0.0.9""#);
    fs::write(
        home.join(".t3/runtime/service-state.json"),
        "{\n  \"protocol\": 3,\n  \"activeVersion\": \"0.0.9\"\n}\n",
    )
    .unwrap();
    fs::write(home.join("user-service"), "").unwrap();
    let (h, _) = remote(&home, Mode::Check, "all").await;
    ok(&h);
    assert_eq!(value(&h, "T3"), "0.0.9");
    fs::remove_dir_all(home.join(".t3")).unwrap();
    remove(&home.join("user-service"));
    // Back to where the missing-tools checks below left off.
    reset(&home);
    for name in ["codex", "claude"] {
        remove(&home.join(format!(".local/bin/{name}")));
    }
    // A new machine: every missing tool comes from its own official installer, never npm.
    tool(&home, ".local/bin/node", "exit 0"); // a Node new enough for Pi's installer
    let (h, _) = remote(&home, Mode::Update, "codex,claude,pi").await;
    ok(&h);
    assert_eq!(
        [value(&h, "Codex"), value(&h, "Claude"), value(&h, "Pi")],
        ["new → 0.2.0", "new → 2.0.0", "new → 0.6.0"]
    );
    assert!(h.auth.is_empty());
    assert!(!home.join("npm-calls").exists(), "installers, not npm");
    assert_eq!(fs::read_to_string(home.join("codex-noninteractive")).unwrap(), "1\n");
    // Updating them later goes through their own updaters: Codex's standalone, `pi update`.
    let (h, _) = remote(&home, Mode::Update, "codex,pi").await;
    ok(&h);
    assert!(!home.join("npm-calls").exists());
    remove(&home.join(".local/bin/node"));

    // No Node at all: Pi's installer gets the official build first, checked against its SHA-256.
    let plat = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-arm64",
        ("macos", _) => "darwin-x64",
        (_, "aarch64") => "linux-arm64",
        _ => "linux-x64",
    };
    let name = format!("node-v24.9.0-{plat}");
    let dist = home.join("node-dist");
    tool(&dist, &format!("{name}/bin/node"), "exit 0");
    tool(&dist, &format!("{name}/bin/npm"), "exit 0");
    let archive = format!("{name}.tar.gz");
    assert!(
        std::process::Command::new("tar")
            .args(["-czf", &archive, &name])
            .current_dir(&dist)
            .status()
            .unwrap()
            .success()
    );
    use sha2::Digest;
    let sum: String =
        sha2::Sha256::digest(fs::read(dist.join(&archive)).unwrap()).iter().map(|b| format!("{b:02x}")).collect();
    let sums = format!("{} node-v24.9.0-other.tar.gz\n{sum}  {archive}\n", "0".repeat(64));
    fs::write(dist.join("SHASUMS256.txt"), &sums).unwrap();
    unsafe {
        std::env::set_var("T3UP_NODE_DIST", "https://dist.example/latest-v24.x");
    }
    remove(&home.join(".local/bin/pi"));
    let (h, _) = remote(&home, Mode::Update, "pi").await;
    ok(&h);
    assert_eq!(value(&h, "Pi"), "new → 0.6.0");
    assert!(home.join(".local/share/pi-node/current/bin/node").exists());
    // A download that doesn't match is refused, and Pi isn't installed.
    fs::remove_dir_all(home.join(".local/share/pi-node")).unwrap();
    remove(&home.join(".local/bin/pi"));
    fs::write(dist.join("SHASUMS256.txt"), sums.replace(&sum, &"f".repeat(64))).unwrap();
    let (h, _) = remote(&home, Mode::Update, "pi").await;
    assert_eq!(h.steps["Pi"].0, StepState::Fail);
    assert!(h.error.contains("checksum"), "{}", h.error);
    assert!(!home.join(".local/share/pi-node").join("current").exists());

    // Tools npm installed still update with npm, one install at a time.
    reset(&home);
    link(&home, "pi", PI);
    let started = Instant::now();
    let (h, _) = remote(&home, Mode::Update, "codex,pi").await;
    ok(&h);
    assert!(started.elapsed().as_secs_f64() > 2.0, "npm installs must serialize");
    assert_eq!(fs::read_to_string(home.join("npm-calls")).unwrap().lines().count(), 2);
    remove(&home.join("npm-calls"));
    let (h, _) = remote(&home, Mode::Update, "pi").await;
    ok(&h);
    assert_eq!(
        fs::read_to_string(home.join("npm-calls")).unwrap(),
        format!("install -g --prefix {} @earendil-works/pi-coding-agent@latest\n", home.join(".local").display())
    );
    remove(&home.join(".local/bin/pi"));
    tool(&home, ".local/share/pnpm/pi", "echo 0.5.0");
    tool(&home, ".local/bin/pnpm", r#"echo "$*" >> "$HOME/npm-calls""#);
    remove(&home.join("npm-calls"));
    let (h, _) = remote(&home, Mode::Update, "pi").await;
    ok(&h);
    assert_eq!(fs::read_to_string(home.join("npm-calls")).unwrap(), "add -g @earendil-works/pi-coding-agent@latest\n");
    let (h, _) = remote(&home, Mode::Update, "all,pi").await;
    ok(&h);
    assert_eq!(value(&h, "Grok"), "0.3.0 → 0.4.0");
    remove(&home.join("claude-auth"));
    remove(&home.join("grok-auth"));
    let (mut h, _) = remote(&home, Mode::Check, "all").await;
    ok(&h);
    h.auth.sort();
    assert_eq!(h.auth, ["Claude", "Grok"]);
    remove(&home.join(".local/bin/codex"));
    tool(&home, ".local/bin/codex", "echo 'codex-cli 0.1.0'");
    let (h, _) = remote(&home, Mode::Update, "codex").await;
    assert_eq!(h.steps["Codex"].0, StepState::Fail);
    assert!(h.error.contains("update it by hand"));
    assert_eq!(h.steps["Health"].0, StepState::Done);

    // Removing: the way each was installed, settings and sign-in kept; nothing else changes.
    reset(&home);
    let (h, _) = remote(&home, Mode::Remove, "codex").await; // npm owns this one
    ok(&h);
    assert_eq!(h.steps["Codex"], (StepState::Skip, "removed".into()));
    assert!(!h.installed.contains("Codex"));
    assert_eq!(
        fs::read_to_string(home.join("npm-calls")).unwrap(),
        format!("uninstall -g --prefix {} @openai/codex\n", home.join(".local").display())
    );
    assert_eq!([value(&h, "Claude"), value(&h, "T3")], ["1.0.0", "0.0.1"], "removing updates nothing");
    assert!(!home.join("update-calls").exists());
    // Codex's and Claude's own installs come apart; ~/.codex and ~/.claude stay.
    reset(&home);
    link(&home, "codex", CODEX_STANDALONE);
    fs::write(home.join(".codex/config.toml"), "model = 'x'\n").unwrap();
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::write(home.join(".claude/settings.json"), "{}").unwrap();
    let (h, _) = remote(&home, Mode::Remove, "codex,claude").await;
    ok(&h);
    assert_eq!([value(&h, "Codex"), value(&h, "Claude")], ["removed", "removed"]);
    assert!(!home.join(".codex/packages/standalone").exists() && !home.join(".local/share/claude").exists());
    assert!(!home.join(".local/bin/codex").exists() && !home.join(".local/bin/claude").exists());
    assert!(home.join(".codex/config.toml").exists() && home.join(".claude/settings.json").exists());
    // Put the fakes back for what follows.
    tool(
        &home,
        CODEX_STANDALONE,
        r#"case $1 in --version) echo "codex-cli $(cat "$HOME/codex")";; login) [ -f "$HOME/codex-auth" ];; esac"#,
    );
    tool(
        &home,
        CLAUDE,
        r#"case $1 in
--version) echo "$(cat "$HOME/claude") (Claude Code)";;
update) sleep 1; [ -f "$HOME/claude-broken" ] && { echo 'disk full' >&2; exit 3; }; echo 2.0.0 > "$HOME/claude";;
auth) [ -f "$HOME/claude-auth" ] && echo '{ "loggedIn": true }' || echo '{ "loggedIn": false }';;
esac"#,
    );
    // Something t3up didn't install, it won't guess at.
    reset(&home);
    remove(&home.join(".local/bin/codex"));
    tool(&home, ".local/bin/codex", "echo 'codex-cli 0.1.0'");
    let (h, _) = remote(&home, Mode::Remove, "codex").await;
    assert_eq!(h.steps["Codex"].0, StepState::Fail);
    assert!(h.error.contains("remove it by hand"), "{}", h.error);
    remove(&home.join(".local/bin/codex"));

    reset(&home);
    fs::write(home.join("ps-output"), "1 0 init\n10 1 node /usr/bin/t3 serve\n11 10 /usr/lib/node_modules/t3/node_modules/@t3code/t3-linux-x64/t3 serve\n12 11 /usr/bin/codex --task\n13 12 /usr/bin/codex child\n14 11 /home/u/bin/claude\n15 14 node /x/codex.js\n16 11 t3-resource-monitor\n17 11 cloudflared\n18 1 /usr/bin/grok\n20 1 /home/u/.t3/runtime/versions/0.0.46-nightly.20261004.2648/t3 serve\n21 20 /usr/bin/opencode\n22 21 /usr/bin/pi\n23 20 pi --run\n24 20 node /usr/lib/node_modules/@earendil-works/pi-coding-agent/dist/cli.js\n25 20 node /usr/lib/node_modules/@anthropic-ai/claude-code/cli.js\n26 20 node /usr/lib/node_modules/@xai-official/grok/dist/index.js\n27 26 node /usr/lib/node_modules/@xai-official/grok/dist/index.js\n").unwrap();
    let (h, _) = remote(&home, Mode::Check, "all").await;
    ok(&h);
    assert_eq!(h.busy, Some(7), "only top-level agents beneath T3");
    let (h, _) = remote(&home, Mode::Update, "codex").await;
    ok(&h);
    assert_eq!(h.busy, Some(7));

    for system in [false, true] {
        reset(&home);
        fs::write(home.join("bad-health"), "").unwrap();
        fs::write(home.join("fast-health"), "").unwrap();
        if system {
            fs::write(home.join("system-service"), "").unwrap();
        }
        let (h, events) = remote(&home, Mode::Update, "t3").await;
        assert_eq!(h.status, Status::Failed);
        assert_eq!(fs::read_to_string(home.join("t3")).unwrap().trim(), "0.0.1");
        assert!(events.contains(&Event::Rollback("T3: 0.0.2 -> 0.0.1".into())), "{:?}", h.lines);
        assert!(events.contains(&Event::Begin("Rollback".into())));
        assert!(events.contains(&Event::Done("Rollback: OK".into())));
        assert!(!events.contains(&Event::Complete));
        assert_eq!(h.version, "0.0.1");
        assert_eq!(h.current["T3"], "0.0.1");
        if system {
            assert!(fs::read_to_string(home.join("npm-calls")).unwrap().contains("t3@0.0.1"));
        } else {
            assert!(
                fs::read_to_string(home.join("update-calls")).unwrap().contains("update --yes --allow-downgrade 0.0.1")
            );
        }
    }
    reset(&home);
    for name in ["bad-health", "fast-health", "rollback-broken"] {
        fs::write(home.join(name), "").unwrap();
    }
    let (h, events) = remote(&home, Mode::Update, "t3").await;
    assert_eq!(h.status, Status::Failed);
    assert!(events.contains(&Event::Fail("Rollback".into())));
    assert!(h.lines.iter().any(|l| l.contains("rollback failed")));
    assert!(!events.iter().any(|e| matches!(e, Event::Rollback(_))));

    // Read-only runs and unchanged T3 versions never trigger rollback.
    for mode in [Mode::Check, Mode::Update] {
        reset(&home);
        for name in ["always-bad-health", "fast-health"] {
            fs::write(home.join(name), "").unwrap();
        }
        let (h, events) = remote(&home, mode, "codex").await;
        assert_eq!(h.status, Status::Failed);
        assert!(
            !events
                .iter()
                .any(|e| (matches!(e, Event::Rollback(_)) || matches!(e, Event::Begin(s) if s == "Rollback")))
        );
        assert!(!home.join("update-calls").exists());
    }
    reset(&home);
    for name in ["always-bad-health", "fast-health"] {
        fs::write(home.join(name), "").unwrap();
    }
    let (h, events) = remote(&home, Mode::Update, "t3").await;
    assert_eq!(h.status, Status::Failed);
    assert_eq!(fs::read_to_string(home.join("t3")).unwrap().trim(), "0.0.1");
    assert!(h.lines.iter().any(|l| l.contains("rollback health check failed")));
    assert!(!events.iter().any(|e| matches!(e, Event::Rollback(_))));

    tool(&root.0, "ssh", "printf 'one\\n'; printf 'two\\n' >&2; printf 'three\\n'; exit 255");
    let (h, events) = remote(&home, Mode::Check, "all").await;
    assert_eq!(h.status, Status::Failed);
    let output: Vec<_> = events
        .iter()
        .filter_map(|e| if let Event::Output { text, .. } = e { Some(text.as_str()) } else { None })
        .collect();
    assert_eq!(output, ["one", "two", "three"]);
    assert!(matches!(events.last(), Some(Event::Exit { code: Some(255), error: None })));

    // Spawn errors still produce Start then Exit, with a useful error.
    unsafe {
        std::env::set_var("T3UP_SSH", root.0.join("missing-ssh"));
    }
    let (h, events) = remote(&home, Mode::Check, "all").await;
    assert_eq!(h.status, Status::Failed);
    assert!(matches!(events.last(), Some(Event::Exit { error: Some(_), .. })));
    // A log I/O failure also exits cleanly, before SSH can start.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    job::run_job(
        Job {
            host: "box".into(),
            mode: Mode::Check,
            version: "".into(),
            only: "all".into(),
            logs: home.join("t3"),
            script: Arc::from(""),
        },
        tx,
    )
    .await;
    assert!(matches!(rx.recv().await.unwrap().1, Event::Start { .. }));
    assert!(matches!(rx.recv().await.unwrap().1, Event::Exit { code: None, error: Some(_) }));
    assert!(rx.recv().await.is_none());

    // Quitting kills the SSH process rather than leaving a running session behind.
    tool(&root.0, "ssh", "cat >/dev/null; echo $$ > \"$HOME/ssh-pid\"; exec /bin/sleep 30");
    unsafe {
        std::env::set_var("T3UP_SSH", root.0.join("ssh"));
    }
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(job::run_job(
        Job {
            host: "box".into(),
            mode: Mode::Check,
            version: "".into(),
            only: "all".into(),
            logs: home.join("logs"),
            script: Arc::from("exit 0\n"),
        },
        tx,
    ));
    let pid = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Ok(pid) = tokio::fs::read_to_string(home.join("ssh-pid")).await {
                break pid.trim().parse::<i32>().unwrap();
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        // SAFETY: signal 0 only checks the process spawned by this test.
        while unsafe { libc::kill(pid, 0) } == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("aborting the job must kill SSH");
}

// Direct dash runs use per-command environments so these cases can run beside remote_suite.
fn t3_version_run(before: &str) -> (Sandbox, std::process::Output) {
    let root = Sandbox::new();
    tool(
        &root.0,
        ".local/bin/t3",
        r#"case $1 in
--version)
  if [ -f "$HOME/updated" ]; then echo 't3 v0.0.2'
  elif [ "$INITIAL_VERSION" = fail ]; then exit 3
  else printf '%s\n' "$INITIAL_VERSION"; fi;;
update) echo "$*" >> "$HOME/update-calls"; touch "$HOME/updated";;
esac"#,
    );
    for (name, body) in [("curl", "exit 1"), ("sleep", "exit 0"), ("systemctl", "exit 1"), ("ps", "exit 0")] {
        tool(&root.0, &format!(".local/bin/{name}"), body);
    }
    let script = model::remote_script().replace(":/usr/local/bin:/opt/homebrew/bin:", ":");
    let out = std::process::Command::new("/bin/dash")
        .args(["-c", &script, "t3up", "update", "", "t3"])
        .env_clear()
        .env("HOME", &root.0)
        .env("PATH", "/usr/bin:/bin")
        .env("INITIAL_VERSION", before)
        .output()
        .unwrap();
    (root, out)
}

#[test]
fn failed_t3_version_does_not_update() {
    let (root, out) = t3_version_run("fail");
    assert!(!out.status.success());
    assert!(!root.0.join("update-calls").exists(), "a failed version check must stop the T3 step");
    assert!(String::from_utf8_lossy(&out.stdout).contains("@@t3up\tfail\tT3"));
}

#[test]
fn rollback_refuses_unknown_or_invalid_previous_versions() {
    for before in ["t3 v", "t3 vnightly", "t3 v0.0.1 invalid"] {
        let (root, out) = t3_version_run(before);
        assert!(!out.status.success());
        assert_eq!(std::fs::read_to_string(root.0.join("update-calls")).unwrap().lines().count(), 1, "{before:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("rollback impossible"), "{out:?}");
        assert!(String::from_utf8_lossy(&out.stdout).contains("@@t3up\tfail\tRollback"));
    }
}
