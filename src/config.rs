//! Files t3up keeps on this machine: the server list, state (logs, tools seen per server).
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::model::HOST_RE;

fn home() -> PathBuf {
    std::env::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| home().join(fallback))
}

/// `~/.config/t3up/servers`, or `$T3UP_SERVERS_FILE`.
pub fn servers_file() -> PathBuf {
    match std::env::var_os("T3UP_SERVERS_FILE") {
        Some(path) => PathBuf::from(path),
        None => xdg("XDG_CONFIG_HOME", ".config").join("t3up/servers"),
    }
}

/// One SSH alias or user@host per line; blank lines and # comments ignored, duplicates dropped.
pub fn parse_servers(text: &str) -> Result<Vec<String>, String> {
    let mut hosts: Vec<String> = vec![];
    for line in text.lines() {
        let host = line.split('#').next().unwrap_or("").trim();
        if host.is_empty() || hosts.iter().any(|h| h == host) {
            continue;
        }
        if !HOST_RE.is_match(host) {
            return Err(format!("invalid server '{host}'"));
        }
        hosts.push(host.to_string());
    }
    Ok(hosts)
}

/// The servers file's text ('' if missing) and the hosts in it.
pub fn read_servers() -> Result<(String, Vec<String>), String> {
    let path = servers_file();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let hosts = parse_servers(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((text, hosts))
}

/// The file's text with `host` appended on its own line.
pub fn add_server(text: &str, host: &str) -> String {
    let text = if text.trim().is_empty() { "# t3up servers: one SSH alias or user@host per line.\n" } else { text };
    format!("{}\n{host}\n", text.trim_end_matches('\n'))
}

/// The file's text without the line naming `host`; every other line (comments too) kept.
pub fn remove_server(text: &str, host: &str) -> String {
    let kept: Vec<&str> = text.lines().filter(|l| l.split('#').next().unwrap_or("").trim() != host).collect();
    format!("{}\n", kept.join("\n").trim_end_matches('\n'))
}

pub fn write_servers(text: &str) -> std::io::Result<()> {
    let path = servers_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, format!("{}\n", text.trim_end_matches('\n')))
}

/// Concrete Host aliases from ~/.ssh/config, offered as suggestions.
pub fn ssh_hosts() -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(home().join(".ssh/config")) else { return vec![] };
    let mut hosts = vec![];
    for line in text.lines() {
        let mut words = line.split_whitespace();
        if words.next().is_some_and(|w| w.eq_ignore_ascii_case("host")) {
            hosts.extend(words.filter(|h| HOST_RE.is_match(h)).map(String::from));
        }
    }
    hosts
}

/// `~/.local/state/t3up`: per-run log directories and tools.json.
pub fn state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("t3up")
}

/// A fresh directory name for this run's logs (created when the first job writes to it).
pub fn run_logs() -> PathBuf {
    state_dir().join(chrono::Local::now().format("%Y%m%d-%H%M%S-%6f").to_string())
}

/// {host: [tools]} as last seen, kept across runs, beside the log directories.
pub fn known_tools(state: &Path) -> BTreeMap<String, Vec<String>> {
    std::fs::read_to_string(state.join("tools.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_known_tools(state: &Path, tools: &BTreeMap<String, Vec<String>>) {
    let _ = std::fs::create_dir_all(state);
    let _ = std::fs::write(state.join("tools.json"), serde_json::to_string(tools).unwrap_or_default());
}

/// This machine's short name, e.g. 'Ganz-Harbour'.
pub fn local_name() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: buf is valid for its length; gethostname NUL-terminates on success.
        if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            let name = String::from_utf8_lossy(&buf[..end]).into_owned();
            return name.split('.').next().unwrap_or("").to_string();
        }
    }
    std::env::var("COMPUTERNAME").unwrap_or_default()
}

/// New files are private to this user (logs can carry tokens printed by installers).
pub fn private_umask() {
    #[cfg(unix)]
    // SAFETY: umask only changes this process's file creation mask.
    unsafe {
        libc::umask(0o077);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servers_text() {
        let text = "# test\n\nfirst # unavailable\nsecond\nsecond";
        assert_eq!(parse_servers(text).unwrap(), vec!["first", "second"]);
        assert!(parse_servers("bad;host").is_err());
        assert_eq!(remove_server(text, "first"), "# test\n\nsecond\nsecond\n");
        assert_eq!(add_server("# test\nsecond", "fourth"), "# test\nsecond\nfourth\n");
        assert!(add_server("", "a").ends_with("line.\na\n"));
    }
}
