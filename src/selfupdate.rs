//! t3up updating itself: the newest GitHub release, the build for this machine, checked against the
//! SHA-256 published beside it, swapped in for the running executable.
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::model::newer;

const REPO: &str = "hkarlsen06/t3up";

/// This build's version; `T3UP_SELF_VERSION` pretends otherwise (to try an update for real).
pub fn current() -> String {
    std::env::var("T3UP_SELF_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").into())
}

/// Where releases are listed: GitHub, or `$T3UP_RELEASES_API`; `off` turns the check off (tests).
fn api() -> Option<String> {
    match std::env::var("T3UP_RELEASES_API") {
        Ok(v) if v == "off" => None,
        Ok(v) if !v.is_empty() => Some(v),
        _ => Some(format!("https://api.github.com/repos/{REPO}/releases/latest")),
    }
}

/// The release build for this machine, as the release workflow names it.
pub fn target() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        _ => return None,
    })
}

/// The archive's file name for a target.
pub fn asset(target: &str) -> String {
    let ext = if target.contains("windows") { "zip" } else { "tar.gz" };
    format!("t3up-{target}.{ext}")
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(60))).build().into()
}

/// The newest release's version ('0.3.0') if it's newer than this one. Blocking; None when offline,
/// rate limited, turned off, or already current: an update hint is never worth an error.
pub fn available() -> Option<String> {
    let url = api()?;
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(5))).build().into();
    let body = agent.get(&url).header("User-Agent", "t3up").call().ok()?.body_mut().read_to_string().ok()?;
    let tag = serde_json::from_str::<serde_json::Value>(&body).ok()?["tag_name"].as_str()?.to_string();
    let version = tag.trim_start_matches('v').to_string();
    newer(&version, &current()).then_some(version)
}

/// Whether `bytes` match a published `.sha256` (its first word, any case).
pub fn verify(bytes: &[u8], published: &str) -> bool {
    let want = published.split_whitespace().next().unwrap_or("").to_lowercase();
    let got: String = Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect();
    want.len() == 64 && want == got
}

/// Put `new` (a file beside `exe`) in place of `exe`. A rename, so a running copy keeps its old file
/// (and macOS never sees a signed binary change under it). Windows can't replace a running .exe:
/// it moves aside first.
pub fn swap(new: &Path, exe: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(new, std::fs::Permissions::from_mode(0o755))?;
    }
    if cfg!(windows) {
        let old = exe.with_extension("old.exe");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(exe, &old)?;
        if let Err(e) = std::fs::rename(new, exe) {
            let _ = std::fs::rename(&old, exe);
            return Err(e);
        }
        Ok(())
    } else {
        std::fs::rename(new, exe)
    }
}

/// The real executable (through any symlink, like ~/.local/bin/t3up → somewhere).
pub fn executable() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("can't find t3up's own file: {e}"))?;
    Ok(std::fs::canonicalize(&exe).unwrap_or(exe))
}

/// Download, check and install `version` over the running executable. Blocking. Returns the version.
pub fn update(version: &str, say: &dyn Fn(&str)) -> Result<String, String> {
    let target = target().ok_or("there's no t3up build for this machine; use cargo install")?;
    let exe = executable()?;
    let dir = exe.parent().ok_or("t3up's own file has no folder")?;
    let name = asset(target);
    let base = format!("https://github.com/{REPO}/releases/download/v{version}");
    say(&format!("Downloading t3up {version}"));
    let get = |url: &str| -> Result<Vec<u8>, String> {
        let mut response = agent().get(url).header("User-Agent", "t3up").call().map_err(|e| format!("{url}: {e}"))?;
        response.body_mut().with_config().limit(200 << 20).read_to_vec().map_err(|e| format!("{url}: {e}"))
    };
    let archive = get(&format!("{base}/{name}"))?;
    let published = String::from_utf8(get(&format!("{base}/{name}.sha256"))?).unwrap_or_default();
    if !verify(&archive, &published) {
        return Err("the download doesn't match its published checksum".into());
    }
    // Unpack beside the executable: same disk, so the swap is a rename.
    let work = tempdir_in(dir)
        .map_err(|e| format!("can't write to {}: {e} (installed by root? update with sudo)", dir.display()))?;
    let result = (|| {
        std::fs::write(work.join(&name), &archive).map_err(|e| e.to_string())?;
        let status = std::process::Command::new("tar")
            .args(["-xf", &name])
            .current_dir(&work)
            .status()
            .map_err(|e| format!("tar: {e}"))?;
        if !status.success() {
            return Err("couldn't unpack the download".to_string());
        }
        let bin = if cfg!(windows) { "t3up.exe" } else { "t3up" };
        let new = work.join(format!("t3up-{target}")).join(bin);
        let ok = std::process::Command::new(&new).arg("--version").output().is_ok_and(|o| o.status.success());
        if !ok {
            return Err("the new t3up doesn't run on this machine".into());
        }
        swap(&new, &exe).map_err(|e| format!("can't replace {}: {e}", exe.display()))
    })();
    let _ = std::fs::remove_dir_all(&work);
    result.map(|()| version.to_string())
}

fn tempdir_in(dir: &Path) -> std::io::Result<PathBuf> {
    let work = dir.join(format!(".t3up-update-{}", std::process::id()));
    std::fs::create_dir_all(&work)?;
    Ok(work)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_and_names() {
        let sum: String = Sha256::digest(b"t3up").iter().map(|b| format!("{b:02x}")).collect();
        assert!(verify(b"t3up", &format!("{}\n", sum.to_uppercase())));
        assert!(!verify(b"t3up!", &sum));
        assert!(!verify(b"t3up", ""));
        assert_eq!(asset("aarch64-apple-darwin"), "t3up-aarch64-apple-darwin.tar.gz");
        assert_eq!(asset("x86_64-pc-windows-msvc"), "t3up-x86_64-pc-windows-msvc.zip");
        assert!(target().is_some(), "CI's machines all have a build");
    }

    #[test]
    fn swap_replaces_the_file() {
        let dir = std::env::temp_dir().join(format!("t3up-selfswap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (exe, new) = (dir.join("t3up"), dir.join("t3up-new"));
        std::fs::write(&exe, "old").unwrap();
        std::fs::write(&new, "new").unwrap();
        swap(&new, &exe).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new");
        assert!(!new.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
