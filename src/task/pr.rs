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

// ---------------------------------------------------------------- a request made

impl TaskView {
    /// What a request of this task says of its tickets, as its project's
    /// layers set it, `closes` as chosen.
    pub(super) fn request_text(&self, closes: &str) -> git::RequestText {
        let values = &crate::layers::of_root(&self.root).values;
        let tickets: Vec<(String, String)> = if crate::state::with_settings(|s| s.tickets) {
            self.links.iter().filter(|l| l.is_ticket()).map(|l| (l.key.clone(), l.url.clone())).collect()
        } else {
            Vec::new()
        };
        git::RequestText {
            title_template: values.pr_title.clone(),
            key: tickets.first().map(|(k, _)| k.clone()).unwrap_or_default(),
            tickets,
            closes: closes.to_string(),
        }
    }

    /// Makes the branch's request, or opens the one there. With tickets,
    /// asks first what it says of them: Fixes (its tracker may close them
    /// when it is merged), Refs, or nothing.
    pub(super) fn make_request(&mut self, upstream: bool, window: &mut Window, cx: &mut Context<Self>) {
        // A review's branch with no request yet: making one pushes it.
        if self.review && self.pr.is_none() {
            return self.confirm_review_push(Rc::new(move |t: &mut Self, window: &mut Window, cx: &mut Context<Self>| t.make_request_now(upstream, window, cx)), window, cx);
        }
        self.make_request_now(upstream, window, cx);
    }

    fn make_request_now(&mut self, upstream: bool, window: &mut Window, cx: &mut Context<Self>) {
        let tickets: Vec<String> = self.links.iter().filter(|l| l.is_ticket()).map(|l| l.key.clone()).collect();
        // Its request there already: opened, nothing to ask.
        if tickets.is_empty() || self.pr.is_some() || !crate::state::with_settings(|s| s.tickets) {
            let text = self.request_text("none");
            return self.run_action("PR", move |root| git::pull_request(&root, upstream, &text), true, window, cx);
        }
        let first = crate::layers::of_root(&self.root).values.closes.clone();
        let chosen = std::rc::Rc::new(std::cell::RefCell::new(first));
        let view = cx.weak_entity();
        let template = crate::layers::of_root(&self.root).values.pr_title.clone();
        let title = crate::layers::request_title(&template, &tickets[0], "…");
        window.open_dialog(cx, move |dialog, _, cx| {
            let (chosen, view) = (chosen.clone(), view.clone());
            let muted = cx.theme().muted_foreground;
            let now = chosen.borrow().clone();
            let mut choices = v_flex().gap_1();
            for (word, label) in crate::layers_ui::CLOSES {
                let chosen = chosen.clone();
                choices = choices.child(
                    gpui_kit::component::checkbox::Checkbox::new(SharedString::from(format!("request-{word}"))).label(format!("{label}: {}", tickets.join(", "))).checked(now == word).on_click(move |_, window, _| {
                        *chosen.borrow_mut() = word.to_string();
                        window.refresh();
                    }),
                );
            }
            let make = move |window: &mut Window, cx: &mut App| {
                window.close_dialog(cx);
                let closes = chosen.borrow().clone();
                let _ = view.update(cx, |t, cx| {
                    let text = t.request_text(&closes);
                    t.run_action("PR", move |root| git::pull_request(&root, upstream, &text), true, window, cx)
                });
            };
            let make = std::rc::Rc::new(make);
            let (ok, button) = (make.clone(), make.clone());
            dialog
                .title("Pull request")
                .w(px(520.))
                .on_ok(move |_, window, cx| {
                    ok(window, cx);
                    false
                })
                .child(
                    v_flex()
                        .gap_2()
                        .child(div().text_sm().child(format!("Its title: {title}")))
                        .child(div().text_xs().text_color(muted).child("The description says of its tickets:"))
                        .child(choices),
                )
                .footer(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(div().flex_1())
                        .child(Button::new("request-cancel").ghost().label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                        .child(Button::new("request-make").primary().label("Create").on_click(move |_, window, cx| button(window, cx))),
                )
        });
    }
}

// ---------------------------------------------------------------- the key in commits

impl TaskView {
    /// What an empty commit box starts with: its ticket's key, or nothing.
    pub(super) fn commit_start(&self) -> String {
        self.commit_key.as_ref().map(|k| format!("{k} ")).unwrap_or_default()
    }

    /// Whether the commit box holds nothing of anyone's: empty, or its key alone.
    pub(super) fn commit_box_blank(&self, cx: &App) -> bool {
        let now = self.commit_input.read(cx).value();
        let now = now.trim();
        now.is_empty() || now == self.commit_start().trim()
    }

    /// The key its commit box starts with, set again; an empty box takes it.
    pub fn set_commit_key(&mut self, key: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        if self.commit_key == key {
            return;
        }
        let was = self.commit_start();
        self.commit_key = key;
        let now = self.commit_input.read(cx).value().to_string();
        if now.trim().is_empty() || now == was {
            let start = self.commit_start();
            self.commit_input.update(cx, |i, cx| i.set_value(start, window, cx));
        }
    }
}
