//! Updates for release builds: the latest GitHub release of the repository
//! this binary came from, downloaded next to the running binary and put in
//! its place. Local builds have the new-build notice instead.

use std::path::{Path, PathBuf};

use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub tag: String,
    pub asset: String,
    pub url: String,
    pub size: u64,
}

/// The release this binary is: its tag, a UTC time `yyyymmddhhmmss`.
/// IDEAE_UPDATE_FROM pretends to be an older one, for tests.
fn current() -> Option<u64> {
    std::env::var("IDEAE_UPDATE_FROM").ok().or(option_env!("IDEAE_VERSION").map(str::to_string))?.parse().ok()
}

pub fn enabled() -> bool {
    current().is_some()
}

fn repo() -> Option<String> {
    let url = env!("CARGO_PKG_REPOSITORY");
    url.strip_prefix("https://github.com/").map(|r| r.trim_end_matches('/').to_string())
}

/// This system's binary in a release.
fn suffix() -> &'static str {
    if cfg!(windows) { "-windows-x86_64.exe" } else { "-linux-x86_64" }
}

/// A newer release than this one, off the UI thread.
pub fn newer() -> Option<Release> {
    let current = current()?;
    let out = crate::procs::command("curl")
        .args(["-fsSL", "--max-time", "20", "-H", "Accept: application/vnd.github+json"])
        .arg(format!("https://api.github.com/repos/{}/releases/latest", repo()?))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let release = parse(&serde_json::from_slice(&out.stdout).ok()?)?;
    (release.tag.parse::<u64>().ok()? > current).then_some(release)
}

fn parse(v: &Value) -> Option<Release> {
    let tag = v["tag_name"].as_str()?.to_string();
    let asset = v["assets"].as_array()?.iter().find(|a| a["name"].as_str().is_some_and(|n| n.ends_with(suffix())))?;
    Some(Release {
        tag,
        asset: asset["name"].as_str()?.to_string(),
        url: asset["browser_download_url"].as_str()?.to_string(),
        size: asset["size"].as_u64().unwrap_or(0),
    })
}

/// "15.11 11:32" from a tag.
pub fn when(tag: &str) -> String {
    if tag.len() == 14 {
        format!("{}.{} {}:{}", &tag[6..8], &tag[4..6], &tag[8..10], &tag[10..12])
    } else {
        tag.to_string()
    }
}

/// Downloads `release` and puts it in place of the running binary.
pub fn install(release: &Release) -> Result<PathBuf, String> {
    let exe = crate::desktop::exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().ok_or("no directory")?;
    let download = dir.join(format!(".{}.download", release.asset));
    let status = crate::procs::command("curl")
        .args(["-fsSL", "--max-time", "600", "-o"])
        .arg(&download)
        .arg(&release.url)
        .status()
        .map_err(|e| format!("curl: {e}"))?;
    let size = std::fs::metadata(&download).map(|m| m.len()).unwrap_or(0);
    if !status.success() || (release.size > 0 && size != release.size) {
        let _ = std::fs::remove_file(&download);
        return Err(format!("the download failed ({size} of {} bytes)", release.size));
    }
    replace(&exe, &download)?;
    Ok(exe)
}

#[cfg(unix)]
fn replace(exe: &Path, new: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(new, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    // The running process keeps its copy; the path gets the new one.
    std::fs::rename(new, exe).map_err(|e| format!("cannot replace {}: {e}", exe.display()))
}

/// A running exe cannot be overwritten on Windows, but it can be renamed.
#[cfg(windows)]
fn replace(exe: &Path, new: &Path) -> Result<(), String> {
    let old = exe.with_extension("old.exe");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old).map_err(|e| format!("cannot move {} aside: {e}", exe.display()))?;
    std::fs::rename(new, exe).map_err(|e| {
        let _ = std::fs::rename(&old, exe);
        format!("cannot replace {}: {e}", exe.display())
    })
}

#[cfg(test)]
mod tests {
    use super::{parse, suffix, when};

    #[test]
    fn picks_this_systems_asset() {
        let v: serde_json::Value = serde_json::from_str(&format!(
            r#"{{"tag_name":"20261115113213","assets":[
                {{"name":"ideae-20261115113213-other","browser_download_url":"u0","size":1}},
                {{"name":"ideae-20261115113213{}","browser_download_url":"u1","size":7}}]}}"#,
            suffix()
        ))
        .unwrap();
        let r = parse(&v).unwrap();
        assert_eq!((r.url.as_str(), r.size), ("u1", 7));
        assert_eq!(when("20261115113213"), "15.11 11:32");
    }
}
