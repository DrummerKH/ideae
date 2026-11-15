//! Settings (Ctrl+,): a dialog over the state's settings, applied on Save.

use std::{cell::RefCell, rc::Rc};

use gpui_kit::component::{
    ActiveTheme as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    input::{Input, InputState},
    switch::Switch,
    h_flex, v_flex,
};
use gpui_kit::*;

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
        window.open_dialog(cx, move |dialog, _, _| {
            let (draft, agent, idle, this) = (draft.clone(), agent.clone(), idle.clone(), this.clone());
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
                    let toggle = |id: &'static str, on: bool, set: fn(&mut state::Settings, bool)| {
                        let draft = draft.clone();
                        Switch::new(id).checked(on).on_click(move |value: &bool, window, _| {
                            set(&mut draft.borrow_mut(), *value);
                            window.refresh();
                        })
                    };
                    let servers = lsp::SERVERS.iter().map(|&program| {
                        let draft = draft.clone();
                        let on = !d.lsp_off.iter().any(|p| p == program);
                        Checkbox::new(SharedString::from(format!("lsp-{program}")))
                            .label(program)
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
                        v_flex()
                            .gap_3()
                            .child(section("AGENT", cx))
                            .child(row(
                                "Agent command",
                                "Runs in each task's agent tab; empty for a plain shell. New tasks use it.",
                                div().w(px(220.)).child(Input::new(&agent).small()),
                                cx,
                            ))
                            .child(row(
                                "Notifications",
                                "A desktop notification when an agent out of view needs you or finishes.",
                                toggle("notifications", d.notifications, |s, v| s.notifications = v),
                                cx,
                            ))
                            .child(section("LANGUAGE SERVERS", cx))
                            .child(h_flex().flex_wrap().gap_x_4().gap_y_1().children(servers))
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
                                "Pull request status",
                                "Ask gh about each task's PR and show it on the row.",
                                toggle("pr-status", d.pr_status, |s, v| s.pr_status = v),
                                cx,
                            )),
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
