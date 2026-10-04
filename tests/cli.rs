//! Subprocesses have their own fake SSH, server list, registry, and state directory.
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
    time::Instant,
};

struct Sandbox(PathBuf);
impl Sandbox {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "t3up-cli-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("servers"), "# test\n\nfirst # unavailable\nsecond\nsecond").unwrap();
        fs::write(
            root.join("ssh"),
            r#"#!/bin/dash
while [ "$1" = -o ]; do shift 2; done
host=$1
printf 'start %s %s\n' "$host" "$2" >> "$CALLS"
cat > /dev/null
sleep "${DELAY:-1}"
printf 'end %s\n' "$host" >> "$CALLS"
if [ "$host" = first ] && [ "${FAIL_FIRST:-0}" = 1 ]; then
  if [ "${ROLLBACK:-0}" = 1 ]; then printf '@@t3up\trollback\tT3: 0.0.2 -> 0.0.1\n'; fi
  echo 'Connection refused' >&2
  exit 255
fi
printf 'Codex| \033[32mdownloaded\033[0m\rprogress\r\nok\n'
printf '@@t3up\tbegin\tCodex\n@@t3up\tdone\tCodex: codex-cli 1.0.0 -> codex-cli 1.1.0\n'
printf '@@t3up\tauth\tCodex\n@@t3up\tbusy\t3\n@@t3up\tversion\t1.0.0\n@@t3up\tdone\tHealth: OK\n'
[ "${NO_COMPLETE:-0}" = 1 ] || printf '@@t3up\tcomplete\tOK\n'
exit 0
"#,
        )
        .unwrap();
        fs::set_permissions(root.join("ssh"), fs::Permissions::from_mode(0o755)).unwrap();
        for (pkg, tags) in [
            ("@openai/codex", "{\"latest\":\"1.2.0\"}"),
            ("t3", "{}"),
            ("@anthropic-ai/claude-code", "{}"),
            ("opencode-ai", "{}"),
            ("opencode", "{}"),
            ("@xai-official/grok", "{}"),
            ("@earendil-works/pi-coding-agent", "{}"),
        ] {
            let path = root.join(format!("registry/-/package/{pkg}/dist-tags"));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, tags).unwrap();
        }
        Self(root)
    }
    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_t3up"));
        c.env("T3UP_SSH", self.0.join("ssh"))
            .env("T3UP_SERVERS_FILE", self.0.join("servers"))
            .env("XDG_STATE_HOME", self.0.join("state"))
            .env("T3UP_REGISTRY", format!("file://{}", self.0.join("registry").display()))
            .env("T3UP_RELEASES_API", "off")
            .env("CALLS", self.0.join("calls"))
            .env("FAIL_FIRST", "0")
            .env("DELAY", "1")
            .env("ROLLBACK", "0")
            .env("NO_COMPLETE", "0");
        c
    }
    fn calls(&self) -> String {
        fs::read_to_string(self.0.join("calls")).unwrap_or_default()
    }
    fn clear(&self) {
        let _ = fs::remove_file(self.0.join("calls"));
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn stdout(result: &Output) -> String {
    String::from_utf8_lossy(&result.stdout).into_owned()
}

#[test]
fn cli_suite() {
    let root = Sandbox::new();
    // Exercise canary scheduling independently of argument parsing.
    direct_headless(&root);
    root.clear();
    let start = Instant::now();
    let result = root.command().args(["--check"]).env("FAIL_FIRST", "1").output().unwrap();
    assert!(start.elapsed().as_secs_f64() < 1.8, "hosts did not run in parallel");
    assert_eq!(
        result.status.code(),
        Some(1),
        "stdout: {} stderr: {}",
        stdout(&result),
        String::from_utf8_lossy(&result.stderr)
    );
    let out = stdout(&result);
    assert!(out.contains("second           OK") && out.contains("1/2 passed"), "{out}");
    assert!(out.contains("update available: Codex 1.2.0"), "{out}");
    assert!(out.contains("! Codex not signed in; run: ssh -t second 'for dir in"), "{out}");
    assert!(out.contains("first   ✗ Connection refused"), "{out}");
    assert!(out.contains(" · 3 agents live"), "{out}");
    assert!(!out.contains('\x1b'));
    assert!(!out.contains("downloaded"));
    let calls = root.calls();
    assert!(calls.contains("start first sh -s -- check '' all"));
    assert!(calls.contains("start second sh -s -- check '' all"));
    assert_eq!(calls.lines().filter(|l| l.starts_with("start ")).count(), 2);
    let dirs: Vec<_> = fs::read_dir(root.0.join("state/t3up")).unwrap().map(|p| p.unwrap().path()).collect();
    let logs: Vec<_> = dirs.iter().flat_map(|p| fs::read_dir(p).unwrap()).map(|p| p.unwrap().path()).collect();
    assert_eq!(logs.len(), 2);
    assert!(logs.iter().any(|p| { fs::read_to_string(p).unwrap().contains("Connection refused") }));
    assert!(logs.iter().any(|p| fs::read_to_string(p).unwrap().contains("\x1b[32m")), "raw logs keep escapes");

    root.clear();
    for args in [vec!["bad'; touch /tmp/no"], vec!["--check", "--only", "t3"], vec!["--host", "unknown", "--check"]] {
        let result = root.command().args(args).output().unwrap();
        assert_eq!(result.status.code(), Some(2));
        assert!(root.calls().is_empty(), "invalid arguments must not call SSH");
    }
    let result = root.command().args(["--only", "codex", "--host", "second"]).env("DELAY", "0").output().unwrap();
    assert!(result.status.success(), "{}", stdout(&result));
    assert_eq!(
        root.calls().lines().filter(|l| l.starts_with("start ")).collect::<Vec<_>>(),
        ["start second sh -s -- update '' codex"]
    );

    root.clear();
    let result = root.command().args(["--check", "--verbose"]).env("DELAY", "0").output().unwrap();
    assert!(result.status.success());
    let out = stdout(&result);
    assert!(out.contains("Codex: downloaded"), "{out}");
    assert!(!out.contains('\x1b'));
    assert!(out.contains("2/2 passed"));

    root.clear();
    let result = root.command().args(["--check"]).env("NO_COMPLETE", "1").env("DELAY", "0").output().unwrap();
    assert_eq!(result.status.code(), Some(1), "exit 0 without complete is a failure");
    assert!(stdout(&result).contains("0/2 passed"));

    // Default update is a canary; the other two must start only after the first has ended.
    fs::write(root.0.join("servers"), "first\nsecond\nthird\n").unwrap();
    root.clear();
    let start = Instant::now();
    let result = root.command().args(["--update"]).output().unwrap();
    assert!(result.status.success(), "{}", stdout(&result));
    assert!(start.elapsed().as_secs_f64() < 2.8, "rest of rollout must run in parallel");
    assert!(stdout(&result).contains("Canary: updating first first"));
    let calls = root.calls();
    let lines: Vec<_> = calls.lines().collect();
    assert!(lines[0].starts_with("start first "));
    assert_eq!(lines[1], "end first", "canary must run alone: {calls}");
    assert!(lines[2].starts_with("start ") && lines[3].starts_with("start "), "{calls}");
    assert!(stdout(&result).contains("3/3 passed"));

    root.clear();
    let result = root
        .command()
        .args(["--update"])
        .env("FAIL_FIRST", "1")
        .env("ROLLBACK", "1")
        .env("DELAY", "0")
        .output()
        .unwrap();
    assert_eq!(
        result.status.code(),
        Some(1),
        "stdout: {} stderr: {}",
        stdout(&result),
        String::from_utf8_lossy(&result.stderr)
    );
    let out = stdout(&result);
    assert!(out.contains("second           SKIPPED  (canary failed)"), "{out}");
    assert!(out.contains("third            SKIPPED  (canary failed)"), "{out}");
    assert!(out.contains("↩ T3 rolled back"), "{out}");
    assert!(out.contains("0/3 passed"));
    assert_eq!(root.calls().lines().count(), 2);

    root.clear();
    let start = Instant::now();
    let result = root.command().args(["--update", "--all-at-once"]).output().unwrap();
    assert!(result.status.success());
    assert!(start.elapsed().as_secs_f64() < 1.8);
    assert!(!stdout(&result).contains("Canary:"));
    assert_eq!(root.calls().lines().filter(|l| l.starts_with("start ")).count(), 3);
}

fn direct_headless(root: &Sandbox) {
    use t3up::{
        headless::{self, Options},
        model::Mode,
    };
    let vars = [
        ("T3UP_SSH", root.0.join("ssh").display().to_string()),
        ("T3UP_REGISTRY", format!("file://{}", root.0.join("registry").display())),
        ("CALLS", root.0.join("calls").display().to_string()),
        ("DELAY", "0.1".into()),
        ("FAIL_FIRST", "0".into()),
        ("NO_COMPLETE", "0".into()),
        ("ROLLBACK", "0".into()),
    ];
    let old: Vec<_> = vars.iter().map(|(k, _)| (*k, std::env::var_os(k))).collect();
    // One test owns the process environment; jobs finish before it is changed again.
    for (k, v) in &vars {
        unsafe {
            std::env::set_var(k, v);
        }
    }
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let mut opts = Options {
        hosts: vec!["first".into(), "second".into(), "third".into()],
        mode: Mode::Update,
        version: "".into(),
        only: "all".into(),
        verbose: false,
        canary: true,
    };
    let code = runtime.block_on(headless::run(opts.clone(), String::new(), root.0.join("direct-logs")));
    assert_eq!(code, 0);
    let calls = root.calls();
    let lines: Vec<_> = calls.lines().collect();
    assert!(lines[0].starts_with("start first "));
    assert_eq!(lines[1], "end first", "canary must run alone: {calls}");
    assert!(lines[2].starts_with("start ") && lines[3].starts_with("start "));
    root.clear();
    unsafe {
        std::env::set_var("FAIL_FIRST", "1");
        std::env::set_var("ROLLBACK", "1");
    }
    let code = runtime.block_on(headless::run(opts.clone(), String::new(), root.0.join("direct-logs")));
    assert_eq!(code, 1);
    assert_eq!(root.calls().lines().count(), 2, "canary failure must skip the rest");
    root.clear();
    opts.mode = Mode::Check;
    let code = runtime.block_on(headless::run(opts, String::new(), root.0.join("direct-logs")));
    assert_eq!(code, 1);
    assert_eq!(root.calls().lines().filter(|l| l.starts_with("start ")).count(), 3, "check ignores canary");
    drop(runtime);
    for (k, v) in old {
        unsafe {
            if let Some(v) = v {
                std::env::set_var(k, v);
            } else {
                std::env::remove_var(k);
            }
        }
    }
}
