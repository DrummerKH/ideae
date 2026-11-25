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
    if host.is_empty() || host.contains(char::is_whitespace) || host.matches(HOP).count() > 1 {
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
    // At the last ':' not followed by a port alone: `hub:2222>mac:/p`, and `box:2222` is no path.
    let (host, path) = text.rsplit_once(':').filter(|(_, rest)| !rest.trim().chars().all(|c| c.is_ascii_digit()))?;
    // One host on the way at most.
    let fine = !host.is_empty() && !host.contains(['/', ' ']) && host.matches(HOP).count() <= 1 && !path.trim().is_empty();
    fine.then(|| (host.to_string(), path.trim().to_string()))
}

/// A host reached through another, `me@hub>me@mac`: the one ssh reaches
/// from here, and the one that one reaches with its own keys and config.
pub fn hop(host: &str) -> (&str, Option<&str>) {
    match host.split_once(HOP) {
        Some((first, then)) => (first.trim(), Some(then.trim()).filter(|t| !t.is_empty())),
        None => (host, None),
    }
}

const HOP: char = '>';

/// `user@box:2222` as ssh's arguments: its port, then the rest.
pub fn destination(host: &str) -> Vec<String> {
    match host.rsplit_once(':').filter(|(_, port)| !port.is_empty() && port.chars().all(|c| c.is_ascii_digit())) {
        Some((name, port)) => vec!["-p".into(), port.into(), name.into()],
        None => vec![host.into()],
    }
}

/// How ssh reaches `host` (`user@box:2222`): `IDEAE_SSH` (a command with
/// options of its own, `ssh -i key`) or `ssh`, then its port, then the
/// rest. The host may be an alias of `~/.ssh/config`; one reached through
/// another ([`hop`]) is that other's.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn ssh_for(host: &str) -> Vec<String> {
    let host = hop(host).0;
    let mut ssh: Vec<String> = std::env::var("IDEAE_SSH")
        .ok()
        .map(|s| s.split_whitespace().map(String::from).collect::<Vec<_>>())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| vec!["ssh".into()]);
    ssh.extend(destination(host));
    ssh
}

/// A host as people read it: `mac via hub` for one reached through another.
pub fn label(host: &str) -> String {
    match hop(host) {
        (first, Some(beyond)) => format!("{beyond} via {first}"),
        (first, None) => first.to_string(),
    }
}

/// Printed by a first host when its own ssh to the next fails ([`hop`]):
/// which leg of the way failed.
pub const BEYOND_FAILED: &str = "ideae: the first host's ssh to the next failed";

/// Added to what ssh said when ssh itself failed (its 255), not a command it ran.
pub const SSH_FAILED: &str = "ideae: ssh failed";

/// What ssh said when reaching `host` failed, in words that tell what to
/// do: the usual failures (no key taken, a name not known, a port closed,
/// a host gone quiet or not met, forwarding not allowed); through another
/// host, which leg failed. What ssh said follows.
pub fn explain_ssh(host: &str, said: &str) -> String {
    // What a command there said, ssh having done its part, is told as it is.
    if !said.contains(SSH_FAILED) && !said.contains(BEYOND_FAILED) {
        return said.trim().to_string();
    }
    let said = &said.replace(SSH_FAILED, "");
    let (first, beyond) = hop(host);
    let beyond_failed = said.contains(BEYOND_FAILED);
    let (target, from) = match beyond {
        Some(beyond) if beyond_failed => (beyond, Some(first)),
        _ => (first, None),
    };
    let lower = said.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| lower.contains(w));
    let try_it = match from {
        Some(from) => format!("`ssh {target}` on {from} once shows why ({from} reaches it with its own keys and config, asking nothing)"),
        None => format!("`ssh {target}` in a terminal shows why"),
    };
    let what = if has(&["remote host identification has changed"]) {
        format!("{target}'s key is not the one known for it. If it was set up again, `ssh-keygen -R {target}` forgets the old one")
    } else if has(&["host key verification failed"]) {
        match from {
            Some(from) => format!("{from} has not met {target} yet: `ssh {target}` on {from} once, to trust its key"),
            None => format!("{target}'s key was not trusted"),
        }
    } else if has(&["too many authentication failures"]) {
        format!("ssh offered {target} too many keys before the right one: name it for {target} in ~/.ssh/config (IdentityFile, IdentitiesOnly yes)")
    } else if has(&["permission denied"]) {
        format!("{target} took no key or password: {try_it}")
    } else if has(&["could not resolve hostname", "name or service not known", "nodename nor servname"]) {
        format!("no machine named {target} is known: check the name, or its alias in ~/.ssh/config")
    } else if has(&["connection refused"]) {
        format!("{target} takes no ssh connections on that port")
    } else if has(&["timed out", "no route to host", "network is unreachable", "host is down"]) {
        format!("{target} does not answer: off, asleep, or out of reach")
    } else if has(&["administratively prohibited", "forwarding disabled", "port forwarding failed"]) {
        format!("{target} does not allow forwarding ports (AllowTcpForwarding in its sshd_config)")
    } else if beyond_failed {
        format!("{} could not reach {target}: {try_it}", from.unwrap_or(first))
    } else {
        return said.replace(BEYOND_FAILED, "").trim().to_string();
    };
    let detail: Vec<&str> = said.lines().map(str::trim).filter(|l| !l.is_empty() && !l.contains(BEYOND_FAILED)).collect();
    let detail = detail.get(detail.len().saturating_sub(3)..).unwrap_or_default().join(" ");
    if detail.is_empty() { format!("{what}.") } else { format!("{what}.\nssh said: {detail}") }
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
        // Through another host: the first is ssh'd to from here, the second from it.
        assert_eq!(parse_target("me@hub>me@mac:/Users/me/proj"), Some(("me@hub>me@mac".into(), "/Users/me/proj".into())));
        assert_eq!(parse_url("ssh://hub:2222>mac:2200/srv/x"), Some(("hub:2222>mac:2200".into(), PathBuf::from("/srv/x"))));
        assert_eq!(hop("hub:2222>mac:2200"), ("hub:2222", Some("mac:2200")));
        assert_eq!(hop("box"), ("box", None));
        assert_eq!(parse_target("a>b>c:/p"), None, "one host on the way at most");
        assert_eq!(parse_url("ssh://a>b>c/p"), None);
        assert_eq!(ssh_for("hub:2222>mac:2200")[1..], ["-p", "2222", "hub"]);
        assert_eq!(destination("mac:2200"), ["-p", "2200", "mac"]);
        assert_eq!(label("me@hub>me@mac"), "me@mac via me@hub");
        assert_eq!(label("box"), "box");
        let key = join("hub>mac", Path::new("/srv/x"));
        assert_eq!(split(&key), Some(("hub>mac".into(), PathBuf::from("/srv/x"))));
    }

    #[test]
    fn what_ssh_said_is_told_in_words_with_its_leg() {
        let ssh = |said: &str| format!("{said}\n{SSH_FAILED}");
        assert_eq!(explain_ssh("box", "cd: /root/x: Permission denied"), "cd: /root/x: Permission denied", "a command's, not ssh's");
        assert_eq!(parse_target("box:2222"), None, "a port, no path");
        assert_eq!(parse_target("hub:2222>mac:/p"), Some(("hub:2222>mac".into(), "/p".into())));
        let denied = explain_ssh("box", &ssh("me@box: Permission denied (publickey)."));
        assert!(denied.starts_with("box took no key or password: `ssh box` in a terminal") && denied.ends_with("ssh said: me@box: Permission denied (publickey)."), "{denied}");
        let beyond = explain_ssh("hub>mac", &format!("mac: Permission denied (publickey).\n{BEYOND_FAILED}"));
        assert!(beyond.starts_with("mac took no key or password: `ssh mac` on hub once"), "{beyond}");
        let first = explain_ssh("hub>mac", &ssh("ssh: connect to host hub port 22: Connection refused"));
        assert!(first.starts_with("hub takes no ssh connections"), "{first}");
        assert!(explain_ssh("hub>mac", &format!("Host key verification failed.\n{BEYOND_FAILED}")).starts_with("hub has not met mac yet"));
        assert!(explain_ssh("box", &ssh("channel 2: open failed: administratively prohibited: open failed")).starts_with("box does not allow forwarding ports"));
        assert_eq!(explain_ssh("box", &ssh("something else entirely")), "something else entirely");
    }
}
