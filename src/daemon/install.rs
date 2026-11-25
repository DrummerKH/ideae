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
    super::remote::clear_first_host(&remote);
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
    (remote.exe, remote.home) = (Some(exe.clone()), Some(PathBuf::from(&home)));
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

    /// Against a real server: `IDEAE_LIVE_REMOTE="ssh -p 2222 -i key me@host"`,
    /// and `IDEAE_LIVE_HOP="me@other"` for one that server reaches.
    /// Its ideae-server is put there (or found), a session runs, the link
    /// is cut and comes back, the window goes and another finds the session.
    ///
    /// `scripts/server.sh && IDEAE_LIVE_REMOTE=... cargo test -- --ignored a_real_server`
    #[test]
    #[ignore]
    fn a_real_server_gets_its_ideae_and_keeps_a_session() {
        let Ok(ssh) = std::env::var("IDEAE_LIVE_REMOTE") else { return };
        let ssh: Vec<String> = ssh.split_whitespace().map(String::from).collect();
        // IDEAE_LIVE_HOP: a host that one reaches with its own key (`dev@10.0.0.3`), reached through it.
        let at = |ssh: Vec<String>| super::Remote { hop: std::env::var("IDEAE_LIVE_HOP").ok().filter(|h| !h.trim().is_empty()).map(|h| h.split_whitespace().map(String::from).collect()), ..super::Remote::new(ssh) };
        let started = Instant::now();
        let (remote, _) = super::ensure(at(ssh.clone())).expect("installed");
        // Through a first host: what a run killed there left is cleared, what this live one keeps stays.
        if remote.hop.is_some() {
            let (first, mine) = (remote.first(), super::super::remote::run_token());
            let plant = format!("touch ~/.ssh/ideae-1x1-9-0.sock ~/.ssh/ideae-{mine}-9-0.sock");
            super::super::remote::run(&first, &plant, None).unwrap();
            super::super::remote::clear_first_host(&remote);
            let left = super::super::remote::run(&first, &format!("cd ~/.ssh && ls ideae-*-9-0.sock 2>/dev/null; rm -f ideae-{mine}-9-0.sock"), None).unwrap();
            assert_eq!(left.trim(), format!("ideae-{mine}-9-0.sock"), "a killed run's socket is cleared, a live one's kept");
        }
        let first = started.elapsed();
        // Found the second time, not sent again.
        let started = Instant::now();
        let (again, _) = super::ensure(at(ssh)).expect("found");
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

        // A server of a task there: its port is told by the daemon, and reached here through ssh.
        let port = 18000 + (std::process::id() % 1000) as u16;
        let serve = crate::daemon::proto::Spec { cwd: "/".into(), env: Vec::new(), input: None, program: vec!["sh".into(), "-c".into(), format!("while :; do {{ echo hello; sleep 1; }} | nc -l -p {port}; done")] };
        let _served = daemon.pipe("live-task#serve".into(), serve);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (_, body) = daemon.call(&Call::Listening { task: "live-task".into() }, b"").unwrap();
            let found: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap_or_default();
            if found.iter().any(|l| l["port"] == port) {
                break;
            }
            assert!(Instant::now() < deadline, "its port is told: {found:?}");
            std::thread::sleep(Duration::from_millis(100));
        }
        let here = daemon.forward(port).expect("forwarded");
        let mut said = String::new();
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", here)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let _ = std::io::Read::read_to_string(&mut stream, &mut said);
        assert_eq!(said.trim(), "hello", "reached here at {here}");
        eprintln!("forwarded {port} there to {here} here");
        daemon.kill("live-task#serve");

        // A port of here (a task's Chrome) reached there at the same port: an agent there asks it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mine = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut client, _)) = listener.accept() {
                let _ = std::io::Write::write_all(&mut client, b"from here\n");
            }
        });
        daemon.wire().unwrap().reach_here(mine).expect("reached from there");
        let asked = Call::Exec { cwd: "/".into(), program: "sh".into(), args: vec!["-c".into(), format!("nc 127.0.0.1 {mine} </dev/null")], env: Vec::new() };
        let (_, out) = daemon.call(&asked, b"").unwrap();
        assert_eq!(String::from_utf8_lossy(&out).trim(), "from here", "port {mine} of here, asked there");
        // Let go of with its task: no longer there.
        daemon.wire().unwrap().release_here(mine);
        let (_, out) = daemon.call(&asked, b"").unwrap();
        assert!(String::from_utf8_lossy(&out).trim().is_empty(), "port {mine} let go of there too");
        eprintln!("port {mine} of here reached there");

        // A chat agent's host there, its own ideae-server's: it answers a window.
        let (exe, home) = daemon.there().expect("its ideae-server and home");
        let host = crate::acp::wire::HostSpec { command: "no-such-agent".into(), cwd: "/tmp".into(), journal: Some(home.join(".local/share/ideae-live/acp/x.jsonl")), ..Default::default() };
        let hosted = Spec { cwd: "/tmp".into(), env: Vec::new(), input: Some(String::from_utf8(crate::acp::wire::line(&host)).unwrap()), program: vec![exe.display().to_string(), "--acp-host".into()] };
        let told = daemon.pipe("live-task#acp".into(), hosted);
        daemon.write("live-task#acp", &crate::acp::wire::line(&crate::acp::wire::ToHost::Attach { version: crate::acp::wire::VERSION }));
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut heard = String::new();
        while !heard.contains("\"Hello\"") {
            assert!(Instant::now() < deadline, "the host there answered: {heard}");
            match told.try_recv() {
                Ok(PipeEvent::Bytes(b)) => heard.push_str(&String::from_utf8_lossy(&b)),
                _ => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        daemon.kill("live-task#acp");
        eprintln!("an agent's host ran there");

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
        // What was ended before the link was cut is not started again with it.
        std::thread::sleep(Duration::from_millis(500));
        let (_, body) = daemon.call(&Call::Listening { task: "live-task".into() }, b"").unwrap();
        assert_eq!(String::from_utf8_lossy(&body).trim(), "[]", "the ended server is not opened again");
        daemon.disconnect();

        let (daemon, _) = super::super::remote::connect(remote.clone(), |_| {}).expect("connected again");
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
        // ssh's link there goes with the test.
        super::super::remote::close_beyond(&remote);
        super::super::remote::close_master(&remote);
    }

    /// A host that wants a password: asked of the window once (the
    /// app's own askpass), not again while ssh's master connection holds;
    /// that connection ends when told to.
    /// `cargo build && IDEAE_LIVE_PASSWORD_REMOTE="ssh -p 2222 -o PubkeyAuthentication=no me@host" IDEAE_LIVE_PASSWORD=… cargo test -- --ignored a_real_server_asks`
    #[test]
    #[ignore]
    fn a_real_server_asks_for_a_password_once() {
        let (Ok(ssh), Ok(password)) = (std::env::var("IDEAE_LIVE_PASSWORD_REMOTE"), std::env::var("IDEAE_LIVE_PASSWORD")) else { return };
        let app = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/debug/ideae");
        assert!(app.is_file(), "cargo build first");
        // SAFETY: set before any ssh is started.
        unsafe { std::env::set_var("IDEAE_ASKPASS_PROGRAM", &app) };
        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = asked.clone();
        if crate::askpass::socket().is_none() {
            crate::askpass::listen(move |ask| {
                seen.lock().unwrap().push(ask.prompt.clone());
                let _ = ask.reply.send(Some(password.clone()));
            })
            .unwrap();
        }
        let remote = super::Remote::new(ssh.split_whitespace().map(String::from).collect());
        super::super::remote::close_beyond(&remote);
        super::super::remote::close_master(&remote);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(super::run(&remote, "echo one", None).unwrap(), "one");
        assert_eq!(super::run(&remote, "echo two", None).unwrap(), "two");
        let asked = asked.lock().unwrap().clone();
        assert_eq!(asked.len(), 1, "asked once: {asked:?}");
        assert!(asked[0].to_lowercase().contains("password"), "{asked:?}");
        super::super::remote::close_beyond(&remote);
        super::super::remote::close_master(&remote);
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
