//! Projects on other machines ([`crate::hosts`]): a task of theirs opens
//! once its host is connected, its ideae-server put there and its daemon
//! reached; the agents there report to the window as local ones do.

use super::*;

/// Something to do once a host is connected.
pub(super) type Then = Box<dyn FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>)>;

impl Workspace {
    /// A project on another machine, from the command line:
    /// `ssh://me@box/home/me/proj`.
    pub fn add_remote_project(&mut self, url: &str) -> Option<usize> {
        let (host, there) = crate::hosts::parse_url(url)?;
        Some(self.add_remote_root(&host, &there))
    }

    fn add_remote_root(&mut self, host: &str, there: &Path) -> usize {
        let key = crate::hosts::join(host, there);
        let ix = self.add_project_root(key.clone());
        let p = &mut self.projects[ix];
        // Its worktrees are not listed here yet: its main thread is there from the start.
        if !p.tasks.iter().any(|t| t.main) {
            p.tasks.insert(0, TaskEntry { path: key, main: true, ..Default::default() });
        }
        ix
    }

    /// Adds a project of another machine: asks there that the folder is
    /// in a git repository, and keys it by the repository's own folder as
    /// git names it there (links followed), the one its worktrees list.
    pub fn add_remote_project_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("me@box:/home/me/project"));
        let apply = {
            let input = input.clone();
            move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
                let text = input.read(cx).value().to_string();
                let Some((host, path)) = crate::hosts::parse_target(&text) else {
                    window.push_notification(Notification::error("As user@host:/path/to/repository, or ssh://user@host:port/path").title("Add project"), cx);
                    return;
                };
                window.close_dialog(cx);
                this.check_remote_project(host, path, window, cx);
            }
        };
        let this = cx.weak_entity();
        let focused = std::rc::Rc::new(std::cell::Cell::new(false));
        window.open_dialog(cx, move |dialog, _, _| {
            let (input, focused, this, apply) = (input.clone(), focused.clone(), this.clone(), apply.clone());
            dialog
                .title("Add a project on another machine")
                .w(px(520.))
                .on_ok({
                    let (this, apply) = (this.clone(), apply.clone());
                    move |_, window, cx| {
                        let _ = this.update(cx, |this, cx| apply(this, window, cx));
                        false
                    }
                })
                .content(move |content, window, cx| {
                    if !focused.replace(true) {
                        let input = input.clone();
                        window.defer(cx, move |window, cx| input.read(cx).focus_handle(cx).focus(window, cx));
                    }
                    content.child(
                        v_flex().gap_1().child(Input::new(&input)).child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("A git repository there, reached by ssh as your terminal reaches it (keys, ~/.ssh/config). Its agents and terminals run there, and go on while this window is closed."),
                        ),
                    )
                })
                .footer(h_flex().justify_end().child(Button::new("add-remote-project").primary().label("Add").on_click(move |_, window, cx| {
                    let _ = this.update(cx, |this, cx| apply(this, window, cx));
                })))
        });
    }

    /// `path` on `host`: added once ssh finds a git repository there.
    pub(crate) fn check_remote_project(&mut self, host: String, path: String, window: &mut Window, cx: &mut Context<Self>) {
        // `~/x` is the shell's to read there; the rest as it is.
        let quote = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
        let place = match path.strip_prefix("~/") {
            Some(rest) => format!("~/{}", quote(rest)),
            None => quote(&path),
        };
        let line = format!("cd {place} && git rev-parse --show-toplevel");
        cx.spawn_in(window, async move |this, cx| {
            let to = host.clone();
            let found = cx
                .background_spawn(async move {
                    #[cfg(unix)]
                    return crate::daemon::remote::run(&crate::daemon::remote::Remote::new(crate::hosts::ssh_for(&to)), &line, None);
                    #[cfg(not(unix))]
                    {
                        let _ = (line, to);
                        Err::<String, String>("Projects on other machines need ideae on Linux or macOS for now".into())
                    }
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                match found.map(|top| top.lines().last().unwrap_or_default().trim().to_string()) {
                    Ok(top) if top.starts_with('/') => {
                        let ix = this.add_remote_root(&host, Path::new(&top));
                        this.open_main(ix, window, cx);
                    }
                    Ok(_) => window.push_notification(Notification::error(format!("{path}: not a git repository there")).title(format!("{host}: not added")), cx),
                    Err(e) => window.push_notification(Notification::error(e).title(format!("{host}: not added")), cx),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The host of a remote project, and how its link is: connecting,
    /// down (its agents go on there), or nothing to say.
    pub(crate) fn host_state(&self, path: &Path) -> Option<(String, Option<&'static str>)> {
        let (host, _) = crate::hosts::split(path)?;
        if self.host_waiting.contains_key(&host) {
            return Some((host, Some("connecting…")));
        }
        #[cfg(unix)]
        if crate::daemon::client::for_path(path).is_some() && !crate::hostfs::reachable(path) {
            return Some((host, Some("no link")));
        }
        Some((host, None))
    }

    /// Does `then` now if `path`'s host is connected (or `path` is here),
    /// else once it is: it is being connected.
    pub(super) fn with_host(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>, then: Then) {
        let Some((host, _)) = crate::hosts::split(path) else { return then(self, window, cx) };
        #[cfg(unix)]
        if crate::daemon::client::for_path(path).is_some() {
            return then(self, window, cx);
        }
        self.host_then.entry(host.clone()).or_default().push(then);
        if !self.host_waiting.contains_key(&host) {
            self.host_waiting.insert(host.clone(), Vec::new());
            self.connect_host(host, window, cx);
        }
        cx.notify();
    }

    /// What ssh asks, asked here: a password or a passphrase hidden as it
    /// is typed, a host's key to trust as it is. Put away, it is answered no.
    #[cfg(unix)]
    pub(super) fn ask_for_ssh(&mut self, ask: crate::askpass::Ask, window: &mut Window, cx: &mut Context<Self>) {
        let secret = ask.is_secret();
        let input = cx.new(|cx| InputState::new(window, cx).masked(secret));
        let reply = std::rc::Rc::new(std::cell::RefCell::new(Some(ask.reply)));
        let prompt: SharedString = ask.prompt.trim().to_string().into();
        let answer = {
            let (input, reply) = (input.clone(), reply.clone());
            move |given: bool, cx: &mut App| {
                if let Some(reply) = reply.borrow_mut().take() {
                    let _ = reply.send(given.then(|| input.read(cx).value().to_string()));
                }
            }
        };
        let focused = std::rc::Rc::new(std::cell::Cell::new(false));
        window.open_dialog(cx, move |dialog, _, _| {
            let (input, focused, prompt) = (input.clone(), focused.clone(), prompt.clone());
            let (ok, cancel, close) = (answer.clone(), answer.clone(), answer.clone());
            dialog
                .title("ssh asks")
                .w(px(480.))
                .on_ok(move |_, _, cx| {
                    ok(true, cx);
                    true
                })
                .on_cancel(move |_, _, cx| {
                    cancel(false, cx);
                    true
                })
                .on_close(move |_, _, cx| close(false, cx))
                .content(move |content, window, cx| {
                    if !focused.replace(true) {
                        let input = input.clone();
                        window.defer(cx, move |window, cx| input.read(cx).focus_handle(cx).focus(window, cx));
                    }
                    content.child(v_flex().gap_2().child(div().text_sm().child(prompt.clone())).child(Input::new(&input)).child(
                        div().text_xs().text_color(cx.theme().muted_foreground).child("Kept nowhere: asked again only when the link to it is made anew."),
                    ))
                })
        });
    }

    /// Whether `path` is a task of a host not connected yet. It is then
    /// being connected, and the task loads once it is; `show`: and comes
    /// into view, unless another was shown meanwhile.
    pub(super) fn waits_for_host(&mut self, path: &Path, show: bool, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some((host, _)) = crate::hosts::split(path) else { return false };
        #[cfg(unix)]
        if crate::daemon::client::for_path(path).is_some() {
            return false;
        }
        if show {
            self.host_shown = Some(path.to_path_buf());
        }
        if let Some(entry) = self.entry_mut(path) {
            entry.busy.get_or_insert_with(|| "connecting…".into());
        }
        // Being connected already (for a task, or for what waits on it).
        let first = !self.host_waiting.contains_key(&host);
        let waiting = self.host_waiting.entry(host.clone()).or_default();
        if !waiting.iter().any(|p| p == path) {
            waiting.push(path.to_path_buf());
        }
        if first {
            self.connect_host(host, window, cx);
        }
        cx.notify();
        true
    }

    #[cfg(not(unix))]
    fn connect_host(&mut self, host: String, window: &mut Window, cx: &mut Context<Self>) {
        self.host_then.remove(&host);
        let waiting = self.host_waiting.remove(&host).unwrap_or_default();
        for path in waiting {
            if let Some(entry) = self.entry_mut(&path) {
                entry.busy = None;
            }
        }
        window.push_notification(Notification::error("Projects on other machines need ideae on Linux or macOS for now.").title(format!("{host} not reached")), cx);
    }

    #[cfg(unix)]
    fn connect_host(&mut self, host: String, window: &mut Window, cx: &mut Context<Self>) {
        let hook_tx = self.hook_tx.clone();
        cx.spawn_in(window, async move |this, cx| {
            let to = host.clone();
            let live = hook_tx.clone();
            let connected = cx
                .background_spawn(async move {
                    let (remote, hooks) = crate::daemon::install::ensure(crate::daemon::remote::Remote::new(crate::hosts::ssh_for(&to)))?;
                    crate::agents::set_host_hooks(&to, hooks);
                    let host = to.clone();
                    crate::daemon::remote::connect(remote, move |hook| {
                        let _ = live.send_blocking(here(&host, hook.event()));
                    })
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                let waiting = this.host_waiting.remove(&host).unwrap_or_default();
                let then = this.host_then.remove(&host).unwrap_or_default();
                for path in &waiting {
                    if let Some(entry) = this.entry_mut(path) {
                        entry.busy = None;
                    }
                }
                match connected {
                    Ok((daemon, earlier)) => {
                        crate::daemon::client::add_remote(host.clone(), daemon);
                        // What its agents reported while no window listened.
                        for hook in earlier {
                            let at = hook.at;
                            let _ = hook_tx.send_blocking(agent::HookEvent { replayed: Some(at), ..here(&host, hook.event()) });
                        }
                        // Its worktrees, as made there by anyone.
                        this.sync_worktrees(window, cx);
                        let shown = this.host_shown.take();
                        for then in then {
                            then(this, window, cx);
                        }
                        for path in waiting {
                            if shown.as_deref() == Some(path.as_path()) {
                                this.switch(&path, window, cx);
                            } else {
                                this.load(&path, window, cx);
                            }
                        }
                    }
                    Err(e) => {
                        window.push_notification(Notification::error(e).title(format!("{host} not reached")), cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

}

/// A report from `host`: the paths it names (a worktree an agent makes,
/// a file it edits) are paths there, keyed as the window keys them.
#[cfg(unix)]
fn here(host: &str, mut event: agent::HookEvent) -> agent::HookEvent {
    let key = |path: &str| crate::hosts::join(host, Path::new(path)).display().to_string();
    event.worktree = event.worktree.as_deref().map(key);
    event.file = event.file.as_deref().filter(|f| f.starts_with('/')).map(key).or(event.file);
    event
}

#[cfg(all(test, unix))]
mod tests {
    use crate::workspace::Workspace;
    use gpui_kit::{AppContext as _, Bounds, Point, TestAppContext, WindowBounds, WindowOptions, px, size};
    use std::path::PathBuf;
    use std::time::Duration;

    /// A language server, as the stand-in `gopls` of the test below runs it
    /// there: it answers hover with the file as it was named to it, tells a
    /// problem of each file opened by the name it knows, and its pid.
    #[test]
    #[ignore]
    fn stand_in_language_server() {
        use std::io::{BufRead as _, Read as _, Write as _};
        use std::os::fd::FromRawFd as _;
        if std::env::var_os("IDEAE_TEST_LSP").is_none() {
            return;
        }
        // SAFETY: descriptors 0 and 1 are this process's, used by nothing else here.
        let (input, mut output) = unsafe { (std::fs::File::from_raw_fd(0), std::fs::File::from_raw_fd(1)) };
        if let Ok(file) = std::env::var("IDEAE_TEST_LSP_PID") {
            let _ = std::fs::write(file, std::process::id().to_string());
        }
        let mut input = std::io::BufReader::new(input);
        let mut send = |message: serde_json::Value| {
            let body = message.to_string();
            let _ = write!(output, "Content-Length: {}\r\n\r\n{body}", body.len());
            let _ = output.flush();
        };
        loop {
            let mut length = 0;
            loop {
                let mut line = String::new();
                if input.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                let line = line.trim_end();
                if line.is_empty() {
                    break;
                }
                if let Some(n) = line.strip_prefix("Content-Length:") {
                    length = n.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; length];
            if input.read_exact(&mut body).is_err() {
                return;
            }
            let message: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let id = message["id"].clone();
            match message["method"].as_str().unwrap_or_default() {
                "initialize" => send(serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"capabilities": {"hoverProvider": true, "textDocumentSync": 1}, "processId": message["params"]["processId"]}})),
                "textDocument/didOpen" => {
                    let uri = message["params"]["textDocument"]["uri"].clone();
                    send(serde_json::json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": uri, "diagnostics": [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "message": "seen there"}]}}));
                }
                "textDocument/hover" => {
                    let named = format!("named {}", message["params"]["textDocument"]["uri"].as_str().unwrap_or_default());
                    send(serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"contents": {"kind": "plaintext", "value": named}}}));
                }
                "shutdown" => send(serde_json::json!({"jsonrpc": "2.0", "id": id, "result": null})),
                "exit" => return,
                _ if !id.is_null() => send(serde_json::json!({"jsonrpc": "2.0", "id": id, "result": null})),
                _ => {}
            }
        }
    }

    /// A project on "another machine": this one, behind a stand-in ssh with
    /// a home and a state of its own, its ideae-server put there from
    /// target/debug. Its main thread's agent runs there, in the folder
    /// there; a new task's worktree is made there and its agent runs in it;
    /// Pause ends that agent there.
    ///
    /// `cargo build && cargo test -- --ignored a_remote_project` after changing remote projects.
    #[gpui_kit::test]
    #[ignore]
    fn a_remote_project_runs_its_agents_on_its_host(cx: &mut TestAppContext) {
        let _one = crate::workspace::variants::tests::ONE_WINDOW.lock().unwrap_or_else(|e| e.into_inner());
        // A failure leaves nothing running "there": its daemon ends its agents.
        struct EndHosts;
        impl Drop for EndHosts {
            fn drop(&mut self) {
                for daemon in crate::daemon::client::remotes() {
                    daemon.shutdown();
                }
            }
        }
        let _end = EndHosts;
        cx.executor().allow_parking();
        let dir = std::env::temp_dir().join(format!("ideae-remote-project-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (home, bins) = (dir.join("box-home"), dir.join("bins"));
        for d in [&home, &bins] {
            std::fs::create_dir_all(d).unwrap();
        }
        // Its .ideae.toml sets its tasks up there, copies an ignored file into them and tears them down.
        let torn = dir.join("torn");
        let toml = format!("[task]\nsetup = [\"echo made > .made\"]\nteardown = [\"pwd > '{}'\"]\ncopy = [\".env.local\"]\n", torn.display());
        std::fs::create_dir_all(dir.join("repo")).unwrap();
        std::fs::write(dir.join("repo/.ideae.toml"), &toml).unwrap();
        std::fs::write(dir.join("repo/.gitignore"), ".made\n.env.local\n").unwrap();
        std::fs::write(dir.join("repo/.env.local"), "SECRET=1\n").unwrap();
        let script = "git init -q -b main repo; cd repo; git config user.email t@t; git config user.name t; echo one > f.txt; git add f.txt .ideae.toml .gitignore; git commit -qm base";
        assert!(std::process::Command::new("sh").arg("-ec").arg(script).current_dir(&dir).status().unwrap().success());
        let repo = dir.join("repo").canonicalize().unwrap();

        let server = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/debug/ideae-server");
        assert!(server.is_file(), "cargo build first: {}", server.display());
        let uname = String::from_utf8(std::process::Command::new("uname").arg("-sm").output().unwrap().stdout).unwrap();
        let name = match uname.split_whitespace().collect::<Vec<_>>()[..] {
            ["Darwin", "arm64"] => "aarch64-macos",
            ["Darwin", _] => "x86_64-macos",
            [_, "aarch64"] => "aarch64-linux",
            _ => "x86_64-linux",
        };
        std::fs::copy(&server, bins.join(format!("ideae-server-{name}"))).unwrap();
        // Claude Code as far as its hooks go: it runs the commands of its
        // --settings file there, with its session id and its last words.
        let claude = format!(
            r#"#!/bin/sh
while [ $# -gt 0 ]; do case "$1" in --settings) s="$2"; shift;; --session-id) id="$2"; shift;; esac; shift; done
exe=$(sed -n 's/.*"command": "\(.*\) --hook SessionStart".*/\1/p' "$s" | head -1)
pwd >> '{pwds}'
echo "{{\"session_id\":\"$id\"}}" | eval "$exe --hook SessionStart"
echo "{{\"session_id\":\"$id\",\"last_assistant_message\":\"All done here\"}}" | eval "$exe --hook Stop"
echo $$ >> '{pwds}.pids'
exec cat
"#,
            pwds = dir.join("pwds").display()
        );
        std::fs::write(bins.join("claude"), claude).unwrap();
        // gopls as far as the test goes: the test binary run again (none is here, to be found first).
        let gopls_pid = dir.join("gopls.pid");
        std::fs::write(
            bins.join("gopls"),
            format!(
                "#!/bin/sh\nexec env IDEAE_TEST_LSP=1 IDEAE_TEST_LSP_PID='{}' '{}' --exact workspace::hosts_ui::tests::stand_in_language_server --ignored --nocapture --test-threads=1\n",
                gopls_pid.display(),
                std::env::current_exe().unwrap().display()
            ),
        )
        .unwrap();
        let ssh = dir.join("ssh");
        std::fs::write(
            &ssh,
            format!(
                "#!/bin/sh\nwhile [ \"$1\" != \"--\" ]; do shift; done; shift\nexport HOME='{}' IDEAE_STATE='{}' PATH='{}':\"$PATH\"\nunset XDG_RUNTIME_DIR\nexec sh -c \"$*\"\n",
                home.display(),
                dir.join("box-state.json").display(),
                bins.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        for program in [&ssh, &bins.join("claude"), &bins.join("gopls")] {
            std::fs::set_permissions(program, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let pwds = dir.join("pwds");
        // By its whole path: the shell there is a login shell, whose PATH may
        // find the user's own claude first.
        let agent = bins.join("claude").display().to_string();
        // SAFETY: set before anything of the app reads them.
        unsafe {
            std::env::set_var("IDEAE_STATE", dir.join("state.json"));
            std::env::set_var("IDEAE_NO_DAEMON", "1");
            std::env::set_var("IDEAE_SSH", ssh.display().to_string());
            std::env::set_var("IDEAE_SERVER_DIR", bins.display().to_string());
        }
        crate::state::set_settings(crate::state::Settings { notifications: false, pr_status: false, ..Default::default() });
        cx.update(|cx| {
            gpui_kit::init(cx);
            gpui_cn::init(cx);
            crate::theme::init(cx);
        });
        // A daemon of an earlier build runs there already (daemons outlive windows): a copy,
        // another build by its file's time. It must hand over to this build's, or calls go unanswered.
        let older = bins.join("ideae-server-older");
        std::fs::copy(&server, &older).unwrap();
        let an_hour_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options().write(true).open(&older).unwrap().set_modified(an_hour_ago).unwrap();
        let mut old_daemon = std::process::Command::new(&older)
            .arg("--daemon")
            .env("HOME", &home)
            .env("IDEAE_STATE", dir.join("box-state.json"))
            .env("IDEAE_APP", format!("{}-remote", crate::state::APP_ID))
            .env("PATH", format!("{}:{}", bins.display(), std::env::var("PATH").unwrap_or_default()))
            .env_remove("XDG_RUNTIME_DIR")
            .spawn()
            .unwrap();
        let url = format!("ssh://box{}", repo.display());
        let (handle, workspace) = cx.update(|cx| {
            let bounds = Bounds { origin: Point::default(), size: size(px(1300.), px(800.)) };
            let options = WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() };
            gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| Workspace::new(vec![PathBuf::from(&url)], Some(Some(agent.clone())), None, window, cx))).expect("a test window")
        });
        let main = crate::hosts::join("box", &repo);
        // Its commands allowed, as the trust dialog would.
        let key = crate::project_config::trust_key(&crate::project_config::parse(&toml).unwrap().task).unwrap();
        workspace.update(cx, |w, _| {
            w.projects[0].trusted.insert(key, true);
        });
        let lines = || std::fs::read_to_string(&pwds).unwrap_or_default().lines().map(str::to_string).collect::<Vec<_>>();
        let until = |cx: &mut TestAppContext, what: &str, done: &dyn Fn(&mut TestAppContext) -> bool| {
            for _ in 0..600 {
                cx.run_until_parked();
                // The refresh's pause for more events is on the test's clock.
                cx.executor().advance_clock(Duration::from_millis(250));
                cx.run_until_parked();
                if done(cx) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            panic!("{what}: ran {:?}", lines());
        };
        workspace.update(cx, |w, _| {
            assert_eq!(w.projects[0].path, main);
            assert_eq!(w.projects[0].name, "repo");
        });
        until(cx, "the main thread's agent ran there", &|_| !lines().is_empty());
        assert!(old_daemon.try_wait().unwrap().is_some(), "the earlier build's daemon handed over and went");
        assert_eq!(lines(), [repo.display().to_string()], "in the folder there");
        workspace.update(cx, |w, cx| {
            assert_eq!(w.active.as_deref(), Some(main.as_path()));
            assert!(w.entry_mut(&main).unwrap().view.as_ref().unwrap().read(cx).has_agent());
        });
        // Its hooks ran there and reached the window: the turn is done, its words are kept.
        let done = |cx: &mut TestAppContext| {
            workspace.update(cx, |w, cx| {
                let view = w.entry_mut(&main).unwrap().view.clone().unwrap();
                let view = view.read(cx);
                let agent = view.agent_of(crate::task::Who::Main).unwrap();
                agent.status == crate::agent::AgentStatus::Done && agent.said.as_ref().is_some_and(|s| s.whole.contains("All done here"))
            })
        };
        until(cx, "the agent's hooks reached the window", &|cx| done(cx));
        // The sidebar draws it, its host beside its name.
        use gpui_kit::test::TestWindowExt as _;
        cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx)).unwrap();
        cx.update_window(handle.into(), |_, window, cx| workspace.update(cx, |w, cx| w.create_task(0, "Fix it".into(), String::new(), Vec::new(), Vec::new(), window, cx))).unwrap();
        until(cx, "the task's agent ran in its worktree there", &|_| lines().len() == 2);
        let made_in = PathBuf::from(&lines()[1]);
        until(cx, "its setup ran there", &|_| made_in.join(".made").is_file());
        assert_eq!(std::fs::read_to_string(made_in.join(".env.local")).unwrap(), "SECRET=1\n", "[task] copy, there");
        // Its check's commands run there to their end: their code and output; a timeout ends them.
        let in_task = crate::hosts::join("box", &made_in);
        let never = std::sync::atomic::AtomicBool::new(false);
        let ran = crate::project_config::run(&["echo hi there; exit 3".into()], &in_task, &[], Duration::from_secs(20), &never);
        assert_eq!((ran.code, ran.output.trim()), (Some(3), "hi there"));
        let pids = dir.join("slow.pid");
        let ran = crate::project_config::run(&[format!("echo $$ > '{}'; sleep 30", pids.display())], &in_task, &[], Duration::from_millis(800), &never);
        assert!(ran.timed_out && ran.code.is_none());
        let pid = std::fs::read_to_string(&pids).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::process::Command::new("kill").args(["-0", pid.trim()]).status().unwrap().success() {
            assert!(std::time::Instant::now() < deadline, "what ran out of time there ended");
            std::thread::sleep(Duration::from_millis(20));
        }
        let worktree = PathBuf::from(&lines()[1]);
        assert_eq!(worktree.parent(), Some(repo.with_file_name("repo.worktrees").as_path()));
        let branch = String::from_utf8(std::process::Command::new("git").args(["branch", "--show-current"]).current_dir(&worktree).output().unwrap().stdout).unwrap();
        let task = crate::hosts::join("box", &worktree);
        workspace.update(cx, |w, _| {
            assert_eq!(w.active.as_deref(), Some(task.as_path()));
            assert_eq!(w.entry_mut(&task).unwrap().branch, branch.trim());
        });

        // Its first refresh done, which tells the daemon what to watch there.
        let view = workspace.update(cx, |w, _| w.entry_mut(&task).unwrap().view.clone().unwrap());
        until(cx, "the task's first refresh", &|cx| view.read_with(cx, |t, _| t.refreshed_once()));
        // Its changes, read there: a file changed and one new.
        std::fs::write(worktree.join("f.txt"), "two\n").unwrap();
        std::fs::write(worktree.join("g.txt"), "new\n").unwrap();
        // Told by its daemon's watcher there, well before the poll of a minute.
        let seen = |cx: &mut TestAppContext| {
            let names = view.read_with(cx, |t, _| t.changed_names());
            names.contains(&"f.txt".to_string()) && names.contains(&"g.txt".to_string())
        };
        let mut waited = Duration::ZERO;
        while !seen(cx) {
            assert!(waited < Duration::from_secs(20), "its changes were told by the watcher there");
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(250));
            waited += Duration::from_millis(250);
            std::thread::sleep(Duration::from_millis(20));
        }
        // A file of it opens from there, and is saved there.
        let file = task.join("f.txt");
        cx.update_window(handle.into(), |_, window, cx| view.update(cx, |t, cx| t.open_file(file.clone(), 0, window, cx))).unwrap();
        until(cx, "the file came from there", &|cx| view.read_with(cx, |t, cx| t.open_text(&file, cx)).as_deref() == Some("two\n"));
        cx.update_window(handle.into(), |_, window, cx| view.update(cx, |t, cx| t.type_and_save(&file, "zero\n", window, cx))).unwrap();
        until(cx, "the file was saved there", &|_| std::fs::read_to_string(worktree.join("f.txt")).unwrap() == "zero\ntwo\n");

        // A server it runs there: its port is told by its daemon, by the task's sessions.
        let port = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
        let daemon = crate::daemon::client::for_path(&task).unwrap();
        let serve = crate::daemon::proto::Spec { cwd: worktree.clone(), env: Vec::new(), input: None, program: vec!["python3".into(), "-m".into(), "http.server".into(), "--bind".into(), "127.0.0.1".into(), port.to_string()] };
        let _served = daemon.pipe(format!("{}#serve", task.display()), serve);
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !crate::hostfs::listening(&task, None).iter().any(|l| l.port == port) {
            assert!(std::time::Instant::now() < deadline, "its server's port is told");
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(crate::hostfs::listening(&main, None).iter().all(|l| l.port != port), "another task's sessions are not its");
        daemon.kill(&format!("{}#serve", task.display()));

        // Its language server runs there: found by the login shell there (the stand-in, and no
        // other), told and telling files by their names there, which here are its keys.
        let go = task.join("main.go");
        std::fs::write(worktree.join("main.go"), "package main\n").unwrap();
        assert_eq!(crate::lsp_remote::locate(&go, "gopls"), Some(bins.join("gopls")), "the stand-in, not a gopls of this machine");
        let servers = crate::lsp_share::TaskServers::default();
        let found = servers.server_for(&task, &go).expect("its server there");
        found.client.did_open(&go, "go", "package main\n").unwrap();
        let hover = found.client.hover(&crate::lsp::path_to_uri(&go), lsp_types::Position { line: 0, character: 0 }).unwrap().expect("hover");
        let named = serde_json::to_value(&hover.contents).unwrap()["value"].as_str().unwrap_or_default().to_string();
        assert_eq!(named, format!("named {}", crate::lsp::path_to_uri(&worktree.join("main.go"))), "named as it is there");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while found.client.problems(&crate::lsp::path_to_uri(&go)).1.is_empty() {
            assert!(std::time::Instant::now() < deadline, "its problems are told of the file here");
            std::thread::sleep(Duration::from_millis(20));
        }
        let pid = std::fs::read_to_string(&gopls_pid).unwrap();
        drop(found);
        servers.clear();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::process::Command::new("kill").args(["-0", pid.trim()]).status().unwrap().success() {
            assert!(std::time::Instant::now() < deadline, "its server there ended with it");
            std::thread::sleep(Duration::from_millis(20));
        }

        // Committed there, by the commit box.
        cx.update_window(handle.into(), |_, window, cx| view.update(cx, |t, cx| t.commit_for_test("Saved there", window, cx))).unwrap();
        let subject = || String::from_utf8(std::process::Command::new("git").args(["log", "-1", "--format=%s"]).current_dir(&worktree).output().unwrap().stdout).unwrap();
        until(cx, "committed there", &|_| subject().trim() == "Saved there");
        // gh, here with the user's sign-in, is told the repository its origin there names.
        let origin = std::process::Command::new("git").args(["remote", "add", "origin", "git@github.com:owner/repo.git"]).current_dir(&repo).status().unwrap();
        assert!(origin.success());
        let gh = crate::integrations::github::gh_for(&task).expect("a GitHub repository");
        let named = gh.get_envs().find(|(k, _)| *k == "GH_REPO").and_then(|(_, v)| v).map(|v| v.to_string_lossy().into_owned());
        assert_eq!(named.as_deref(), Some("github.com/owner/repo"));
        workspace.update(cx, |w, cx| assert!(crate::commands::InHand::of(w.entry_mut(&task).unwrap(), cx).remote));

        // Two tries at one task: their worktrees made there, from one commit, each with its agent.
        cx.update_window(handle.into(), |_, window, cx| {
            workspace.update(cx, |w, cx| w.create_task(0, "Try both".into(), String::new(), vec!["one way".into(), "another".into()], Vec::new(), window, cx))
        })
        .unwrap();
        let variants = |cx: &mut TestAppContext| workspace.update(cx, |w, _| w.projects[0].tasks.iter().filter(|t| !t.group.is_empty() && t.busy.is_none() && t.view.is_some()).map(|t| t.path.clone()).collect::<Vec<_>>());
        until(cx, "both variants made there and open", &|cx| variants(cx).len() == 2);
        for variant in variants(cx) {
            assert!(crate::hosts::is_remote(&variant) && crate::hosts::there(&variant).is_dir(), "{} there", variant.display());
        }
        until(cx, "their agents ran there", &|_| lines().len() == 4);

        // A worktree made there by hand is a task here.
        let by_hand = repo.with_file_name("repo.worktrees").join("by-hand");
        let made = std::process::Command::new("git").args(["worktree", "add", "-q", "-b", "by-hand"]).arg(&by_hand).current_dir(&repo).status().unwrap();
        assert!(made.success());
        let by_hand = crate::hosts::join("box", &by_hand);
        cx.update_window(handle.into(), |_, window, cx| workspace.update(cx, |w, cx| w.sync_worktrees(window, cx))).unwrap();
        until(cx, "the worktree made there is listed", &|cx| workspace.update(cx, |w, _| w.entry_mut(&by_hand).is_some_and(|t| t.branch == "by-hand")));
        workspace.update(cx, |w, _| {
            let main = w.entry_mut(&main).unwrap();
            assert_eq!(main.branch, "main", "the main thread's branch, as read there");
        });

        // Added again by hand, through a link to it: the same project, keyed as git names it there.
        std::os::unix::fs::symlink(&repo, dir.join("link")).unwrap();
        std::fs::create_dir_all(dir.join("plain")).unwrap();
        cx.update_window(handle.into(), |_, window, cx| {
            workspace.update(cx, |w, cx| {
                w.switch(&task, window, cx);
                assert_eq!(w.active.as_deref(), Some(task.as_path()));
                w.check_remote_project("box".into(), dir.join("link").display().to_string(), window, cx);
                w.check_remote_project("box".into(), dir.join("plain").display().to_string(), window, cx);
            })
        })
        .unwrap();
        // Found there: its main thread comes into view.
        until(cx, "the link was found to be the project", &|cx| workspace.update(cx, |w, _| w.active.as_deref() == Some(main.as_path())));
        workspace.update(cx, |w, _| {
            assert_eq!(w.projects.len(), 1, "no second project for the link, none for a folder without git");
            assert_eq!(w.host_state(&main), Some(("box".to_string(), None)), "connected: nothing to say but its host");
        });

        // Paused: its agent there ends.
        let pid = std::fs::read_to_string(format!("{}.pids", pwds.display())).unwrap().lines().nth(1).unwrap().trim().to_string();
        cx.update_window(handle.into(), |_, window, cx| workspace.update(cx, |w, cx| w.close_task(&task, window, cx))).unwrap();
        let gone = || !std::process::Command::new("kill").args(["-0", &pid]).status().unwrap().success();
        until(cx, "the paused task's agent ended", &|_| gone());

        // Archived: packed on its host with what is nowhere else (a commit there, a change not
        // committed), a stub here; its worktree goes there.
        std::fs::write(worktree.join("h.txt"), "kept\n").unwrap();
        cx.update_window(handle.into(), |_, window, cx| workspace.update(cx, |w, cx| w.archive_now(&task, None, window, cx))).unwrap();
        until(cx, "archived", &|cx| workspace.update(cx, |w, _| w.entry_mut(&task).is_none()));
        assert_eq!(std::fs::read_to_string(&torn).unwrap().trim(), worktree.display().to_string(), "torn down there, in it");
        assert!(!worktree.exists(), "its worktree is gone there");
        let root = workspace.update(cx, |w, _| w.archive_dir.clone());
        let listed = crate::archive::list(&root);
        let item = listed.iter().find(|a| a.meta.worktree == task).expect("listed here");
        assert!(crate::archive::is_stub(&item.file) && item.size > 0);
        assert_eq!(item.meta.uncommitted, ["h.txt"]);
        assert!(item.meta.unpushed.iter().any(|c| c.subject == "Saved there"));
        let kept = std::fs::read_dir(home.join(".local/share").join(crate::state::APP_ID).join("archive")).unwrap().flatten().flat_map(|d| std::fs::read_dir(d.path()).unwrap().flatten()).filter(|f| f.file_name().to_string_lossy().ends_with(".tar.gz")).count();
        assert_eq!(kept, 1, "the archive is there, not here");
        assert!(std::fs::read_dir(&root).unwrap().flatten().flat_map(|d| std::fs::read_dir(d.path()).unwrap().flatten()).all(|f| !f.file_name().to_string_lossy().ends_with(".tar.gz")));

        // Restored there, with its commit and its change.
        let plan = crate::archive::Plan { start: crate::archive::Start::Origin, branch: "fix-again".into(), commits: true, changes: true, profiles: Vec::new(), copy_env: false };
        let restored = crate::archive::restore(&main, &item.file, &item.meta, &plan).expect("restored there");
        let back = crate::hosts::there(&restored.path);
        assert_eq!((restored.commits.clone(), restored.changes.clone()), (crate::archive::Applied::Done, crate::archive::Applied::Done));
        assert_eq!(std::fs::read_to_string(back.join("h.txt")).unwrap(), "kept\n");
        let log = String::from_utf8(std::process::Command::new("git").args(["log", "-1", "--format=%s"]).current_dir(&back).output().unwrap().stdout).unwrap();
        assert_eq!(log.trim(), "Saved there");
        crate::archive::delete(&item.file).unwrap();
        assert!(!item.file.exists());

        drop(_end);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
