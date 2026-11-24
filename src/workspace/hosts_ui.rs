//! Projects on other machines ([`crate::hosts`]): a task of theirs opens
//! once its host is connected, its ideae-server put there and its daemon
//! reached; the agents there report to the window as local ones do.

use super::*;

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
        let waiting = self.host_waiting.entry(host.clone()).or_default();
        let first = waiting.is_empty();
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

    /// A task of a project on another machine: its worktree made there
    /// (`<project>.worktrees/<branch>`, from the remote's default branch, or
    /// the project's base), then shown as a local one is.
    pub(super) fn create_remote_task(&mut self, project: usize, title: String, prompt: String, agents: Vec<String>, given: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.projects.get_mut(project) else { return };
        let Some((host, repo)) = crate::hosts::split(&p.path) else { return };
        let branch = naming::branch_name(&title);
        // Named as a local task's folder is (`feat/x` in `feat-x`), beside the repository there.
        let dir = PathBuf::from(format!("{}.worktrees", repo.display())).join(branch.replace('/', "-"));
        let path = crate::hosts::join(&host, &dir);
        if p.tasks.iter().any(|t| t.path == path) {
            window.push_notification(Notification::error(format!("A task of this project is in {} already", dir.display())).title("Could not create the task"), cx);
            return;
        }
        let base = p.base.clone();
        p.collapsed = false;
        p.tasks.push(TaskEntry {
            path: path.clone(),
            branch: branch.clone(),
            title,
            last_active: state::now(),
            busy: Some("creating…".into()),
            prompt: prompt.clone(),
            created: state::now(),
            agent: agents_ui::pick_profile(agents.first().map_or("", String::as_str)),
            choices: agents_ui::pick_choices(agents.first().map_or("", String::as_str)),
            fresh: true,
            ..Default::default()
        });
        cx.notify();
        let quote = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
        let base = if base.is_empty() {
            "\"$(git symbolic-ref -q --short refs/remotes/origin/HEAD || echo HEAD)\"".to_string()
        } else {
            quote(&base)
        };
        let line = format!(
            "cd {repo} && {{ git fetch -q origin 2>/dev/null || true; }} && git worktree add -q -b {branch} {dir} {base}",
            repo = quote(&repo.display().to_string()),
            branch = quote(&branch),
            dir = quote(&dir.display().to_string()),
        );
        cx.spawn_in(window, async move |this, cx| {
            let made = cx
                .background_spawn(async move {
                    #[cfg(unix)]
                    return crate::daemon::remote::run(&crate::daemon::remote::Remote::new(crate::hosts::ssh_for(&host)), &line, None).map(|_| ());
                    #[cfg(not(unix))]
                    {
                        let _ = line;
                        Err::<(), String>(format!("{host}: projects on other machines need ideae on Linux or macOS for now"))
                    }
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                match made {
                    Ok(()) => {
                        if let Some(entry) = this.entry_mut(&path) {
                            entry.busy = None;
                        }
                        this.switch(&path, window, cx);
                        if !prompt.is_empty() {
                            let send = state::settings().send_new_prompt || given || std::env::var_os("IDEAE_NEW_TASK").is_some();
                            // Its view comes once its host is connected: the prompt waits for it there.
                            match this.entry_mut(&path).and_then(|entry| entry.view.clone()) {
                                Some(view) => view.update(cx, |t, cx| t.queue_prompt(prompt, send, window, cx)),
                                None => {
                                    if let Some(entry) = this.entry_mut(&path) {
                                        entry.woke = Some(prompt);
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        // The one being made, not another there before it.
                        if let Some((pi, ti)) = this.find(&path).filter(|&(pi, ti)| this.projects[pi].tasks[ti].busy.is_some()) {
                            this.projects[pi].tasks.remove(ti);
                        }
                        window.push_notification(Notification::error(e).title("Could not create the task"), cx);
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
        let script = "git init -q -b main repo; cd repo; git config user.email t@t; git config user.name t; echo one > f.txt; git add f.txt; git commit -qm base";
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
        for program in [&ssh, &bins.join("claude")] {
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
        let worktree = PathBuf::from(&lines()[1]);
        assert_eq!(worktree.parent(), Some(repo.with_file_name("repo.worktrees").as_path()));
        let branch = String::from_utf8(std::process::Command::new("git").args(["branch", "--show-current"]).current_dir(&worktree).output().unwrap().stdout).unwrap();
        let task = crate::hosts::join("box", &worktree);
        workspace.update(cx, |w, _| {
            assert_eq!(w.active.as_deref(), Some(task.as_path()));
            assert_eq!(w.entry_mut(&task).unwrap().branch, branch.trim());
        });

        // Its changes, read there: a file changed and one new.
        std::fs::write(worktree.join("f.txt"), "two\n").unwrap();
        std::fs::write(worktree.join("g.txt"), "new\n").unwrap();
        let view = workspace.update(cx, |w, _| w.entry_mut(&task).unwrap().view.clone().unwrap());
        view.update(cx, |t, _| t.refresh_now());
        until(cx, "its changes were read there", &|cx| {
            let names = view.read_with(cx, |t, _| t.changed_names());
            names.contains(&"f.txt".to_string()) && names.contains(&"g.txt".to_string())
        });
        // A file of it opens from there, and is saved there.
        let file = task.join("f.txt");
        cx.update_window(handle.into(), |_, window, cx| view.update(cx, |t, cx| t.open_file(file.clone(), 0, window, cx))).unwrap();
        until(cx, "the file came from there", &|cx| view.read_with(cx, |t, cx| t.open_text(&file, cx)).as_deref() == Some("two\n"));
        cx.update_window(handle.into(), |_, window, cx| view.update(cx, |t, cx| t.type_and_save(&file, "zero\n", window, cx))).unwrap();
        until(cx, "the file was saved there", &|_| std::fs::read_to_string(worktree.join("f.txt")).unwrap() == "zero\ntwo\n");

        // Committed there, by the commit box.
        cx.update_window(handle.into(), |_, window, cx| view.update(cx, |t, cx| t.commit_for_test("Saved there", window, cx))).unwrap();
        let subject = || String::from_utf8(std::process::Command::new("git").args(["log", "-1", "--format=%s"]).current_dir(&worktree).output().unwrap().stdout).unwrap();
        until(cx, "committed there", &|_| subject().trim() == "Saved there");
        workspace.update(cx, |w, cx| {
            let commands = crate::commands::InHand::of(w.entry_mut(&task).unwrap(), cx);
            assert!(commands.remote && !commands.git(crate::task::GitCommand::PullRequest), "gh is here, not there");
        });

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

        drop(_end);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
