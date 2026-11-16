use super::*;

use crate::replace::{Fingerprint, Outcome, Plan, Report};

impl TaskView {
    pub fn open_replace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        crate::replace::open(self.root.clone(), cx.weak_entity(), window, cx);
    }

    /// The open files with edits not saved, or at odds with the disk.
    pub fn unsaved_paths(&self, cx: &App) -> HashSet<PathBuf> {
        self.tabs
            .iter()
            .filter_map(|tab| match &tab.kind {
                TabKind::File(file) => Some(file.read(cx)).filter(|f| f.dirty || f.conflict).map(|f| f.path.clone()),
                _ => None,
            })
            .collect()
    }

    /// Files open here are edited in their editor, as one undo, and saved;
    /// one with unsaved edits is left alone. The others are written off the
    /// UI thread. Each only if it still holds what was searched.
    pub fn replace_in_files(&mut self, plans: Vec<(String, Plan)>, window: &mut Window, cx: &mut Context<Self>) -> Task<Report> {
        let open: HashMap<PathBuf, Entity<FileView>> = self
            .tabs
            .iter()
            .filter_map(|tab| match &tab.kind {
                TabKind::File(file) => Some((file.read(cx).path.clone(), file.clone())),
                _ => None,
            })
            .collect();
        let mut report = Report::default();
        let mut on_disk = Vec::new();
        for (name, plan) in plans {
            let Some(file) = open.get(&plan.path) else {
                on_disk.push((name, plan));
                continue;
            };
            let outcome = file.update(cx, |f, cx| {
                if f.dirty || f.conflict {
                    return Outcome::Unsaved;
                }
                let rope = f.editor.read(cx).text();
                if Fingerprint::of_chunks(rope.chunks().map(str::as_bytes)) != plan.print {
                    return Outcome::Changed;
                }
                f.edit(window, cx, |text, selection| {
                    crate::edit::rewrite(text, &crate::replace::apply(text, &plan.edits), selection.start)
                });
                f.save(cx);
                Outcome::Replaced(plan.edits.len())
            });
            report.done.push((name, outcome));
        }
        cx.background_spawn(async move {
            report.done.extend(on_disk.into_iter().map(|(name, plan)| {
                let outcome = crate::replace::write(&plan);
                (name, outcome)
            }));
            report
        })
    }
}
