use super::*;

// ---------------------------------------------------------------- PR feedback

impl TaskView {
    /// "PR #12: 3 comments → agent", while reviewers left something open.
    pub(super) fn render_pr_feedback(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let pr = self.pr.clone().filter(|p| p.open_feedback > 0)?;
        let n = pr.open_feedback;
        Some(
            Button::new("pr-feedback")
                .xsmall()
                .ghost()
                .disabled(self.busy.is_some())
                .label(format!("PR #{}: {n} comment{} → agent", pr.number, if n == 1 { "" } else { "s" }))
                .tooltip("Put the unresolved review threads and change requests into the agent's prompt")
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.send_pr_feedback(pr.clone(), window, cx)
                }))
                .into_any_element(),
        )
    }

    pub(super) fn send_pr_feedback(&mut self, pr: crate::pr::Pr, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some("PR feedback");
        cx.notify();
        let root = self.root.clone();
        cx.spawn_in(window, async move |this, cx| {
            let job_pr = pr.clone();
            let feedback = cx.background_spawn(crate::procs::on_thread(move || { crate::pr::feedback(&root, &job_pr) })).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = None;
                match feedback {
                    Some(f) if f.len() > 0 => {
                        this.paste_to_agent(f.message(&pr), window, cx);
                        this.action = Some((true, format!("{} review point(s) in the agent's prompt", f.len())));
                    }
                    Some(_) => this.action = Some((true, "No open review comments".into())),
                    None => this.action = Some((false, "PR feedback: gh could not read the review".into())),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

/// `file` as the panels name it: relative to the worktree, with slashes.
/// The agent may name the worktree through another path than the app does
/// (a symlink on the way), so the real paths are compared too.
pub(super) fn path_in(root: &std::path::Path, file: &std::path::Path) -> Option<String> {
    let real = |path: &std::path::Path| match (path.parent(), path.file_name()) {
        // The file itself may not be there yet.
        (Some(parent), Some(name)) => parent.canonicalize().ok().map(|parent| parent.join(name)),
        _ => None,
    };
    let rel = match file.strip_prefix(root) {
        Ok(rel) => rel.to_path_buf(),
        Err(_) => real(file)?.strip_prefix(root.canonicalize().ok()?).ok()?.to_path_buf(),
    };
    let parts: Vec<_> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    (!parts.is_empty()).then(|| parts.join("/"))
}

#[cfg(test)]
mod path_in_tests {
    use std::path::Path;

    use super::path_in;

    #[test]
    fn relative_to_the_worktree_also_through_a_symlink() {
        assert_eq!(path_in(Path::new("/w/t"), Path::new("/w/t/src/a.rs")).as_deref(), Some("src/a.rs"));
        assert_eq!(path_in(Path::new("/w/t"), Path::new("/w/other/a.rs")), None);
        #[cfg(unix)]
        {
            let dir = std::env::temp_dir().join(format!("ideae-path-in-{}", std::process::id()));
            let (real, link) = (dir.join("real"), dir.join("link"));
            std::fs::create_dir_all(real.join("src")).unwrap();
            std::os::unix::fs::symlink(&real, &link).unwrap();
            assert_eq!(path_in(&link, &real.join("src/new.rs")).as_deref(), Some("src/new.rs"));
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
