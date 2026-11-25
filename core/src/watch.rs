//! Watching a task's folders: each on its own (the folders of its files)
//! and some with all that is in them (git's refs). The window does it for
//! a task here, a daemon for a window elsewhere.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use notify::{RecursiveMode, Watcher as _};

/// macOS restarts its FSEvents stream, with every path again, at each
/// directory added or removed, and loses what happens meanwhile: a project
/// of a thousand directories came to see its changes only at the poll ten
/// seconds later. There one stream watches the few trees that hold them all,
/// and the events of the directories not asked for are dropped here.
/// `IDEAE_WHOLE_TREES=1` does it elsewhere too, to try it.
pub fn whole_trees() -> bool {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| cfg!(target_os = "macos") || std::env::var_os("IDEAE_WHOLE_TREES").is_some());
    *ON
}

#[derive(Default)]
pub struct Filter {
    flat: Arc<BTreeSet<PathBuf>>,
    deep: Vec<PathBuf>,
    /// A tree's path as FSEvents tells it (symlinks resolved, as /private/tmp
    /// for /tmp) and as it is known here.
    aliases: Vec<(PathBuf, PathBuf)>,
}

impl Filter {
    /// A path of the directories watched each on its own (or one of them),
    /// or of the trees watched with all that is in them.
    pub fn lets_through(&self, path: &Path) -> bool {
        let known = self.aliases.iter().find_map(|(real, known)| {
            let rest = path.strip_prefix(real).ok()?;
            Some(if rest.as_os_str().is_empty() { known.clone() } else { known.join(rest) })
        });
        let path = known.as_deref().unwrap_or(path);
        self.flat.contains(path) || path.parent().is_some_and(|dir| self.flat.contains(dir)) || self.deep.iter().any(|d| path.starts_with(d))
    }
}

/// The fewest directories whose trees hold all of `dirs`.
pub fn covering_trees<'a>(dirs: impl Iterator<Item = &'a PathBuf>) -> Vec<PathBuf> {
    let mut dirs: Vec<&PathBuf> = dirs.collect();
    dirs.sort();
    let mut trees: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        // Sorted, a tree's directories follow it.
        if !trees.last().is_some_and(|tree| dir.starts_with(tree)) {
            trees.push(dir.clone());
        }
    }
    trees
}

/// A watcher and what it watches now.
pub struct Watch {
    watcher: Option<notify::RecommendedWatcher>,
    watched: (Arc<BTreeSet<PathBuf>>, Vec<PathBuf>),
    /// On macOS the trees watched whole, and what lets their events through.
    trees: Vec<PathBuf>,
    filter: Arc<RwLock<Filter>>,
}

impl Watch {
    /// `on_change` is told, from the watcher's thread, of any change in
    /// what is watched (nothing of what was only read).
    pub fn new(on_change: impl Fn() + Send + 'static) -> Self {
        let filter: Arc<RwLock<Filter>> = Arc::default();
        let events = filter.clone();
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if let Ok(event) = event {
                let wanted = !whole_trees() || event.paths.is_empty() || {
                    let filter = events.read().unwrap_or_else(std::sync::PoisonError::into_inner);
                    event.paths.iter().any(|path| filter.lets_through(path))
                };
                if wanted && !matches!(event.kind, notify::EventKind::Access(_)) {
                    on_change();
                }
            }
        })
        .inspect_err(|e| eprintln!("file watcher off, polling only: {e}"))
        .ok();
        Watch { watcher, watched: Default::default(), trees: Vec::new(), filter }
    }

    /// Watches `flat` each on its own and `deep` with all in them, and no longer what else it did.
    pub fn sync(&mut self, flat: Arc<BTreeSet<PathBuf>>, deep: Vec<PathBuf>) {
        if (Arc::ptr_eq(&flat, &self.watched.0) || flat == self.watched.0) && deep == self.watched.1 {
            return;
        }
        let Some(watcher) = self.watcher.as_mut() else { return };
        if whole_trees() {
            let trees = covering_trees(flat.iter().chain(&deep));
            if trees != self.trees {
                for tree in self.trees.iter().filter(|t| !trees.contains(t)) {
                    let _ = watcher.unwatch(tree);
                }
                for tree in trees.iter().filter(|t| !self.trees.contains(t)) {
                    let _ = watcher.watch(tree, RecursiveMode::Recursive);
                }
                self.trees = trees;
            }
            let aliases = self.trees.iter().filter_map(|t| t.canonicalize().ok().filter(|real| real != t).map(|real| (real, t.clone()))).collect();
            *self.filter.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Filter { flat: flat.clone(), deep: deep.clone(), aliases };
            self.watched = (flat, deep);
            return;
        }
        let (old_flat, old_deep) = std::mem::take(&mut self.watched);
        for dir in old_flat.difference(&flat) {
            let _ = watcher.unwatch(dir);
        }
        for dir in flat.difference(&old_flat) {
            let _ = watcher.watch(dir, RecursiveMode::NonRecursive);
        }
        for dir in old_deep.iter().filter(|d| !deep.contains(d)) {
            let _ = watcher.unwatch(dir);
        }
        for dir in deep.iter().filter(|d| !old_deep.contains(d)) {
            let _ = watcher.watch(dir, RecursiveMode::Recursive);
        }
        self.watched = (flat, deep);
    }
}

#[cfg(test)]
mod watch_tests {
    use super::{Filter, covering_trees};
    use std::{collections::BTreeSet, path::PathBuf, sync::Arc};

    #[test]
    fn whole_trees_and_what_they_let_through() {
        let p = PathBuf::from;
        let flat: BTreeSet<PathBuf> = ["/w", "/w/src", "/w/src/a", "/repo/.git/worktrees/w"].map(p).into();
        let deep = vec![p("/repo/.git/refs")];
        assert_eq!(covering_trees(flat.iter().chain(&deep)), [p("/repo/.git/refs"), p("/repo/.git/worktrees/w"), p("/w")]);
        let filter = Filter { flat: Arc::new(flat), deep, aliases: vec![(p("/private/w"), p("/w"))] };
        for yes in ["/w/Cargo.toml", "/w/src/main.rs", "/w/src/a", "/repo/.git/refs/heads/main", "/repo/.git/worktrees/w/HEAD", "/private/w/src/a/b.rs", "/private/w"] {
            assert!(filter.lets_through(&p(yes)), "{yes}");
        }
        for no in ["/w/target/debug/x", "/w/src/a/deep/c.rs", "/repo/.git/objects/ab/cd", "/private/w/target/x"] {
            assert!(!filter.lets_through(&p(no)), "{no}");
        }
    }
}
