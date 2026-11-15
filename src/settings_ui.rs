//! Settings (Ctrl+,): a dialog over the state's settings, applied on Save.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use gpui_kit::component::{
    ActiveTheme as _, Selectable as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    input::{Input, InputState},
    switch::Switch,
    h_flex, v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};

use crate::{lsp, state, workspace::Workspace};

fn section(title: &'static str, cx: &App) -> Div {
    div().pt_2().text_xs().text_color(cx.theme().muted_foreground).child(title)
}

fn row(label: &'static str, hint: &'static str, control: impl IntoElement, cx: &App) -> Div {
    h_flex()
        .gap_4()
        .justify_between()
        .child(
            v_flex()
                .min_w_0()
                .child(div().text_sm().child(label))
                .child(div().text_xs().text_color(cx.theme().muted_foreground).child(hint)),
        )
        .child(control)
}

impl Workspace {
    pub fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = state::settings();
        let draft = Rc::new(RefCell::new(current.clone()));
        let agent = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder("claude");
            input.set_value(current.agent.clone(), window, cx);
            input
        });
        let idle = cx.new(|cx| {
            let mut input = InputState::new(window, cx);
            input.set_value(current.lsp_idle_minutes.to_string(), window, cx);
            input
        });
        let this = cx.weak_entity();
        // The shortcut waiting for keys, and what catches them before any
        // binding does.
        let recording: Rc<Cell<Option<&'static str>>> = Rc::default();
        let catcher: Rc<RefCell<Option<Subscription>>> = Rc::default();
        window.open_dialog(cx, move |dialog, _, _| {
            let (draft, agent, idle, this) = (draft.clone(), agent.clone(), idle.clone(), this.clone());
            let (recording, catcher) = (recording.clone(), catcher.clone());
            let save = {
                let (draft, agent, idle, this) = (draft.clone(), agent.clone(), idle.clone(), this.clone());
                move |window: &mut Window, cx: &mut App| {
                    let mut settings = draft.borrow().clone();
                    settings.agent = agent.read(cx).value().trim().to_string();
                    if let Ok(minutes) = idle.read(cx).value().trim().parse::<u32>() {
                        settings.lsp_idle_minutes = minutes.max(1);
                    }
                    let _ = this.update(cx, |w, cx| w.apply_settings(settings, cx));
                    window.close_dialog(cx);
                }
            };
            dialog
                .title("Settings")
                .w(px(620.))
                .content(move |content, _, cx| {
                    let d = draft.borrow().clone();
                    let keys = keys_section(&draft, &recording, &catcher);
                    let toggle = |id: &'static str, on: bool, set: fn(&mut state::Settings, bool)| {
                        let draft = draft.clone();
                        Switch::new(id).checked(on).on_click(move |value: &bool, window, _| {
                            set(&mut draft.borrow_mut(), *value);
                            window.refresh();
                        })
                    };
                    let servers = lsp::SERVERS.iter().map(|def| {
                        let program = def.program;
                        let draft = draft.clone();
                        let on = !d.lsp_off.iter().any(|p| p == program);
                        let missing = if def.available() { "" } else { ", not installed" };
                        Checkbox::new(SharedString::from(format!("lsp-{program}")))
                            .label(format!("{program}  ({}{missing})", def.extensions()))
                            .checked(on)
                            .on_click(move |value: &bool, window, _| {
                                let mut d = draft.borrow_mut();
                                d.lsp_off.retain(|p| p != program);
                                if !*value {
                                    d.lsp_off.push(program.to_string());
                                }
                                window.refresh();
                            })
                    });
                    content.child(
                        div().id("settings-body").h(px(560.)).overflow_y_scroll().pr_2().child(v_flex()
                            .gap_3()
                            .child(section("AGENT", cx))
                            .child(row(
                                "Agent command",
                                "Runs in each task's agent tab; empty for a plain shell. New tasks use it.",
                                div().w(px(220.)).child(Input::new(&agent).small()),
                                cx,
                            ))
                            .child(row(
                                "Agents' language servers",
                                "Claude Code's LSP plugins, a rust-analyzer and the like per agent. Off saves much memory; agents lose go-to-definition. For agents started after Save.",
                                toggle("agent-lsp", d.agent_lsp, |s, v| s.agent_lsp = v),
                                cx,
                            ))
                            .child(row(
                                "Notifications",
                                "A desktop notification when an agent out of view needs you or finishes.",
                                toggle("notifications", d.notifications, |s, v| s.notifications = v),
                                cx,
                            ))
                            .child(section("LANGUAGE SERVERS", cx))
                            .child(v_flex().gap_1().children(servers))
                            .child(row(
                                "Sleep after (minutes)",
                                "A task out of view this long stops its language servers.",
                                div().w(px(80.)).child(Input::new(&idle).small()),
                                cx,
                            ))
                            .child(section("TASKS", cx))
                            .child(row(
                                "Copy .env files",
                                "New worktrees get the main checkout's ignored .env* files.",
                                toggle("copy-env", d.copy_env, |s, v| s.copy_env = v),
                                cx,
                            ))
                            .child(row(
                                "Check for updates",
                                "Release builds ask GitHub for a newer release now and then.",
                                toggle("check-updates", d.check_updates, |s, v| s.check_updates = v),
                                cx,
                            ))
                            .child(row(
                                "Pull request status",
                                "Ask gh about each task's PR and show it on the row.",
                                toggle("pr-status", d.pr_status, |s, v| s.pr_status = v),
                                cx,
                            ))
                            .child(section("KEYS", cx))
                            .child(keys)),
                    )
                })
                .footer({
                    let save = save.clone();
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(Button::new("settings-cancel").ghost().label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                        .child(Button::new("settings-save").primary().label("Save").on_click(move |_, window, cx| save(window, cx)))
                })
        });
    }
}

/// The shortcuts, each with its keys: a click waits for new ones (Esc
/// leaves them, Backspace removes them), ↺ puts the default back.
fn keys_section(
    draft: &Rc<RefCell<state::Settings>>,
    recording: &Rc<Cell<Option<&'static str>>>,
    catcher: &Rc<RefCell<Option<Subscription>>>,
) -> impl IntoElement {
    use crate::keymap::{SHORTCUTS, display};
    let d = draft.borrow().clone();
    let effective = |id: &str, default: String| d.keys.get(id).cloned().unwrap_or(default);
    let all: Vec<(&'static str, &'static str, String)> =
        SHORTCUTS.iter().map(|s| (s.id, s.name, effective(s.id, s.default_keys()))).collect();
    v_flex().gap_0p5().children(SHORTCUTS.iter().map(|shortcut| {
        let id = shortcut.id;
        let keys = effective(id, shortcut.default_keys());
        let waiting = recording.get() == Some(id);
        let clash: Vec<&str> =
            all.iter().filter(|(other, _, k)| *other != id && !keys.is_empty() && *k == keys).map(|(_, name, _)| *name).collect();
        let (draft, recording, catcher) = (draft.clone(), recording.clone(), catcher.clone());
        let reset_draft = draft.clone();
        let changed = d.keys.contains_key(id);
        h_flex()
            .gap_2()
            .child(div().flex_1().text_sm().child(shortcut.name))
            .when(!clash.is_empty(), |row| {
                row.child(div().text_xs().text_color(gpui_kit::red()).child(format!("also {}", clash.join(", "))))
            })
            .child(
                Button::new(SharedString::from(format!("key-{id}")))
                    .small()
                    .outline()
                    .w(px(150.))
                    .label(if waiting {
                        "Press keys… Esc".to_string()
                    } else if keys.is_empty() {
                        "none".to_string()
                    } else {
                        display(&keys)
                    })
                    .selected(waiting)
                    .on_click(move |_, window, cx| {
                        recording.set(Some(id));
                        let (draft, recording, own) = (draft.clone(), recording.clone(), catcher.clone());
                        let subscription = cx.intercept_keystrokes(move |event, window, cx| {
                            let Some(id) = recording.get() else { return };
                            let k = &event.keystroke;
                            if matches!(k.key.as_str(), "shift" | "control" | "alt" | "platform" | "function" | "") {
                                return;
                            }
                            cx.stop_propagation();
                            let plain = !k.modifiers.modified();
                            match k.key.as_str() {
                                "escape" if plain => {}
                                "backspace" | "delete" if plain => {
                                    draft.borrow_mut().keys.insert(id.to_string(), String::new());
                                }
                                _ => {
                                    let keys = k.unparse(cx.platform_info().os);
                                    let default = crate::keymap::find(id).map(|s| s.default_keys()).unwrap_or_default();
                                    let mut d = draft.borrow_mut();
                                    if keys == default {
                                        d.keys.remove(id);
                                    } else {
                                        d.keys.insert(id.to_string(), keys);
                                    }
                                }
                            }
                            recording.set(None);
                            let own = own.clone();
                            window.defer(cx, move |_, _| {
                                own.borrow_mut().take();
                            });
                            window.refresh();
                        });
                        *catcher.borrow_mut() = Some(subscription);
                        window.refresh();
                    }),
            )
            .child(
                Button::new(SharedString::from(format!("key-reset-{id}")))
                    .small()
                    .ghost()
                    .label("↺")
                    .tooltip("Back to the default")
                    .when(!changed, |b| b.invisible())
                    .on_click(move |_, window, _| {
                        reset_draft.borrow_mut().keys.remove(id);
                        window.refresh();
                    }),
            )
    }))
}
