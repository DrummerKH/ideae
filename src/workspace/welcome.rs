//! The first start's word on agents: those installed and not yet set up
//! are offered as profiles; with none at all, where to get one. Settings →
//! Agents → Find installed agents looks again.

use std::{cell::RefCell, rc::Rc};

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex,
    notification::Notification,
    v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};

use super::Workspace;
use crate::{
    agents::find::{self, Found, Welcome},
    state,
};

thread_local! {
    /// What Settings → Agents → Find installed agents found last.
    static FIND_SAID: RefCell<Option<String>> = const { RefCell::new(None) };
    /// The agents whose hooks were offered in this run.
    static HOOKS_OFFERED: RefCell<std::collections::BTreeSet<std::path::PathBuf>> = const { RefCell::new(std::collections::BTreeSet::new()) };
}

/// The names of agents, as one says them: "Codex and OpenCode".
fn names(found: &[Found]) -> String {
    let names: Vec<&str> = found.iter().map(|f| f.known.name).collect();
    match names.as_slice() {
        [] => String::new(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

impl Workspace {
    /// Once, soon after the first start: which agents are here. Not while
    /// presenting, nor when the command line chose the agent.
    pub(super) fn welcome_agents(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if state::with_settings(|s| s.agents_welcomed) || self.is_presenting() || self.agent_override.is_some() {
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            let found = cx.background_spawn(async move { find::find_installed() }).await;
            let _ = this.update_in(cx, |this, window, cx| {
                let (profiles, default) = state::with_settings(|s| (s.agents.clone(), s.default_agent.clone()));
                match find::welcome(&found, &profiles) {
                    Welcome::Nothing => this.agents_welcomed(cx),
                    Welcome::Offer(offers) => {
                        let default_missing = !find::default_found(&found, &profiles, &default);
                        this.offer_agents(offers, default_missing, window, cx)
                    }
                    Welcome::NoneFound => this.no_agent_found(window, cx),
                }
            });
        })
        .detach();
    }

    /// Agents that read hooks from their own files alone (Kimi Code,
    /// Antigravity), run in a terminal here: once a run, ideae's hooks
    /// are offered for their file, unless declined for good.
    pub(crate) fn offer_agent_hooks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use crate::agents::own_config;
        if self.is_presenting() {
            return;
        }
        let (profiles, declined) = state::with_settings(|s| (s.agents.clone(), s.agent_hooks_declined.clone()));
        let asked = HOOKS_OFFERED.with(|o| o.borrow().clone());
        // One offer for each file: two accounts of an agent have two.
        let mut candidates: Vec<(crate::agents::Profile, std::path::PathBuf)> = Vec::new();
        for p in profiles {
            let Some(file) = own_config::config_file(&p) else { continue };
            let key = own_config::shown(&file);
            if !declined.contains(&key) && !asked.contains(&file) && !candidates.iter().any(|(_, f)| *f == file) {
                candidates.push((p, file));
            }
        }
        if candidates.is_empty() {
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            let read: Vec<crate::agents::Profile> = candidates.iter().map(|(p, _)| p.clone()).collect();
            cx.background_spawn(async move { own_config::refresh(&read) }).await;
            let _ = this.update_in(cx, |this, window, cx| {
                for (profile, file) in candidates.into_iter().filter(|(p, _)| own_config::hooked(p) == Some(false)) {
                    if HOOKS_OFFERED.with(|o| o.borrow_mut().insert(file)) {
                        this.offer_hooks_for(profile, window, cx);
                    }
                }
            });
        })
        .detach();
    }

    fn offer_hooks_for(&mut self, profile: crate::agents::Profile, window: &mut Window, cx: &mut Context<Self>) {
        use crate::agents::own_config;
        let Some(file) = own_config::config_file(&profile) else { return };
        let name = profile.kind.label();
        let shown = own_config::shown(&file);
        let this = cx.weak_entity();
        let note = Notification::new().title(format!("{name} can tell ideae what it does")).autohide(false).content(move |_, _, cx| {
            let muted = cx.theme().muted_foreground;
            let (profile, off) = (profile.clone(), this.clone());
            let key = shown.clone();
            v_flex()
                .gap_2()
                .child(div().text_sm().text_color(muted).child(format!(
                    "It reads hooks from its own file alone. Add ideae's to {shown}? {} They do nothing outside ideae's tasks; the rest of the file stays as it is, and Settings → Agents takes them out.",
                    own_config::gives(profile.kind)
                )))
                .child(
                    h_flex()
                        .gap_2()
                        .child(Button::new("hooks-add").small().primary().label("Add").on_click(cx.listener(move |note, _, window, cx| {
                            let profile = profile.clone();
                            cx.spawn_in(window, async move |_, cx| {
                                let done = cx.background_spawn(async move { own_config::set(&profile, true) }).await;
                                let _ = cx.update(|window, cx| match done {
                                    Ok(()) => window.push_notification(Notification::success("Added: it tells ideae from the next start of its agent"), cx),
                                    Err(e) => window.push_notification(Notification::error(format!("Not added: {e}")), cx),
                                });
                            })
                            .detach();
                            note.dismiss(window, cx);
                        })))
                        .child(Button::new("hooks-later").small().ghost().label("Not now").on_click(cx.listener(|note, _, window, cx| note.dismiss(window, cx))))
                        .child(Button::new("hooks-off").small().ghost().label("Don't ask again").on_click(cx.listener(move |note, _, window, cx| {
                            let _ = off.update(cx, |w, cx| {
                                let mut settings = state::settings();
                                if !settings.agent_hooks_declined.contains(&key) {
                                    settings.agent_hooks_declined.push(key.clone());
                                }
                                w.apply_settings(settings, cx);
                            });
                            note.dismiss(window, cx);
                        }))),
                )
                .into_any_element()
        });
        window.push_notification(note, cx);
    }

    fn agents_welcomed(&mut self, cx: &mut Context<Self>) {
        let mut settings = state::settings();
        settings.agents_welcomed = true;
        self.apply_settings(settings, cx);
    }

    /// "Found Codex, OpenCode": each with a box, on; Add makes their
    /// profiles. The default changes only when asked, and is offered only
    /// when the default agent was not found.
    fn offer_agents(&mut self, offers: Vec<Found>, default_missing: bool, window: &mut Window, cx: &mut Context<Self>) {
        let chosen: Rc<RefCell<Vec<bool>>> = Rc::new(RefCell::new(vec![true; offers.len()]));
        let make_default = Rc::new(std::cell::Cell::new(false));
        let offers = Rc::new(offers);
        let this = cx.weak_entity();
        let title = format!("Found {}", names(&offers));
        let note = Notification::new().title(title).autohide(false).content(move |_, _, cx| {
            let muted = cx.theme().muted_foreground;
            let (add, off) = (this.clone(), this.clone());
            let mut boxes = v_flex().gap_1();
            for (ix, f) in offers.iter().enumerate() {
                let chosen = chosen.clone();
                let on = chosen.borrow()[ix];
                boxes = boxes.child(
                    Checkbox::new(("found-agent", ix))
                        .label(format!("{} · {}", f.known.name, f.shown()))
                        .checked(on)
                        .on_click(move |on, window, _| {
                            chosen.borrow_mut()[ix] = *on;
                            window.refresh();
                        }),
                );
            }
            let first = chosen.borrow().iter().position(|on| *on).map(|ix| offers[ix].known.name);
            let picked = {
                let (offers, chosen) = (offers.clone(), chosen.clone());
                move || offers.iter().zip(chosen.borrow().iter()).filter(|(_, on)| **on).map(|(f, _)| f.clone()).collect::<Vec<_>>()
            };
            let default_box = make_default.clone();
            let as_default = make_default.clone();
            v_flex()
                .gap_2()
                .child(div().text_sm().text_color(muted).child(if offers.len() == 1 {
                    "Add it as an agent? A new task can run it; Settings → Agents has it."
                } else {
                    "Add them as agents? A new task can run any of them; Settings → Agents has them."
                }))
                .child(boxes)
                .when(default_missing && first.is_some(), |s| {
                    s.child(Checkbox::new("found-default").label(format!("Make {} the default", first.unwrap_or_default())).checked(default_box.get()).on_click(move |on, window, _| {
                        default_box.set(*on);
                        window.refresh();
                    }))
                })
                .child(
                    h_flex()
                        .gap_2()
                        .child(Button::new("found-add").small().primary().label("Add").disabled(first.is_none()).on_click(cx.listener(move |note, _, window, cx| {
                            let picked = picked();
                            let make_default = as_default.get();
                            let _ = add.update(cx, |w, cx| {
                                let mut settings = state::settings();
                                let mut new_default = None;
                                for f in &picked {
                                    let profile = f.profile(&settings.agents);
                                    new_default.get_or_insert(profile.id.clone());
                                    settings.agents.push(profile);
                                }
                                if let Some(id) = new_default.filter(|_| make_default) {
                                    settings.default_agent = id;
                                }
                                settings.agents_welcomed = true;
                                w.apply_settings(settings, cx);
                                w.offer_agent_hooks(window, cx);
                            });
                            note.dismiss(window, cx);
                        })))
                        .child(Button::new("found-off").small().ghost().label("Don't show again").on_click(cx.listener(move |note, _, window, cx| {
                            let _ = off.update(cx, |w, cx| w.agents_welcomed(cx));
                            note.dismiss(window, cx);
                        }))),
                )
                .into_any_element()
        });
        window.push_notification(note, cx);
    }

    /// No agent installed: what a task's agent is, and where to get one.
    fn no_agent_found(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.weak_entity();
        let note = Notification::new().title("No coding agent found").autohide(false).content(move |_, _, cx| {
            let muted = cx.theme().muted_foreground;
            let (settings, off) = (this.clone(), this.clone());
            let links = find::KNOWN.iter().take(3).enumerate().map(|(ix, k)| {
                Button::new(("agent-install", ix)).small().outline().label(k.name).tooltip(k.url).on_click(move |_, _, cx| cx.open_url(k.url))
            });
            v_flex()
                .gap_2()
                .child(div().text_sm().text_color(muted).child(
                    "Each task runs an agent in a terminal next to its worktree. None of Claude Code, Codex, OpenCode or Gemini CLI was found, so tasks open with a shell for now. Install one:",
                ))
                .child(h_flex().gap_2().children(links))
                .child(div().text_sm().text_color(muted).child("Then Settings → Agents → Find installed agents adds it; any other command can be set there too."))
                .child(
                    h_flex()
                        .gap_2()
                        .child(Button::new("agents-settings").small().ghost().label("Settings → Agents").on_click(cx.listener(move |note, _, window, cx| {
                            let _ = settings.update(cx, |w, cx| w.open_settings_at(crate::settings_ui::AGENTS_PAGE, window, cx));
                            note.dismiss(window, cx);
                        })))
                        .child(Button::new("agents-off").small().ghost().label("Don't show again").on_click(cx.listener(move |note, _, window, cx| {
                            let _ = off.update(cx, |w, cx| w.agents_welcomed(cx));
                            note.dismiss(window, cx);
                        }))),
                )
                .into_any_element()
        });
        window.push_notification(note, cx);
    }
}

/// Settings → Agents → Find installed agents: those found that the draft
/// has no profile for are added to it, and it says which.
pub fn find_into(draft: Rc<RefCell<state::Settings>>, window: &mut Window, cx: &mut App) {
    FIND_SAID.with(|said| *said.borrow_mut() = Some("Looking…".into()));
    window.refresh();
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        let found = cx.background_spawn(async move { find::find_installed() }).await;
        let mut d = draft.borrow_mut();
        let added = find::not_added(&found, &d.agents);
        for f in &added {
            let profile = f.profile(&d.agents);
            d.agents.push(profile);
        }
        d.normalize();
        let said = match (found.is_empty(), added.is_empty()) {
            (true, _) => "None of Claude Code, Codex, OpenCode or Gemini CLI was found on PATH or where their installers put them.".to_string(),
            (false, true) => format!("Found {}: each has its profile already.", names(&found)),
            (false, false) => format!("Added {}. Save keeps them.", names(&added)),
        };
        drop(d);
        FIND_SAID.with(|s| *s.borrow_mut() = Some(said));
        let _ = handle.update(cx, |_, window, _| window.refresh());
    })
    .detach();
}

/// What the last look found, for the Agents page.
pub fn find_said() -> Option<String> {
    FIND_SAID.with(|said| said.borrow().clone())
}

/// A new Settings dialog starts with nothing said.
pub fn forget_find() {
    FIND_SAID.with(|said| *said.borrow_mut() = None);
}
