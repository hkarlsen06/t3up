//! The pairing command (`model::pair_command`) under bash and dash, with fake tailscale, sudo and t3.
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

fn tool(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Run the pairing command with a fake toolbox; returns what t3 and sudo were asked to do.
fn pair(shell: &str, tailscale: Option<&str>, operator: bool) -> (String, String) {
    let label = format!("{}-{}-{}", shell.rsplit('/').next().unwrap(), tailscale.unwrap_or("none"), operator);
    let dir = std::env::temp_dir().join(format!("t3up-pair-{}-{label}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let log = dir.join("log");
    tool(&dir, "t3", &format!("echo \"t3 $*\" >> {}", log.display()));
    tool(&dir, "sudo", &format!("echo \"sudo $*\" >> {}; exit 0", log.display()));
    if let Some(state) = tailscale {
        let prefs = if operator { r#"{ "OperatorUser": "$(id -un)" }"# } else { r#"{ "OperatorUser": "" }"# };
        tool(
            &dir,
            "tailscale",
            &format!(
                "case $1 in status) [ {state} = running ];; debug) echo \"{}\";; esac",
                prefs.replace('"', "\\\"")
            ),
        );
    }
    let command = t3up::model::pair_command(true); // without PATH_SETUP: only the fakes on PATH
    let command = command.as_str();
    let mut child = Command::new(shell)
        .args(["-c", command])
        .env("PATH", format!("{}:/usr/bin:/bin", dir.display()))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{shell}: {out:?}");
    let calls = fs::read_to_string(&log).unwrap_or_default();
    let _ = fs::remove_dir_all(&dir);
    (calls, String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
fn pairs_over_tailscale_when_it_runs() {
    for shell in ["/bin/bash", "/bin/dash"].into_iter().filter(|s| Path::new(s).exists()) {
        let (calls, _) = pair(shell, Some("running"), true);
        assert_eq!(calls, "t3 pair --tailscale\n", "{shell}: already the operator, no sudo");
        let (_, out) = pair(shell, Some("running"), true);
        assert!(out.contains("@@pair tailscale"));

        let (calls, out) = pair(shell, Some("running"), false);
        assert!(calls.starts_with("sudo tailscale set --operator="), "{shell}: {calls}");
        assert!(calls.ends_with("t3 pair --tailscale\n"), "{shell}: {calls}");
        assert!(out.contains("Tailscale Serve"), "{shell}: says why it needs sudo");

        let (calls, out) = pair(shell, Some("stopped"), true);
        assert_eq!(calls, "t3 pair\n", "{shell}: tailscale down, local link");
        assert!(out.contains("@@pair local"), "{shell}: tells the dashboard it's a local link");

        let (calls, _) = pair(shell, None, true);
        assert_eq!(calls, "t3 pair\n", "{shell}: no tailscale, local link");
    }
}
