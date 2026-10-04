//! The T3 Code desktop app on this machine (macOS).
use std::sync::LazyLock;

use regex::Regex;

/// The installed app's version, '' if none (or not macOS).
pub fn desktop_version() -> String {
    #[cfg(target_os = "macos")]
    {
        macos::desktop_version()
    }
    #[cfg(not(target_os = "macos"))]
    {
        String::new()
    }
}

/// Install the newest release on the app's own update channel. Blocking. Returns the version.
pub fn update_desktop(say: &dyn Fn(&str)) -> Result<String, String> {
    #[cfg(target_os = "macos")]
    {
        macos::update_desktop(say).map_err(|e| e.to_string())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = say;
        Err("T3 Code desktop is not installed in /Applications".into())
    }
}

static VERSION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^version: *'?([^'\s]+)").unwrap());

/// The release manifest's version, URL and SHA-512 of the zip for this machine's chip.
pub fn mac_zip(manifest: &str, base: &str) -> Result<(String, String, String), String> {
    mac_zip_arch(manifest, base, if std::env::consts::ARCH == "aarch64" { "arm64" } else { "x64" })
}

fn mac_zip_arch(manifest: &str, base: &str, arch: &str) -> Result<(String, String, String), String> {
    let version = VERSION.captures(manifest).ok_or("release manifest has no version")?[1].to_string();
    let pattern = Regex::new(&format!(r"- url: (\S+-{arch}\.zip)\s+sha512: (\S+)")).unwrap();
    let asset = pattern.captures(manifest).ok_or_else(|| format!("release {version} has no {arch} zip"))?;
    Ok((version, format!("{base}/{}", &asset[1]), asset[2].into()))
}

#[cfg(target_os = "macos")]
mod macos {
    use std::{
        collections::HashMap,
        fs,
        io::{Read, Write},
        path::{Path, PathBuf},
        process::Command,
        thread,
        time::Duration,
    };

    use anyhow::{Context, Result, bail};
    use base64::{Engine, engine::general_purpose::STANDARD};
    use sha2::{Digest, Sha512};

    use super::*;
    use crate::changelog::github_json;

    fn command(command: &mut Command) -> Result<std::process::Output> {
        let output = command.output().with_context(|| format!("cannot run {command:?}"))?;
        if !output.status.success() {
            bail!("{command:?}: {}", String::from_utf8_lossy(&output.stderr).trim());
        }
        Ok(output)
    }

    fn desktop_app() -> PathBuf {
        let processes = Command::new("ps")
            .args(["-Ao", "comm="])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        let pattern = Regex::new(r"(?m)^(/Applications/T3 Code[^/]*\.app)/").unwrap();
        PathBuf::from(
            pattern
                .captures(&processes)
                .map(|m| m[1].to_string())
                .unwrap_or_else(|| "/Applications/T3 Code (Nightly).app".into()),
        )
    }

    fn bundle_info(app: &Path) -> plist::Dictionary {
        plist::Value::from_file(app.join("Contents/Info.plist"))
            .ok()
            .and_then(plist::Value::into_dictionary)
            .unwrap_or_default()
    }

    fn version(info: &plist::Dictionary) -> String {
        info.get("CFBundleShortVersionString").and_then(plist::Value::as_string).unwrap_or_default().into()
    }

    pub(super) fn desktop_version() -> String {
        version(&bundle_info(&desktop_app()))
    }

    fn team(app: &Path) -> Result<String> {
        if !Command::new("codesign").args(["--verify", "--deep", "--strict"]).arg(app).output()?.status.success() {
            return Ok(String::new());
        }
        let details = Command::new("codesign").arg("-dv").arg(app).output()?;
        let pattern = Regex::new(r"(?m)^TeamIdentifier=([A-Z0-9]{10})$").unwrap();
        Ok(pattern.captures(&String::from_utf8_lossy(&details.stderr)).map(|m| m[1].to_string()).unwrap_or_default())
    }

    fn desktop_release(app: &Path) -> Result<(String, String, String)> {
        let config = fs::read_to_string(app.join("Contents/Resources/app-update.yml"))?;
        let pattern = Regex::new(r"(?m)^(\w+): *(\S+)").unwrap();
        let config: HashMap<_, _> =
            pattern.captures_iter(&config).map(|m| (m[1].to_string(), m[2].to_string())).collect();
        let owner = config.get("owner").context("app-update.yml has no owner")?;
        let repo = config.get("repo").context("app-update.yml has no repo")?;
        let channel = config.get("channel").map(String::as_str).unwrap_or("latest");
        let manifest = format!("{channel}-mac.yml");
        let github = format!("https://github.com/{owner}/{repo}/releases");
        let base = if channel == "latest" {
            format!("{github}/latest/download")
        } else {
            let releases =
                github_json(&format!("/repos/{owner}/{repo}/releases?per_page=30")).map_err(anyhow::Error::msg)?;
            let tag = releases
                .as_array()
                .context("GitHub returned invalid release data")?
                .iter()
                .find(|r| r["assets"].as_array().is_some_and(|assets| assets.iter().any(|a| a["name"] == manifest)))
                .and_then(|r| r["tag_name"].as_str())
                .with_context(|| format!("no release on the {channel} channel"))?;
            format!("{github}/download/{tag}")
        };
        let agent: ureq::Agent =
            ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(20))).build().into();
        let manifest = agent.get(format!("{base}/{manifest}")).call()?.body_mut().read_to_string()?;
        mac_zip(&manifest, &base).map_err(anyhow::Error::msg)
    }

    // Same volume as the app, so replacing it takes two renames.
    struct Work(PathBuf);
    impl Drop for Work {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn busy(app: &Path) -> Result<bool> {
        let commands = command(Command::new("ps").args(["-Ao", "command="]))?;
        let prefix = format!("{}/Contents/MacOS/", app.display());
        Ok(String::from_utf8_lossy(&commands.stdout).lines().any(|s| s.starts_with(&prefix)))
    }

    fn swap(app: &Path, new: &Path, backup: &Path) -> Result<()> {
        fs::rename(app, backup).context("cannot move the installed app aside")?;
        if let Err(error) = fs::rename(new, app) {
            if let Err(restore) = fs::rename(backup, app) {
                bail!(
                    "cannot install app: {error}; cannot restore app: {restore}; old app kept at {}",
                    backup.display()
                );
            }
            return Err(error).context("cannot install app; old app restored");
        }
        Ok(())
    }

    pub(super) fn update_desktop(say: &dyn Fn(&str)) -> Result<String> {
        let app = desktop_app();
        let info = bundle_info(&app);
        if info.is_empty() {
            bail!("T3 Code desktop is not installed in /Applications");
        }
        let (latest, url, checksum) = desktop_release(&app)?;
        if latest == version(&info) {
            return Ok(latest);
        }
        let parent = app.parent().context("app has no parent folder")?;
        let work = command(Command::new("mktemp").arg("-d").arg(parent.join(".t3up-XXXXXXXX")))?;
        let work = Work(PathBuf::from(String::from_utf8(work.stdout)?.trim()));
        let zip = work.0.join("app.zip");
        say(&format!("Downloading T3 Code {latest}"));
        let agent: ureq::Agent =
            ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(60))).build().into();
        let mut response = agent.get(&url).call()?.into_body().into_reader();
        let mut file = fs::File::create(&zip)?;
        let mut digest = Sha512::new();
        let mut buffer = vec![0; 1 << 20];
        loop {
            let n = response.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            digest.update(&buffer[..n]);
            file.write_all(&buffer[..n])?;
        }
        drop(file);
        if STANDARD.encode(digest.finalize()) != checksum {
            bail!("download does not match the release checksum");
        }
        let extracted = work.0.join("new");
        command(Command::new("ditto").args(["-x", "-k"]).arg(&zip).arg(&extracted))?;
        let new = fs::read_dir(extracted)?
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .map(|entry| entry.path())
            .find(|p| p.extension().is_some_and(|s| s == "app"))
            .context("download contains no app")?;
        let new_team = team(&new)?;
        if new_team.is_empty() || new_team != team(&app)? {
            bail!("the new app is not signed by the same developer as the installed one");
        }
        let running = busy(&app)?;
        if running {
            let id = info
                .get("CFBundleIdentifier")
                .and_then(plist::Value::as_string)
                .context("app has no bundle identifier")?;
            // Quote an AppleScript string rather than interpolating the bundle's contents as code.
            let id = id.replace('\\', "\\\\").replace('"', "\\\"");
            say("Quitting T3 Code to install the update");
            command(Command::new("osascript").args(["-e", &format!("tell application id \"{id}\" to quit")]))?;
        }
        let mut stopped = false;
        for _ in 0..120 {
            let processes = command(Command::new("ps").args(["-Ao", "comm="]))?;
            if !busy(&app)? && !String::from_utf8_lossy(&processes.stdout).contains("ShipIt") {
                stopped = true;
                break;
            }
            thread::sleep(Duration::from_millis(500));
        }
        if !stopped {
            bail!("T3 Code did not quit");
        }
        let backup = work.0.join("old.app");
        if version(&bundle_info(&app)) != latest
            && let Err(error) = swap(&app, &new, &backup)
        {
            // Keep a backup that could not be restored, even when cleanup runs.
            if backup.exists() {
                std::mem::forget(work);
            }
            return Err(error);
        }
        if running && let Err(error) = command(Command::new("open").arg(&app)) {
            if backup.exists()
                && let Err(restore) = swap(&app, &backup, &work.0.join("failed-new.app"))
            {
                let reason = format!("cannot reopen app: {error}; {restore}; backup folder: {}", work.0.display());
                std::mem::forget(work);
                bail!(reason);
            }
            return Err(error);
        }
        Ok(latest)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn failed_swap_restores_old_app() {
            let root = std::env::temp_dir().join(format!("t3up-swap-{}", std::process::id()));
            fs::create_dir_all(&root).unwrap();
            let app = root.join("test.app");
            fs::write(&app, "old app").unwrap();
            assert!(swap(&app, &root.join("missing.app"), &root.join("old.app")).is_err());
            assert_eq!(fs::read_to_string(app).unwrap(), "old app");
            fs::remove_dir_all(root).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zip_for_this_chip_never_dmg() {
        let manifest = "version: '0.0.46-nightly.20261003.2632'\nfiles:\n  - url: T3-Code-0.0.46-arm64.zip\n    sha512: AAA==\n  - url: T3-Code-0.0.46-arm64.dmg\n    sha512: BBB==\n  - url: T3-Code-0.0.46-x64.zip\n    sha512: CCC==\n";
        for (arch, checksum) in [("arm64", "AAA=="), ("x64", "CCC==")] {
            assert_eq!(
                mac_zip_arch(manifest, "https://x", arch).unwrap(),
                (
                    "0.0.46-nightly.20261003.2632".into(),
                    format!("https://x/T3-Code-0.0.46-{arch}.zip"),
                    checksum.into(),
                )
            );
        }
        assert_eq!(
            mac_zip(manifest, "https://x").unwrap().2,
            if std::env::consts::ARCH == "aarch64" { "AAA==" } else { "CCC==" }
        );
        assert!(mac_zip("version: 1.0.0\n- url: app-arm64.dmg\n  sha512: AAA==", "https://x").is_err());
        assert!(mac_zip("files:", "https://x").is_err());
    }
}
