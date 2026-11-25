//! The ideae of a server: `ideae-server` of this window's build, put on a
//! machine reached by ssh the first time it is needed, under its own hash,
//! and run from there. The binary comes from this machine (a release's,
//! downloaded once; or one built from the checkout), so a server needs no
//! network of its own.

use std::path::PathBuf;

use super::remote::{Remote, run};

/// A server's system, as releases name their builds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub os: &'static str,
    pub arch: &'static str,
}

impl Target {
    /// From `uname -sm`.
    fn parse(uname: &str) -> Result<Target, String> {
        let mut words = uname.split_whitespace();
        let os = match words.next() {
            Some("Linux") => "linux",
            Some("Darwin") => "macos",
            other => return Err(format!("no ideae-server for {} yet", other.unwrap_or("this system"))),
        };
        let arch = match words.next() {
            Some("x86_64" | "amd64") => "x86_64",
            Some("aarch64" | "arm64") => "aarch64",
            other => return Err(format!("no ideae-server for {os} on {} yet", other.unwrap_or("this machine"))),
        };
        Ok(Target { os, arch })
    }

    /// `x86_64-linux`, as a release's file ends. The other way round from
    /// the app's own files (`-linux-x86_64`): an updater that takes a file
    /// by its end must never take a server's.
    pub fn name(self) -> String {
        format!("{}-{}", self.arch, self.os)
    }

    /// The target it is built for.
    fn triple(self) -> String {
        match self.os {
            "linux" => format!("{}-unknown-linux-musl", self.arch),
            _ => format!("{}-apple-darwin", self.arch),
        }
    }
}

/// Makes sure `ideae-server` of this build is on `remote`'s host, and
/// the settings its agents report through ([`crate::agents::Hooks`]:
/// Claude Code's hooks, OpenCode's plugin, Codex's notify, all running it
/// there); returns `remote` set to run it, and those hooks. Off the UI
/// thread: it runs ssh, and may download a release.
pub fn ensure(mut remote: Remote) -> Result<(Remote, crate::agents::Hooks), String> {
    // The last lines: a login shell may greet before them.
    let probe = run(&remote, "uname -sm; printf '%s\\n' \"$HOME\"", None)?;
    let mut last = probe.lines().rev();
    let (home, uname) = (last.next().unwrap_or_default().to_string(), last.next().unwrap_or_default().to_string());
    if !home.starts_with('/') {
        return Err(format!("{}: no home folder there", remote.name()));
    }
    let target = Target::parse(&uname)?;
    let binary = local_binary(target)?;
    let bytes = std::fs::read(&binary).map_err(|e| format!("{}: {e}", binary.display()))?;
    use sha2::Digest as _;
    let hash: String = sha2::Sha256::digest(&bytes).iter().take(8).map(|b| format!("{b:02x}")).collect();
    let app = crate::state::APP_ID;
    let dir = PathBuf::from(&home).join(".local/share").join(app).join("server");
    let exe = dir.join(format!("ideae-server-{hash}"));
    let q = |path: &std::path::Path| format!("'{}'", path.display().to_string().replace('\'', r"'\''"));
    // Sent only when it is not there yet; then into a file of its own,
    // renamed into place, so a second window never runs half of one.
    let put = |path: &std::path::Path, executable: bool| {
        let (path, dir) = (q(path), q(&dir));
        let mode = if executable { "755" } else { "644" };
        format!("mkdir -p {dir} && cat > {path}.$$ && chmod {mode} {path}.$$ && mv -f {path}.$$ {path}")
    };
    run(&remote, &format!("test -x {} && exit 0; {}", q(&exe), put(&exe, true)), Some(&bytes))?;
    // The hooks' files, written each time: small, and they follow this window's build.
    let settings = dir.join(format!("hooks-{hash}.json"));
    let plugin = settings.with_extension("opencode.js");
    let json = serde_json::to_vec_pretty(&crate::agent::hooks_settings(&exe)).map_err(|e| e.to_string())?;
    run(&remote, &put(&settings, false), Some(&json))?;
    run(&remote, &put(&plugin, false), Some(crate::agents::opencode::plugin_source(&exe).as_bytes()))?;
    // A daemon of its own there, apart from one an ideae of that machine
    // runs for its own window (`<app>-remote`): neither hands the other over.
    remote.ideae = format!("IDEAE_APP={app}-remote {}", q(&exe));
    let hooks = crate::agents::Hooks { claude_settings: Some(settings), exe: Some(exe), opencode_plugin: Some(plugin), claude_mcp: None };
    Ok((remote, hooks))
}

/// This build's `ideae-server` for `target`: from `IDEAE_SERVER_DIR`
/// (`ideae-server-x86_64-linux` and so on), else built from the checkout
/// (`scripts/server.sh`), else this release's, downloaded once.
fn local_binary(target: Target) -> Result<PathBuf, String> {
    let name = format!("ideae-server-{}", target.name());
    if let Some(dir) = std::env::var_os("IDEAE_SERVER_DIR") {
        let path = PathBuf::from(dir).join(&name);
        return if path.is_file() { Ok(path) } else { Err(format!("no {}", path.display())) };
    }
    let built = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/server").join(target.triple()).join("release/ideae-server");
    if built.is_file() {
        return Ok(built);
    }
    if let Some(release) = crate::update::server_release(target) {
        return Ok(release?);
    }
    Err(format!("no {name} for this build: scripts/server.sh builds it"))
}

#[cfg(test)]
mod tests {
    use super::Target;
    use crate::daemon::client::PipeEvent;
    use crate::daemon::proto::Spec;
    use std::time::{Duration, Instant};

    /// Against a real server: `IDEAE_LIVE_REMOTE="ssh -p 2222 -i key me@host"`.
    /// Its ideae-server is put there (or found), a session runs, the link
    /// is cut and comes back, the window goes and another finds the session.
    ///
    /// `scripts/server.sh && IDEAE_LIVE_REMOTE=... cargo test -- --ignored a_real_server`
    #[test]
    #[ignore]
    fn a_real_server_gets_its_ideae_and_keeps_a_session() {
        let Ok(ssh) = std::env::var("IDEAE_LIVE_REMOTE") else { return };
        let ssh: Vec<String> = ssh.split_whitespace().map(String::from).collect();
        let started = Instant::now();
        let (remote, _) = super::ensure(super::Remote::new(ssh.clone())).expect("installed");
        let first = started.elapsed();
        // Found the second time, not sent again.
        let started = Instant::now();
        let (again, _) = super::ensure(super::Remote::new(ssh)).expect("found");
        assert_eq!(again.ideae, remote.ideae);
        eprintln!("installed in {first:?}, found in {:?}: {}", started.elapsed(), remote.ideae);

        let (daemon, _) = super::super::remote::connect(remote.clone(), |_| {}).expect("connected");
        let id = format!("live#{}", std::process::id());
        let spec = Spec { cwd: "/tmp".into(), env: Vec::new(), input: None, program: vec!["sh".into(), "-c".into(), "while read l; do echo got:$l:$(uname -m); done".into()] };
        let events = daemon.pipe(id.clone(), spec.clone());
        let read = |want: &str| {
            let deadline = Instant::now() + Duration::from_secs(30);
            let (mut got, mut back) = (String::new(), false);
            while Instant::now() < deadline && !got.contains(want) {
                match events.try_recv() {
                    Ok(PipeEvent::Bytes(b)) => got.push_str(&String::from_utf8_lossy(&b)),
                    Ok(PipeEvent::Reattached) => back = true,
                    Ok(PipeEvent::Exited) => panic!("ended: {got}"),
                    Err(_) => std::thread::sleep(Duration::from_millis(20)),
                }
            }
            (got, back)
        };
        // Calls: a program run there, a file written, read and looked at there.
        use crate::daemon::proto::Call;
        let (reply, out) = daemon.call(&Call::Exec { cwd: "/".into(), program: "uname".into(), args: vec!["-m".into()], env: Vec::new() }, b"").unwrap();
        assert_eq!(reply.code, Some(0));
        eprintln!("uname there: {}", String::from_utf8_lossy(&out).trim());
        let file = std::path::PathBuf::from(format!("/tmp/ideae-live-{}", std::process::id()));
        assert!(daemon.call(&Call::Write { path: file.clone() }, "é\n".as_bytes()).unwrap().0.error.is_none());
        assert_eq!(daemon.call(&Call::Read { path: file.clone() }, b"").unwrap().1, "é\n".as_bytes());
        assert_eq!(daemon.call(&Call::Stat { path: file.clone() }, b"").unwrap().0.stat.unwrap().len, 3);
        let _ = daemon.call(&Call::Exec { cwd: "/".into(), program: "rm".into(), args: vec![file.display().to_string()], env: Vec::new() }, b"");
        // A folder watched there: a file made in it is told.
        let folder = format!("/tmp/ideae-live-watch-{}", std::process::id());
        let sh = |line: String| daemon.call(&Call::Exec { cwd: "/".into(), program: "sh".into(), args: vec!["-c".into(), line], env: Vec::new() }, b"").unwrap();
        sh(format!("mkdir -p {folder}"));
        let (tx, changed) = async_channel::unbounded();
        daemon.on_change("live", tx);
        daemon.watch("live", vec![folder.clone().into()], Vec::new());
        std::thread::sleep(Duration::from_millis(300));
        sh(format!("echo x > {folder}/new.txt"));
        let deadline = Instant::now() + Duration::from_secs(10);
        while changed.try_recv().is_err() {
            assert!(Instant::now() < deadline, "the watcher there told of the new file");
            std::thread::sleep(Duration::from_millis(20));
        }
        eprintln!("watched there");
        daemon.unwatch("live");
        sh(format!("rm -rf {folder}"));

        daemon.write(&id, b"one\n");
        let (got, _) = read("got:one");
        assert!(got.contains("got:one"), "{got}");
        eprintln!("{}", got.trim());

        daemon.wire().unwrap().drop_link_for_test();
        // Typed while the link is down is lost: waited for its return.
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && !matches!(events.try_recv(), Ok(PipeEvent::Reattached)) {
            std::thread::sleep(Duration::from_millis(20));
        }
        daemon.write(&id, b"two\n");
        assert!(read("got:two").0.contains("got:two"), "the session ran on after the link was cut");
        daemon.disconnect();

        let (daemon, _) = super::super::remote::connect(remote, |_| {}).expect("connected again");
        let events = daemon.pipe(id.clone(), spec);
        daemon.write(&id, b"three\n");
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut got = String::new();
        while Instant::now() < deadline && !got.contains("got:three") {
            if let Ok(PipeEvent::Bytes(b)) = events.try_recv() {
                got.push_str(&String::from_utf8_lossy(&b));
            } else {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(got.contains("got:three"), "another window found the session: {got}");
        daemon.kill(&id);
        std::thread::sleep(Duration::from_millis(300));
        daemon.disconnect();
    }

    #[test]
    fn a_server_s_system_is_named_as_releases_name_it() {
        assert_eq!(Target::parse("Linux x86_64").unwrap().name(), "x86_64-linux");
        assert_eq!(Target::parse("Linux aarch64\n").unwrap().name(), "aarch64-linux");
        assert_eq!(Target::parse("Darwin arm64").unwrap().name(), "aarch64-macos");
        // An updater that takes the file ending as this system's app does
        // (all that releases before 2026-11 do) never takes a server's.
        for os in ["Linux", "Darwin"] {
            for arch in ["x86_64", "aarch64"] {
                let server = format!("ideae-server-20261124000000-{}", Target::parse(&format!("{os} {arch}")).unwrap().name());
                for app in ["-linux-x86_64", "-macos-arm64", "-macos-x86_64", "-windows-x86_64.exe"] {
                    assert!(!server.ends_with(app), "{server} ends as {app}");
                }
            }
        }
        assert_eq!(Target::parse("Darwin arm64").unwrap().triple(), "aarch64-apple-darwin");
        assert!(Target::parse("FreeBSD amd64").is_err());
    }
}
