//! Newest release of each component, from the npm registry.
use std::{collections::HashMap, time::Duration};

const RELEASES: &[(&str, &str, &str)] = &[
    ("T3", "t3", "nightly"),
    ("Codex", "@openai/codex", "latest"),
    ("Claude", "@anthropic-ai/claude-code", "latest"),
    ("OpenCode", "opencode-ai", "latest"),
    ("OpenCode 2", "@opencode/cli", "latest"),
    ("Grok", "@xai-official/grok", "latest"),
    ("Pi", "@earendil-works/pi-coding-agent", "latest"),
];

/// {'T3': '0.0.46-nightly.20261003.2632', ...}. Blocking: run it on a blocking thread.
/// A tool the registry can't answer for is left out, so offline just means no update hints.
pub fn fetch_latest() -> HashMap<String, String> {
    let Ok(registry) = std::env::var("T3UP_REGISTRY") else {
        let mut latest = fetch_at("https://registry.npmjs.org");
        // Claude installs itself, and `claude update` reads its own release channel, which can trail npm
        // by hours: npm's newest would be an update its updater can't make yet.
        if let Some(version) = claude_channel() {
            latest.insert("Claude".into(), version);
        }
        return latest;
    };
    fetch_at(&registry)
}

/// The newest Claude Code on the channel its own updater reads.
fn claude_channel() -> Option<String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(5))).build().into();
    let body = agent
        .get("https://downloads.claude.ai/claude-code-releases/latest")
        .call()
        .ok()?
        .body_mut()
        .read_to_string()
        .ok()?;
    let version = body.trim();
    crate::model::VERSION_RE.is_match(version).then(|| version.to_string())
}

fn fetch_at(registry: &str) -> HashMap<String, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(5))).build().into();
    std::thread::scope(|scope| {
        let jobs: Vec<_> = RELEASES
            .iter()
            .map(|&(name, package, tag)| {
                let agent = &agent;
                scope.spawn(move || {
                    let url = format!("{}/-/package/{package}/dist-tags", registry.trim_end_matches('/'));
                    let body = if let Some(path) = url.strip_prefix("file://") {
                        // Path.as_uri() percent-escapes spaces and non-ASCII bytes.
                        let mut bytes = Vec::new();
                        let mut i = 0;
                        while i < path.len() {
                            if path.as_bytes()[i] == b'%' && i + 2 < path.len() {
                                bytes.push(u8::from_str_radix(path.get(i + 1..i + 3)?, 16).ok()?);
                                i += 3;
                            } else {
                                bytes.push(path.as_bytes()[i]);
                                i += 1;
                            }
                        }
                        let path = String::from_utf8(bytes).ok()?;
                        #[cfg(windows)]
                        let path =
                            if path.starts_with('/') && path.get(2..3) == Some(":") { &path[1..] } else { &path };
                        std::fs::read_to_string(path).ok()?
                    } else {
                        agent.get(&url).call().ok()?.body_mut().read_to_string().ok()?
                    };
                    let tags: serde_json::Value = serde_json::from_str(&body).ok()?;
                    let version = tags[tag].as_str().filter(|s| !s.is_empty())?;
                    Some((name.to_string(), version.to_string()))
                })
            })
            .collect();
        jobs.into_iter().filter_map(|job| job.join().ok().flatten()).collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_registry_leaves_failures_out() {
        let root = std::env::temp_dir().join(format!("t3up registry-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for (package, body) in [
            ("t3", r#"{"nightly":"0.0.46-nightly.20261004.2648","latest":"0.0.45"}"#),
            ("@openai/codex", r#"{"latest":"0.161.0"}"#),
            ("@anthropic-ai/claude-code", "not json"),
            ("opencode-ai", r#"{"other":"1.0.0"}"#),
            ("@opencode/cli", r#"{"latest":"2.0.22"}"#),
            ("@xai-official/grok", r#"{"latest":""}"#),
        ] {
            let path = root.join(format!("-/package/{package}/dist-tags"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        assert_eq!(
            fetch_at(&format!("file://{}", root.display()).replace(' ', "%20")),
            HashMap::from([
                ("T3".into(), "0.0.46-nightly.20261004.2648".into()),
                ("Codex".into(), "0.161.0".into()),
                ("OpenCode 2".into(), "2.0.22".into()),
            ])
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
