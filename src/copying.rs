//! Text that can be taken out of the app: what is there to read is selected
//! with the mouse (Ctrl+C), a row that is clicked has it in its right-click menu.

use gpui_kit::component::{ActiveTheme as _, Sizable as _, SelectableText, button::{Button, ButtonVariants as _}, h_flex, menu::PopupMenuItem};
use gpui_kit::{prelude::FluentBuilder as _, *};

/// A menu's item that puts `text` on the clipboard.
pub fn item(label: impl Into<SharedString>, text: impl Into<String>) -> PopupMenuItem {
    let text = text.into();
    PopupMenuItem::new(label).on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(text.clone())))
}

pub fn text(id: impl Into<ElementId>, text: impl Into<SharedString>) -> SelectableText {
    SelectableText::new(id, text)
}

/// What went wrong, all of it: selectable, scrolled when long, with a Copy.
pub fn message(id: &'static str, text: impl Into<SharedString>, color: Hsla, cx: &App) -> AnyElement {
    let text: SharedString = text.into();
    let whole = text.to_string();
    let long = text.lines().count() > 1 || text.len() > 80;
    h_flex()
        .w_full()
        .min_w_0()
        .gap_1()
        .items_start()
        .text_xs()
        .text_color(color)
        .child(div().id(id).flex_1().min_w_0().max_h(px(140.)).overflow_y_scroll().child(SelectableText::new(SharedString::from(format!("{id}-text")), text)))
        .when(long, |row| {
            row.child(
                Button::new(SharedString::from(format!("{id}-copy")))
                    .xsmall()
                    .ghost()
                    .text_color(cx.theme().muted_foreground)
                    .label("Copy")
                    .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(whole.clone()))),
            )
        })
        .into_any_element()
}
