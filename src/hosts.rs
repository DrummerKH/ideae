//! Projects on other machines, reached by ssh. A remote project's path,
//! and its tasks', is a key: `ssh:/<host>/<path there>`. It is never a path
//! on this machine, so whatever would read it here finds nothing there,
//! instead of another project's files that happen to have the same path.

use std::path::{Path, PathBuf};

const SCHEME: &str = "ssh:";

/// The host and the path on it, of a remote key.
pub fn split(path: &Path) -> Option<(String, PathBuf)> {
    let mut parts = path.components();
    if parts.next()?.as_os_str() != SCHEME {
        return None;
    }
    let host = parts.next()?.as_os_str().to_str()?.to_string();
    Some((host, Path::new("/").join(parts.as_path())))
}

/// The key of `there` on `host`.
pub fn join(host: &str, there: &Path) -> PathBuf {
    let there = there.strip_prefix("/").unwrap_or(there);
    PathBuf::from(format!("{SCHEME}/{host}")).join(there)
}

pub fn is_remote(path: &Path) -> bool {
    split(path).is_some()
}

/// The path on its host of a remote key; a local path as it is.
pub fn there(path: &Path) -> PathBuf {
    split(path).map_or_else(|| path.to_path_buf(), |(_, there)| there)
}

/// `ssh://[user@]host[:port]/path` from the command line: the host as keys
/// name it, and the path there.
pub fn parse_url(url: &str) -> Option<(String, PathBuf)> {
    let rest = url.strip_prefix("ssh://")?;
    let (host, path) = rest.split_once('/')?;
    if host.is_empty() || host.contains(char::is_whitespace) {
        return None;
    }
    Some((host.to_string(), PathBuf::from(format!("/{path}"))))
}

/// What one types for a project there: `me@box:/home/me/proj` (as scp
/// takes it; `~/proj` too), or `ssh://me@box:2222/home/me/proj`.
pub fn parse_target(text: &str) -> Option<(String, String)> {
    let text = text.trim();
    if text.starts_with("ssh://") {
        return parse_url(text).map(|(host, path)| (host, path.display().to_string()));
    }
    let (host, path) = text.split_once(':')?;
    let fine = !host.is_empty() && !host.contains(['/', ' ']) && !path.trim().is_empty();
    fine.then(|| (host.to_string(), path.trim().to_string()))
}

/// How ssh reaches `host` (`user@box:2222`): `IDEAE_SSH` (a command with
/// options of its own, `ssh -i key`) or `ssh`, then its port, then the
/// rest. The host may be an alias of `~/.ssh/config`.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn ssh_for(host: &str) -> Vec<String> {
    let mut ssh: Vec<String> = std::env::var("IDEAE_SSH")
        .ok()
        .map(|s| s.split_whitespace().map(String::from).collect::<Vec<_>>())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| vec!["ssh".into()]);
    match host.rsplit_once(':').filter(|(_, port)| !port.is_empty() && port.chars().all(|c| c.is_ascii_digit())) {
        Some((name, port)) => ssh.extend(["-p".into(), port.into(), name.into()]),
        None => ssh.push(host.into()),
    }
    ssh
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_key_is_no_path_here_and_tells_its_host() {
        let key = join("me@box:2222", Path::new("/home/me/proj"));
        assert_eq!(key, PathBuf::from("ssh:/me@box:2222/home/me/proj"));
        assert!(!key.is_absolute());
        assert_eq!(split(&key), Some(("me@box:2222".into(), PathBuf::from("/home/me/proj"))));
        // A task's worktree under it, and a file of the task.
        let task = key.join("../proj.worktrees/fix");
        assert_eq!(there(&task.join("src/a.rs")), PathBuf::from("/home/me/proj/../proj.worktrees/fix/src/a.rs"));
        assert!(!is_remote(Path::new("/home/me/proj")));
        assert_eq!(there(Path::new("/home/me/proj")), PathBuf::from("/home/me/proj"));
    }

    #[test]
    fn an_ssh_url_names_its_host_and_path() {
        assert_eq!(parse_url("ssh://me@box:2222/home/me/proj"), Some(("me@box:2222".into(), PathBuf::from("/home/me/proj"))));
        assert_eq!(parse_url("ssh://box/srv/x"), Some(("box".into(), PathBuf::from("/srv/x"))));
        assert_eq!(parse_url("/home/me/proj"), None);
        assert_eq!(ssh_for("me@box:2222")[1..], ["-p", "2222", "me@box"]);
        assert_eq!(ssh_for("box")[1..], ["box"]);
        assert_eq!(parse_target("me@box:/home/me/proj"), Some(("me@box".into(), "/home/me/proj".into())));
        assert_eq!(parse_target(" box:~/proj "), Some(("box".into(), "~/proj".into())));
        assert_eq!(parse_target("ssh://me@box:2222/srv/x"), Some(("me@box:2222".into(), "/srv/x".into())));
        assert_eq!(parse_target("/home/me/proj"), None);
        assert_eq!(parse_target("box:"), None);
    }
}
