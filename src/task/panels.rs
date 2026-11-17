use super::*;
use gpui_kit::component::menu::PopupMenuItem;

impl TaskView {
    pub(super) fn render_tabs(&self, right: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let shown = if right { self.right_active } else { Some(self.left_active) };
        let in_pane: Vec<usize> = (0..self.tabs.len()).filter(|&ix| self.tabs[ix].right == right).collect();
        let selected = in_pane.iter().position(|&ix| Some(self.tabs[ix].id) == shown).unwrap_or(0);
        let can_split = !right && in_pane.len() > 1;
        let map = in_pane.clone();
        let menu_tab = self.menu_tab.clone();
        let bar = TabBar::new(if right { "tabs-right" } else { "tabs-left" })
            .selected_index(selected)
            .on_click(cx.listener(move |this, pos: &usize, window, cx| {
                let Some(&ix) = map.get(*pos) else { return };
                // The tab bar takes the clicks for itself, so a double click is
                // told apart here: the same tab again within 400 ms.
                let id = this.tabs[ix].id;
                let now = std::time::Instant::now();
                let double = this.last_tab_click.is_some_and(|(last, at)| last == id && now - at < Duration::from_millis(400));
                this.last_tab_click = Some((id, now));
                this.activate(ix, window, cx);
                if double {
                    this.last_tab_click = None;
                    // A preview tab double-clicked is kept, as in other editors.
                    if std::mem::take(&mut this.tabs[ix].preview) {
                        cx.notify();
                    } else {
                        this.toggle_zoom(right, window, cx);
                    }
                }
            }))
            .suffix(
                h_flex()
                    .child(
                        Button::new("zoom")
                            .small()
                            .ghost()
                            .label(if self.zoomed.is_some() { "⤡" } else { "⤢" })
                            .selected(self.zoomed.is_some())
                            .tooltip("Focus mode: this pane over everything (Shift+Esc, or double-click a tab)")
                            .on_click(cx.listener(move |this, _, window, cx| this.toggle_zoom(right, window, cx))),
                    )
                    .child(
                        Button::new("new-terminal")
                            .small()
                            .ghost()
                            .label("+")
                            .tooltip("New terminal here")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.focus_right = right && this.is_split();
                                this.new_terminal(window, cx)
                            })),
                    )
                    .child(
                        Button::new("new-browser")
                            .small()
                            .ghost()
                            .label("Web")
                            .tooltip("New browser tab here")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.focus_right = right && this.is_split();
                                this.new_browser(window, cx)
                            })),
                    )
                    .when(!right, |bar| {
                        bar.child(
                            Button::new("split")
                                .small()
                                .ghost()
                                .label("◫")
                                .disabled(!can_split)
                                .tooltip("Move this tab to the right pane (or from a tab's right-click menu)")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    if let Some(ix) = this.ix_of(this.left_active) {
                                        this.move_to_other_pane(ix, window, cx)
                                    }
                                })),
                        )
                    })
                    .when(right, |bar| {
                        bar.child(
                            Button::new("join")
                                .small()
                                .ghost()
                                .label("▣")
                                .tooltip("Join: move these tabs back to the left pane")
                                .on_click(cx.listener(|this, _, window, cx| this.join_panes(window, cx))),
                        )
                    }),
            )
            .children(in_pane.into_iter().map(|ix| (ix, &self.tabs[ix])).map(|(ix, tab)| {
                let mut label = tab.title.clone();
                if matches!(tab.kind, TabKind::Terminal(_)) && !tab.shown.is_empty() {
                    label = format!("{}: {}", tab.title, short_title(&tab.shown)).into();
                }
                if tab.title == "agent" {
                    if let Some((glyph, _)) = status_glyph(&self.agent_status) {
                        label = format!("{glyph} {label}").into();
                    }
                }
                if matches!(tab.kind, TabKind::Browser(_)) && !tab.shown.is_empty() && tab.shown != "about:blank" {
                    label = short_title(&tab.shown).into();
                }
                if matches!(tab.kind, TabKind::File(_)) {
                    if tab.dirty {
                        label = format!("● {label}").into();
                    }
                }
                if tab.pinned {
                    label = format!("⚑ {label}").into();
                }
                // Middle click closes what the user opened, never the agent, the
                // first shell or the Changes/History views.
                let middle_closes = match &tab.kind {
                    TabKind::File(_) | TabKind::Browser(_) | TabKind::Binary(_) => true,
                    TabKind::Terminal(_) => tab.title != "agent" && tab.title != "shell",
                    _ => false,
                };
                let dragged = DraggedTab { tab_id: tab.id, title: label.clone() };
                let item = if tab.title == "agent" {
                    let root = self.root.clone();
                    Tab::new().child(
                        div()
                            .id("agent-usage")
                            .child(label)
                            .tooltip(move |_, cx| cx.new(|cx| crate::usage::UsageTooltip::new(root.clone(), cx)).into()),
                    )
                } else {
                    Tab::new().label(label).when(tab.preview, |tab| tab.italic())
                };
                let id = tab.id;
                let menu_tab = self.menu_tab.clone();
                let item = item
                    .on_mouse_down(MouseButton::Right, move |_, _, _| menu_tab.set(Some(id)))
                    .when(middle_closes, |tab| {
                        tab.on_mouse_down(
                            MouseButton::Middle,
                            cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_tab(ix, window, cx)
                            }),
                        )
                    })
                    .on_drag(dragged, |dragged, _, _, cx| {
                        TAB_DRAG.store(true, std::sync::atomic::Ordering::Relaxed);
                        cx.new(|_| dragged.clone())
                    });
                let closable = match &tab.kind {
                    TabKind::Terminal(_) => tab.title != "agent",
                    TabKind::File(_) | TabKind::Browser(_) | TabKind::Binary(_) => true,
                    TabKind::Changes | TabKind::History => true,
                };
                if closable {
                    item.suffix(
                        Button::new(("close", ix))
                            .xsmall()
                            .ghost()
                            .label("×")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_tab(ix, window, cx)
                            })),
                    )
                } else {
                    item
                }
            }));
        let task = cx.weak_entity();
        div().id(if right { "tab-bar-right" } else { "tab-bar-left" }).w_full().child(bar).context_menu(move |menu, _, cx| {
            let Some(view) = task.upgrade() else { return menu };
            let clicked = menu_tab.take();
            let t = view.read(cx);
            let tab = clicked.and_then(|id| t.tabs.iter().find(|t| t.id == id));
            let reopen = !t.closed.is_empty();
            let can_move = tab.is_some_and(|tab| tab.right || t.tabs.iter().filter(|t| !t.right).count() > 1);
            let (pinned, id) = (tab.is_some_and(|t| t.pinned), tab.map(|t| t.id));
            let act = |label: &'static str, f: fn(&mut TaskView, Option<usize>, bool, &mut Window, &mut Context<TaskView>)| {
                let view = view.downgrade();
                PopupMenuItem::new(label).on_click(move |_, window, cx| {
                    let _ = view.update(cx, |t, cx| f(t, id, right, window, cx));
                })
            };
            let menu = match id {
                Some(_) => menu
                    .item(act("Close", |t, id, _, window, cx| {
                        if let Some(ix) = id.and_then(|id| t.ix_of(id)) {
                            t.close_tab(ix, window, cx)
                        }
                    }))
                    .item(act("Close others", |t, id, right, window, cx| t.close_many(right, |_, tab| Some(tab.id) != id, window, cx)))
                    .item(act("Close to the right", |t, id, right, window, cx| {
                        let from = id.and_then(|id| t.ix_of(id)).unwrap_or(usize::MAX);
                        t.close_many(right, |ix, _| ix > from, window, cx)
                    }))
                    .item(act("Close all", |t, _, right, window, cx| t.close_many(right, |_, _| true, window, cx)))
                    .separator()
                    .item(act(if pinned { "Unpin" } else { "Pin" }, |t, id, _, _, cx| {
                        if let Some(id) = id {
                            t.toggle_pin(id, cx)
                        }
                    }))
                    .when(can_move, |menu| {
                        menu.item(act(if right { "Move to the left pane" } else { "Move to the right pane" }, |t, id, _, window, cx| {
                            if let Some(ix) = id.and_then(|id| t.ix_of(id)) {
                                t.move_to_other_pane(ix, window, cx)
                            }
                        }))
                    }),
                None => menu.item(act("Close all", |t, _, right, window, cx| t.close_many(right, |_, _| true, window, cx))),
            };
            menu.when(reopen, |menu| menu.separator().item(act("Reopen closed tab", |t, _, _, window, cx| t.reopen_closed(window, cx))))
        })
    }

    pub(super) fn render_files(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("changed-files")
            .track_focus(&self.side_focus)
            .on_key_down(cx.listener(|this, e: &KeyDownEvent, window, cx| this.side_key(e, window, cx)))
            .size_full()
            .border_l_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("pane-changes")
                            .small()
                            .ghost()
                            .label(format!("Changes ({})", self.files.len()))
                            .selected(self.right == RightPane::Changes)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.right = RightPane::Changes;
                                this.fill_tree(cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("pane-files")
                            .small()
                            .ghost()
                            .label("Files")
                            .selected(self.right == RightPane::Files)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.right = RightPane::Files;
                                this.fill_tree(cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("pane-structure")
                            .small()
                            .ghost()
                            .label("Structure")
                            .selected(self.right == RightPane::Structure)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.right = RightPane::Structure;
                                this.fill_tree(cx);
                                cx.notify();
                            })),
                    )
                    .child({
                        let running = self.running_services(cx).len();
                        Button::new("pane-run")
                            .small()
                            .ghost()
                            .label(if running > 0 { format!("Run ● {running}") } else { "Run".to_string() })
                            .tooltip(format!("Dev servers and workers of this task ({})", crate::keymap::label("run_panel")))
                            .selected(self.right == RightPane::Run)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.right = RightPane::Run;
                                this.fill_tree(cx);
                                this.refresh_services(cx);
                                this.watch_listening(cx);
                                cx.notify();
                            }))
                    }),
            )
            .when(self.right == RightPane::Run, |this| this.child(self.render_run_panel(cx)))
            .when(self.right == RightPane::Files, |this| this.child(self.render_tree(cx)))
            .when(self.right == RightPane::Structure, |this| this.child(self.render_structure(cx)))
            .when(self.right == RightPane::Changes, |this| {
                this.child(self.render_change_lists(cx)).child(self.render_commit_box(cx))
            })
    }

    pub(super) fn render_structure(&self, cx: &mut Context<Self>) -> AnyElement {
        let file = self.structure_file.and_then(|id| {
            self.tabs.iter().find_map(|t| match &t.kind {
                TabKind::File(f) if t.id == id => Some((id, f.clone())),
                _ => None,
            })
        });
        let Some((id, file)) = file else {
            return div()
                .p_3()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("Open a file to see its structure")
                .into_any_element();
        };
        let (path, cursor) = {
            let f = file.read(cx);
            (f.path.clone(), f.cursor_line)
        };
        self.refresh_structure(cx);
        let items = self.structure_items(cx);
        let keyboard = self.side_has_focus.get();
        // The innermost declaration the cursor is in: the last one starting at or above it.
        let current = items.iter().rposition(|i| i.line <= cursor);
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        v_flex()
            .id("structure")
            .flex_1()
            .min_h_0()
            .child(div().px_3().py_1().text_xs().text_color(cx.theme().muted_foreground).child(name))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    // Only the declarations in view are laid out: a file can have thousands.
                    .child(
                        uniform_list(
                            "declarations",
                            items.len(),
                            cx.processor(move |this: &mut Self, range: std::ops::Range<usize>, _, cx| {
                                range
                                    .map(|ix| {
                                        let item = &items[ix];
                                        let (file, line) = (file.clone(), item.line);
                                        // "function open" read slowly down a list: a colored
                                        // letter for the kind, the word in the tooltip.
                                        let (kind, name) = item.label.rsplit_once(' ').unwrap_or(("", item.label.as_str()));
                                        let (mark, color) = kind_mark(kind);
                                        let kind = SharedString::from(kind.to_string());
                                        div()
                                            .id(("decl", ix))
                                            .w_full()
                                            .pl(px(12. + 14. * item.depth as f32))
                                            .pr_2()
                                            .py_0p5()
                                            .text_sm()
                                            .truncate()
                                            .cursor_pointer()
                                            .when(Some(ix) == current, |d| d.bg(cx.theme().accent))
                                            .when(keyboard && ix == this.side_selected, |d| d.border_l_2().border_color(cx.theme().primary))
                                            .hover(|d| d.bg(cx.theme().accent))
                                            .when(!kind.is_empty(), |d| {
                                                let kind = kind.clone();
                                                d.tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(kind.clone()).build(window, cx))
                                            })
                                            .child(
                                                h_flex()
                                                    .gap_1p5()
                                                    .min_w_0()
                                                    .child(div().w(px(12.)).flex_none().text_xs().font_semibold().text_color(color).child(mark))
                                                    .child(div().min_w_0().truncate().child(name.to_string())),
                                            )
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                if let Some(ix) = this.tabs.iter().position(|t| t.id == id) {
                                                    this.activate(ix, window, cx);
                                                }
                                                file.update(cx, |f, cx| f.go_to_line(line, 0, window, cx));
                                            }))
                                    })
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .size_full()
                        .track_scroll(&self.structure_scroll),
                    )
                    .child(gpui_kit::component::scroll::Scrollbar::vertical(&self.structure_scroll)),
            )
            .into_any_element()
    }
}

/// A declaration's kind as a letter and a color, as IDEs mark them.
fn kind_mark(kind: &str) -> (&'static str, Hsla) {
    let has = |words: &[&str]| words.iter().any(|w| kind.contains(w));
    let (mark, hue) = if has(&["method"]) {
        ("m", 0.58)
    } else if has(&["function", "fn"]) {
        ("f", 0.58)
    } else if has(&["interface", "trait", "protocol"]) {
        ("I", 0.38)
    } else if has(&["enum"]) {
        ("E", 0.78)
    } else if has(&["impl"]) {
        ("i", 0.78)
    } else if has(&["class", "struct", "object", "record"]) {
        ("C", 0.08)
    } else if has(&["type"]) {
        ("T", 0.48)
    } else if has(&["module", "namespace", "package", "mod"]) {
        ("M", 0.12)
    } else if has(&["field", "property", "const", "static", "var", "let"]) {
        ("v", 0.15)
    } else {
        return ("·", hsla(0., 0., 0.5, 1.));
    };
    (mark, hsla(hue, 0.6, 0.5, 1.))
}
