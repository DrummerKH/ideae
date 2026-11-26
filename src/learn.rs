//! Learn the keys (Settings → Keys), as JetBrains' Key Promoter: a command
//! that has keys, chosen with the pointer (a menu, the palette, a button),
//! shows its keys large at the bottom of the window for a moment, apart
//! from the notifications.

use std::cell::RefCell;
use std::collections::BTreeMap;

use gpui_kit::{Action, AnyElement, App, IntoElement as _, ParentElement as _, Styled as _, Window, div, prelude::FluentBuilder as _, px};
use gpui_kit::component::{ActiveTheme as _, h_flex};

use crate::{keymap, state};

/// How long the keys stay in view.
const SHOWN_FOR: std::time::Duration = std::time::Duration::from_millis(3000);

/// The keys in view: what they are, what they do, how often done without.
struct Shown {
    keys: String,
    name: &'static str,
    times: u32,
    /// Which showing it is: a later one is not taken away by an earlier one's end.
    turn: u64,
}

thread_local! {
    /// How often each shortcut was gone round, kept in the state.
    static TIMES: RefCell<BTreeMap<String, u32>> = RefCell::default();
    static SHOWN: RefCell<Option<Shown>> = const { RefCell::new(None) };
}

/// The counts of the last run, from the state.
pub fn restore(counts: BTreeMap<String, u32>) {
    TIMES.with_borrow_mut(|times| *times = counts);
}

/// The counts, for the state when it is written.
pub fn counts() -> BTreeMap<String, u32> {
    TIMES.with_borrow(Clone::clone)
}

/// Told by the menus of an action they sent: it is a shortcut's, or the
/// menu's `Unkeyed` that stands for one.
pub fn chosen(action: &dyn Action, window: &mut Window, cx: &mut App) {
    if !state::with_settings(|s| s.learn_keys) {
        return;
    }
    if let Some(id) = shortcut_of(action) {
        used(id, window, cx);
    }
}

fn shortcut_of(action: &dyn Action) -> Option<&'static str> {
    match action.as_any().downcast_ref::<crate::menu::Unkeyed>() {
        Some(unkeyed) => Some(unkeyed.0),
        None => keymap::SHORTCUTS.iter().find(|s| s.action().partial_eq(action)).map(|s| s.id),
    }
}

/// The shortcut `id` was done without its keys.
pub fn used(id: &'static str, window: &mut Window, cx: &mut App) {
    if !state::with_settings(|s| s.learn_keys) {
        return;
    }
    let Some(shortcut) = keymap::find(id) else { return };
    let keys = keymap::label(id);
    if keys.is_empty() {
        return;
    }
    let times = TIMES.with_borrow_mut(|times| {
        let n = times.entry(id.to_string()).or_default();
        *n += 1;
        *n
    });
    let turn = SHOWN.with_borrow(|shown| shown.as_ref().map_or(0, |s| s.turn + 1));
    SHOWN.set(Some(Shown { keys, name: shortcut.name, times, turn }));
    window.refresh();
    cx.spawn(async move |cx| {
        cx.background_executor().timer(SHOWN_FOR).await;
        if SHOWN.with_borrow(|shown| shown.as_ref().is_some_and(|s| s.turn == turn)) {
            SHOWN.set(None);
            cx.update(|cx| cx.refresh_windows());
        }
    })
    .detach();
}

/// A button's click that does what the shortcut `id` does: it does it, then
/// tells its keys.
pub fn clicked<E>(id: &'static str, then: impl Fn(&E, &mut Window, &mut App) + 'static) -> impl Fn(&E, &mut Window, &mut App) + 'static {
    move |event, window, cx| {
        then(event, window, cx);
        used(id, window, cx);
    }
}

/// The keys shown, large, at the bottom middle of the window; nothing when none.
pub fn overlay(cx: &App) -> Option<AnyElement> {
    SHOWN.with_borrow(|shown| {
        let shown = shown.as_ref()?;
        let theme = cx.theme();
        Some(
            div()
                .absolute()
                .bottom(px(64.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    h_flex()
                        .gap_4()
                        .px_6()
                        .py_3()
                        .rounded_lg()
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.popover)
                        .shadow_lg()
                        .child(div().text_size(px(26.)).font_weight(gpui_kit::FontWeight::SEMIBOLD).text_color(theme.foreground).child(shown.keys.clone()))
                        .child(div().text_size(px(17.)).text_color(theme.foreground).child(shown.name))
                        .when(shown.times > 1, |row| row.child(div().text_sm().text_color(theme.muted_foreground).child(format!("done {} times without them", shown.times)))),
                )
                .into_any_element(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::shortcut_of;
    use crate::{menu, task};

    #[test]
    fn a_menu_s_action_is_told_by_its_shortcut() {
        assert_eq!(shortcut_of(&menu::Git(task::GitCommand::Commit)), Some("commit"));
        assert_eq!(shortcut_of(&menu::Git(task::GitCommand::Push)), Some("push"));
        assert_eq!(shortcut_of(&menu::Git(task::GitCommand::Stash)), None);
        assert_eq!(shortcut_of(&menu::Unkeyed("back")), Some("back"));
        assert_eq!(shortcut_of(&task::ShowFileInFiles), Some("show_in_files"));
    }
}
