//! The layered settings of projects (crate::layers) in the window: worked
//! out again when a layer changes, the team's read from each main
//! checkout, and what a move to another space changes told.

use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::component::{WindowExt as _, notification::Notification};
use gpui_kit::*;

use super::Workspace;
use crate::layers::{self, Layer, Overrides, Values};

impl Workspace {
    /// Works out again what applies to each project, after any of its
    /// layers changed, and passes on what changed to what follows it.
    pub fn resolve_projects(&mut self, cx: &mut Context<Self>) {
        let global = crate::state::with_settings(layers::global_layer);
        for pi in 0..self.projects.len() {
            let p = &self.projects[pi];
            let space = p.space.as_ref().and_then(|id| self.spaces.iter().find(|s| &s.id == id)).map(|s| &s.settings);
            let resolved = layers::resolve(&global, space, Some(&p.team), &p.settings);
            // The first time always: what tasks read of it (layers::of_root) is set then.
            if *p.resolved == resolved && p.resolved_once {
                continue;
            }
            self.projects[pi].resolved_once = true;
            let before = std::mem::replace(&mut self.projects[pi].resolved, Arc::new(resolved));
            let p = &self.projects[pi];
            let now = p.values();
            // A project's own folder; a space's folder of all, the project's in it; else Settings.
            let folder = match (now.worktrees.trim(), p.resolved.from.worktrees_dir) {
                ("", Layer::Space) if !now.worktrees_dir.trim().is_empty() => crate::git::home_expanded(now.worktrees_dir.trim()).join(&p.name).display().to_string(),
                (own, _) => own.to_string(),
            };
            crate::git::set_project_worktrees(&p.path, &folder);
            crate::integrations::set_project(&p.path, crate::integrations::of_resolved(&p.resolved));
            crate::layers::set_project(&p.path, p.resolved.clone());
            if before.values.language_servers != now.language_servers {
                let on = now.language_servers;
                for view in p.tasks.iter().filter_map(|t| t.view.clone()) {
                    view.update(cx, |t, cx| {
                        t.lsp_off = !on;
                        t.set_language_servers(false, cx);
                        if on && !crate::lsp::POWER_SAVING.load(std::sync::atomic::Ordering::Relaxed) {
                            t.set_language_servers(true, cx);
                        }
                    });
                }
            }
        }
        cx.notify();
    }

    /// A project's own layer, as Settings or a menu left it.
    pub fn set_project_settings(&mut self, pi: usize, settings: Overrides, cx: &mut Context<Self>) {
        let Some(p) = self.projects.get_mut(pi) else { return };
        if p.settings == settings {
            return;
        }
        p.settings = settings.allowed_in(Layer::Project);
        self.resolve_projects(cx);
        self.save_later(cx);
    }

    /// Reads the team's layer of each project out of the `.ideae.toml` of
    /// its main checkout. Not a task's worktree, unlike `[task]`
    /// (project_config::resolve): an agent writes in its worktree, and what
    /// it wrote there must not name where tickets are or what is asked of
    /// whom. The main checkout is not trusted for that either; it is only
    /// not the agent's. A project out of reach keeps the layer last read.
    pub fn read_team_layers(&mut self, cx: &mut Context<Self>) {
        self.read_team_layers_of(true, cx)
    }

    /// As `read_team_layers`; `remote`: those on other machines too, each read a call over ssh.
    pub fn read_team_layers_of(&mut self, remote: bool, cx: &mut Context<Self>) {
        self.read_team_layers_where(&|p| remote || !crate::hosts::is_remote(&p.path), cx)
    }

    /// The team's layer of the project at `path` alone, now: its file changed there.
    pub fn read_team_layer_of(&mut self, path: &std::path::Path, cx: &mut Context<Self>) {
        self.read_team_layers_where(&|p| p.path == path, cx)
    }

    fn read_team_layers_where(&mut self, which: &dyn Fn(&crate::workspace::Project) -> bool, cx: &mut Context<Self>) {
        let projects: Vec<(PathBuf, Option<u64>)> = self
            .projects
            .iter()
            .filter(|p| which(p) && crate::hostfs::reachable(&p.path))
            .map(|p| (p.path.clone(), p.team_read))
            .collect();
        if projects.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let read = cx
                .background_spawn(async move {
                    let mut changed = Vec::new();
                    for (path, had) in projects {
                        // No file is no team layer; a read that failed (ssh, a lock) keeps the one read before.
                        let text = match crate::hostfs::read_to_string(&path.join(crate::services::FILE)) {
                            Ok(text) => text,
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                            Err(_) => continue,
                        };
                        let hash = {
                            use std::hash::{Hash as _, Hasher as _};
                            let mut h = std::collections::hash_map::DefaultHasher::new();
                            text.hash(&mut h);
                            h.finish()
                        };
                        if had == Some(hash) {
                            continue;
                        }
                        // A file that does not parse leaves the team's layer as it was; Project settings says why.
                        let layer = layers::team_layer(&text).ok();
                        changed.push((path, hash, layer));
                    }
                    changed
                })
                .await;
            if read.is_empty() {
                return;
            }
            let _ = this.update(cx, |w, cx| {
                for (path, hash, layer) in read {
                    if let Some(p) = w.projects.iter_mut().find(|p| p.path == path) {
                        p.team_read = Some(hash);
                        if let Some(layer) = layer {
                            p.team = layer;
                        }
                    }
                }
                w.resolve_projects(cx);
                if w.read_old_links() {
                    w.save_later(cx);
                }
            });
        })
        .detach();
    }

    /// What a value is called where it is shown: an agent by its name.
    pub fn describe_value(id: &str, v: &Values) -> String {
        let flag = |on: bool| if on { "on" } else { "off" }.to_string();
        let text = |s: &str| if s.trim().is_empty() { "none".to_string() } else { s.trim().to_string() };
        match id {
            "agent" => crate::state::with_settings(|s| s.profile(&v.agent).or_else(|| s.default_profile()).map(|p| p.name.clone())).unwrap_or_else(|| "a shell".into()),
            "worktrees_dir" => text(&v.worktrees_dir),
            "worktrees" => text(&v.worktrees),
            "base" => if v.base.trim().is_empty() { "the remote's default branch".into() } else { v.base.trim().to_string() },
            "pull_requests" => flag(v.pull_requests),
            "remote_browser" => flag(v.remote_browser),
            "language_servers" => flag(v.language_servers),
            "check_to_agent" => v.check_to_agent.to_string(),
            "branch" => v.branch.clone(),
            "pr_title" => v.pr_title.clone(),
            "closes" => v.closes.clone(),
            _ => String::new(),
        }
    }

    /// Tells what a project's move to another space changed for it.
    pub fn tell_layer_changes(&mut self, pi: usize, before: &layers::Resolved, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.projects.get(pi) else { return };
        let told = layers::changes(before, &p.resolved, Self::describe_value);
        if told.is_empty() {
            return;
        }
        let name = p.name.clone();
        window.push_notification(Notification::info(told.join("\n")).title(format!("{name}: settings of its new space")), cx);
    }

    /// The layers under Global as Settings edits them.
    pub fn layer_drafts(&self) -> crate::layers_ui::Layers {
        use crate::layers_ui::{Layers, ProjectDraft, SpaceDraft};
        Layers {
            spaces: self.spaces.iter().map(|s| SpaceDraft { id: s.id.clone(), name: s.name.clone(), settings: s.settings.clone() }).collect(),
            projects: self
                .projects
                .iter()
                .map(|p| ProjectDraft { path: p.path.clone(), name: p.name.clone(), space: p.space.clone(), settings: p.settings.clone(), team: p.team.clone() })
                .collect(),
        }
    }

    /// What Settings changed of the spaces' and projects' layers; worked
    /// out again by `apply_settings`, which follows.
    pub fn apply_layer_drafts(&mut self, drafts: crate::layers_ui::Layers, cx: &mut Context<Self>) {
        for draft in drafts.spaces {
            if let Some(space) = self.spaces.iter_mut().find(|s| s.id == draft.id) {
                space.settings = draft.settings.allowed_in(Layer::Space);
            }
        }
        for draft in drafts.projects {
            if let Some(p) = self.projects.iter_mut().find(|p| p.path == draft.path) {
                p.settings = draft.settings.allowed_in(Layer::Project);
            }
        }
        self.save_later(cx);
    }

    /// Settings over a project's own layer: from Project settings.
    pub fn open_project_settings_layer(&mut self, pi: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.open_settings_over(0, crate::layers_ui::Scope::Project(pi), window, cx);
    }

    /// Where a value of project `pi` comes from, as it is told.
    pub fn source_label(&self, pi: usize, from: Layer) -> String {
        match from {
            Layer::Space => self.projects.get(pi).and_then(|p| self.space_name(p.space.as_deref())).map(|name| format!("space {name}")).unwrap_or_else(|| "its space".into()),
            Layer::Project => "this project".into(),
            Layer::Global => "Settings".into(),
            other => other.label().into(),
        }
    }

    /// Tasks of before links get theirs, once: the addresses of their
    /// prompt, the keys of their branch an integration owns. Only once the
    /// team's layer is read, for the integrations it sets up. Whether any did.
    pub fn read_old_links(&mut self) -> bool {
        let mut any = false;
        for p in self.projects.iter_mut().filter(|p| p.team_read.is_some()) {
            let integrations = crate::integrations::of_resolved(&p.resolved);
            for t in p.tasks.iter_mut().filter(|t| !t.links_read && !t.main && t.side.is_none()) {
                t.links = crate::integrations::tickets::links_of_old_task(&t.prompt, &t.branch, &integrations);
                t.links_read = true;
                any = true;
            }
        }
        any
    }

    /// How a project names a ticket's branch.
    pub fn branch_template_of(&self, project: &std::path::Path) -> String {
        self.projects.iter().find(|p| p.path == project).map(|p| p.values().branch.clone()).unwrap_or_else(|| crate::naming::DEFAULT_TEMPLATE.into())
    }

    /// The key a task's commit box starts with: its first ticket's, a key
    /// as trackers write them, where its project's commits carry keys.
    pub fn commit_key(&self, pi: usize, ti: usize) -> Option<String> {
        let p = self.projects.get(pi)?;
        if p.keys_in_history != Some(true) || !crate::state::with_settings(|s| s.tickets) {
            return None;
        }
        let ticket = p.tasks.get(ti)?.ticket()?;
        crate::integrations::tickets::is_key(&ticket.key).then(|| ticket.key.clone())
    }

    /// Reads whether each project's recent commits carry tickets' keys,
    /// once a run, off the UI thread; then tells its open tasks.
    pub fn read_key_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let due: Vec<std::path::PathBuf> = self.projects.iter().filter(|p| p.keys_in_history.is_none() && crate::hostfs::reachable(&p.path)).map(|p| p.path.clone()).collect();
        if due.is_empty() {
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            let found = cx
                .background_spawn(async move {
                    due.into_iter()
                        .map(|repo| {
                            let subjects = crate::git::run(&repo, "git", &["log", "-50", "--no-merges", "--format=%s"]).unwrap_or_default();
                            let keyed = subjects.lines().filter(|s| !crate::integrations::tickets::keys_in(s).is_empty()).count();
                            // Most of them, not one by chance.
                            let total = subjects.lines().count();
                            (repo, total > 0 && keyed * 2 >= total)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            let _ = this.update_in(cx, |w, window, cx| {
                for (repo, keyed) in found {
                    if let Some(p) = w.projects.iter_mut().find(|p| p.path == repo) {
                        p.keys_in_history = Some(keyed);
                    }
                }
                w.tell_commit_keys(window, cx);
            });
        })
        .detach();
    }

    /// Each open task's commit box is told the key it starts with.
    pub fn tell_commit_keys(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut told = Vec::new();
        for (pi, p) in self.projects.iter().enumerate() {
            for (ti, t) in p.tasks.iter().enumerate() {
                if let Some(view) = &t.view {
                    told.push((view.clone(), self.commit_key(pi, ti)));
                }
            }
        }
        for (view, key) in told {
            view.update(cx, |t, cx| t.set_commit_key(key, window, cx));
        }
    }
}
