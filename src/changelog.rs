//! What changed between two versions of a component, from its GitHub releases.
use std::{sync::LazyLock, time::Duration};

use regex::Regex;
use serde::Deserialize;

use crate::model::{clean, numbers};

/// One release's notes, cleaned for a terminal.
#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    /// e.g. 'v0.0.46-nightly.20261004.2648'
    pub tag: String,
    /// 'YYYY-MM-DD'
    pub date: String,
    /// One entry per change, e.g. 'fix(web): keep workspace panels below dialogs'.
    pub notes: Vec<String>,
}

fn source(component: &str) -> Option<(&'static str, &'static str)> {
    match component {
        "T3" => Some(("pingdotgg/t3code", "v")),
        "Claude" => Some(("anthropics/claude-code", "v")),
        "Codex" => Some(("openai/codex", "rust-v")),
        "OpenCode" | "OpenCode 2" => Some(("anomalyco/opencode", "v")),
        "Pi" => Some(("earendil-works/pi", "v")),
        _ => None,
    }
}

/// Whether t3up knows where `component` publishes release notes.
pub fn has_changelog(component: &str) -> bool {
    source(component).is_some()
}

pub(crate) fn github_json(path: &str) -> Result<serde_json::Value, String> {
    let base = std::env::var("T3UP_GITHUB_API").unwrap_or_else(|_| "https://api.github.com".into());
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut request = agent.get(format!("{}{path}", base.trim_end_matches('/'))).header("User-Agent", "t3up");
    if let Some(token) =
        ["GITHUB_TOKEN", "GH_TOKEN"].iter().find_map(|key| std::env::var(key).ok().filter(|s| !s.is_empty()))
    {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let mut response = request.call().map_err(|e| format!("GitHub request failed: {e}"))?;
    let status = response.status().as_u16();
    let limited = response.headers().get("x-ratelimit-remaining").is_some_and(|v| v == "0");
    let body = response.body_mut().read_to_string().map_err(|e| format!("Cannot read GitHub response: {e}"))?;
    if status >= 400 {
        let message = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["message"].as_str().map(str::to_string))
            .unwrap_or_default();
        return Err(github_error(status, &message, limited));
    }
    serde_json::from_str(&body).map_err(|_| "GitHub returned invalid release data".into())
}

fn github_error(status: u16, message: &str, limited: bool) -> String {
    if matches!(status, 403 | 429) && (limited || status == 429 || message.to_lowercase().contains("rate limit")) {
        "GitHub rate limit reached; set GITHUB_TOKEN".into()
    } else if message.is_empty() {
        format!("GitHub returned HTTP {status}")
    } else {
        format!("GitHub HTTP {status}: {}", clean(message))
    }
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
}

static LINKS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\([^\s)]+\)").unwrap());
static AUTHOR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+by @\S+ in https?://\S+\s*$").unwrap());
static PR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\s*(?:\(?https?://github\.com/[^\s]+/pull/\d+\)?|\(#[0-9]+(?: by @[^)]+)?\))\s*$").unwrap()
});
static EMPHASIS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"`([^`]*)`|\*\*([^*]+)\*\*|__([^_]+)__|~~([^~]+)~~|\*([^*]+)\*|(^|\W)_([^_]+)_\b").unwrap()
});

fn notes(body: &str) -> Vec<String> {
    body.lines()
        .filter_map(|line| {
            let line = line.trim();
            let line = line.strip_prefix("* ").or_else(|| line.strip_prefix("- "))?;
            let line = LINKS.replace_all(line, "$1");
            let line = AUTHOR.replace(&line, "");
            let line = PR.replace(&line, "");
            let line = EMPHASIS.replace_all(&line, "$1$2$3$4$5$6$7");
            let line = clean(&line);
            (!line.is_empty() && !line.starts_with("Full Changelog")).then_some(line)
        })
        .collect()
}

fn in_range(releases: &[GithubRelease], prefix: &str, from: &[u64], to: &[u64]) -> Vec<Release> {
    releases
        .iter()
        .filter_map(|release| {
            let version = release.tag_name.strip_prefix(prefix)?;
            if release.draft || !version.starts_with(|c: char| c.is_ascii_digit()) {
                return None;
            }
            let version = numbers(version);
            if version.as_slice() <= from || version.as_slice() > to {
                return None;
            }
            let notes = notes(release.body.as_deref().unwrap_or_default());
            if notes.is_empty() {
                return None;
            }
            Some(Release {
                tag: release.tag_name.clone(),
                date: release.published_at.as_deref().unwrap_or_default().split('T').next().unwrap_or_default().into(),
                notes,
            })
        })
        .collect()
}

/// Releases of `component` newer than `from`, up to and including `to`, newest first.
/// Blocking: run it on a blocking thread.
pub fn changelog(component: &str, from: &str, to: &str) -> Result<Vec<Release>, String> {
    let (repo, prefix) = source(component).ok_or_else(|| format!("No release notes source for {component}"))?;
    let (from, to) = (numbers(from), numbers(to));
    if from >= to {
        return Ok(vec![]);
    }
    let mut found = Vec::new();
    for page in 1..=5 {
        let releases: Vec<GithubRelease> =
            serde_json::from_value(github_json(&format!("/repos/{repo}/releases?per_page=100&page={page}"))?)
                .map_err(|_| "GitHub returned invalid release data".to_string())?;
        found.extend(in_range(&releases, prefix, &from, &to));
        let versions: Vec<_> = releases
            .iter()
            .filter(|r| !r.draft)
            .filter_map(|r| r.tag_name.strip_prefix(prefix))
            .filter(|v| v.starts_with(|c: char| c.is_ascii_digit()))
            .map(numbers)
            .collect();
        // Releases are ordered by creation, which can differ slightly from version order.
        if releases.len() < 100 || (!versions.is_empty() && versions.iter().all(|v| *v <= from)) {
            break;
        }
    }
    found.sort_by_cached_key(|r| std::cmp::Reverse(numbers(&r.tag)));
    found.dedup_by(|a, b| a.tag == b.tag);
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_range_and_terminal_notes() {
        let releases: Vec<GithubRelease> = serde_json::from_str(r###"[
            {"tag_name":"v1.3.0","published_at":"2026-10-04T10:00:00Z","body":"## What's Changed\n* feat(web): **keep** [panels](https://example.com) below `dialogs` by @user in https://github.com/a/b/pull/1\n- fix: *things* (https://github.com/a/b/pull/2)\n**Full Changelog**: https://example.com"},
            {"tag_name":"v1.2.0","body":"## Empty\n**Full Changelog**: https://example.com"},
            {"tag_name":"v1.1.0","body":"- fix: excluded lower bound"},
            {"tag_name":"v1.4.0","body":"- fix: excluded upper bound"},
            {"tag_name":"v1.2.1","body":"- fix: draft","draft":true}
        ]"###).unwrap();
        assert_eq!(
            in_range(&releases, "v", &numbers("1.1.0"), &numbers("1.3.0")),
            vec![Release {
                tag: "v1.3.0".into(),
                date: "2026-10-04".into(),
                notes: vec!["feat(web): keep panels below dialogs".into(), "fix: things".into()],
            }]
        );
        assert_eq!(
            notes("- Fix ([#9776](https://github.com/a/b/pull/9776) by [@user](https://github.com/user))"),
            vec!["Fix"]
        );
        assert_eq!(
            notes("- fix: _emphasis_ and _words_, keep `my_api_key` and `__dirname`"),
            vec!["fix: emphasis and words, keep my_api_key and __dirname"]
        );
        assert_eq!(github_error(403, "API rate limit exceeded", false), "GitHub rate limit reached; set GITHUB_TOKEN");
        assert_eq!(github_error(403, "Forbidden", false), "GitHub HTTP 403: Forbidden");
        assert!(has_changelog("Pi") && !has_changelog("Grok"));
    }

    #[test]
    #[ignore = "read-only live GitHub verification"]
    fn live_t3_range() {
        let releases = changelog("T3", "0.0.46-nightly.20261003.2632", "0.0.46-nightly.20261004.2648").unwrap();
        assert_eq!(releases.len(), 3);
        for release in releases {
            println!("{}: {} changes; {}", release.tag, release.notes.len(), release.notes[0]);
            assert!(
                !release
                    .notes
                    .iter()
                    .any(|n| n.contains("https://github.com/pingdotgg/t3code/pull/") || n.contains(" by @"))
            );
        }
    }
}
