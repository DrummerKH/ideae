//! The completion menu and the prediction's ghost text in one editor: which
//! one shows, which one a key acts on, and what an answer that comes late
//! does (docs/features.md, "Completion and edit prediction").

use std::{cell::RefCell, ops::Range, rc::Rc, time::Duration};

use gpui_kit::component::input::{CompletionProvider, EditPrediction, Editor, EditorState, Rope, ShowCompletions};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Entity, Focusable as _, IntoElement, ParentElement as _, Point, Render, Styled as _, Task,
    TestAppContext, Window, WindowBounds, WindowOptions, div, px, size,
};
use lsp_types::{CompletionContext, CompletionItem, CompletionResponse, InlineCompletionContext, InlineCompletionItem, InsertTextFormat};

type Answer<T> = async_channel::Sender<T>;

/// A language server and a model that answer when the test says so.
#[derive(Default)]
struct Fake {
    menus: RefCell<Vec<Answer<Vec<CompletionItem>>>>,
    ghosts: RefCell<Vec<Answer<Option<EditPrediction>>>>,
    /// Letters ask for the menu, as with "complete as you type"; else only `.`.
    letters: std::cell::Cell<bool>,
    inserted: RefCell<Vec<(String, Range<usize>, String)>>,
}

struct Provider(Rc<Fake>);

impl CompletionProvider for Provider {
    fn completions(&self, _: &Rope, _: usize, _: CompletionContext, _: &mut Window, cx: &mut App) -> Task<anyhow::Result<CompletionResponse>> {
        let (tx, rx) = async_channel::bounded(1);
        self.0.menus.borrow_mut().push(tx);
        cx.background_spawn(async move { Ok(CompletionResponse::Array(rx.recv().await.unwrap_or_default())) })
    }

    fn edit_prediction(&self, _: &Rope, _: usize, _: InlineCompletionContext, _: &mut Window, cx: &mut App) -> Task<anyhow::Result<Option<EditPrediction>>> {
        let (tx, rx) = async_channel::bounded(1);
        self.0.ghosts.borrow_mut().push(tx);
        cx.background_spawn(async move { Ok(rx.recv().await.ok().flatten()) })
    }

    fn inline_completion_debounce(&self) -> Duration {
        Duration::from_millis(150)
    }

    fn completion_inserted(&self, item: &CompletionItem, _: &Rope, range: Range<usize>, after: &Rope, _: &mut Window, _: &mut App) {
        self.0.inserted.borrow_mut().push((item.label.clone(), range, after.to_string()));
    }

    fn is_completion_trigger(&self, _: usize, new_text: &str, _: &mut App) -> bool {
        new_text.ends_with('.') || (self.0.letters.get() && new_text.chars().last().is_some_and(char::is_alphanumeric))
    }
}

struct Host(Entity<EditorState>);

impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut gpui_kit::Context<Self>) -> impl IntoElement {
        div().size_full().child(Editor::new(&self.0).h_full())
    }
}

struct Test {
    handle: AnyWindowHandle,
    editor: Entity<EditorState>,
    fake: Rc<Fake>,
}

impl Test {
    fn new(cx: &mut TestAppContext, text: &str) -> Self {
        cx.update(gpui_kit::init);
        let mut editor = None;
        let handle = cx.update(|cx| {
            let bounds = Bounds { origin: Point::default(), size: size(px(800.), px(400.)) };
            let options = WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() };
            gpui_kit::open_window(options, cx, |window, cx| {
                let state = cx.new(|cx| EditorState::new(window, cx).language("rust"));
                editor = Some(state.clone());
                cx.new(|_| Host(state))
            })
            .expect("a test window")
            .0
        });
        let editor = editor.unwrap();
        let fake = Rc::new(Fake::default());
        let test = Test { handle, editor: editor.clone(), fake: fake.clone() };
        test.with(cx, |window, cx| {
            editor.update(cx, |state, cx| {
                state.set_value(text.to_string(), window, cx);
                state.set_selected_range(text.len()..text.len(), cx);
                state.lsp_mut().completion_provider = Some(Rc::new(Provider(fake)));
            });
            editor.read(cx).focus_handle(cx).focus(window, cx);
        });
        test
    }

    fn with<R>(&self, cx: &mut TestAppContext, f: impl FnOnce(&mut Window, &mut App) -> R) -> R {
        let out = cx.update_window(self.handle, |_, window, cx| f(window, cx)).unwrap();
        self.settle(cx, 0);
        out
    }

    /// Time passes (a debounce), and what is ready runs and is drawn.
    fn settle(&self, cx: &mut TestAppContext, ms: u64) {
        cx.run_until_parked();
        if ms > 0 {
            cx.executor().advance_clock(Duration::from_millis(ms));
            cx.run_until_parked();
        }
        cx.update_window(self.handle, |_, window, cx| window.render_frame(cx)).unwrap();
        cx.run_until_parked();
    }

    fn press(&self, cx: &mut TestAppContext, key: &str) {
        self.with(cx, |window, cx| window.press(key, cx));
    }

    fn input(&self, cx: &mut TestAppContext, text: &str) {
        self.with(cx, |window, cx| window.input(text, cx));
    }

    fn ctrl_space(&self, cx: &mut TestAppContext) {
        self.with(cx, |window, cx| window.dispatch_action(Box::new(ShowCompletions), cx));
    }

    fn answer_menu(&self, cx: &mut TestAppContext, labels: &[&str]) {
        self.answer_menu_with(cx, labels.iter().map(|label| CompletionItem { label: label.to_string(), ..Default::default() }).collect());
    }

    fn answer_menu_with(&self, cx: &mut TestAppContext, items: Vec<CompletionItem>) {
        let tx = self.fake.menus.borrow().last().cloned().expect("the menu was asked for");
        let _ = tx.try_send(items);
        self.settle(cx, 0);
    }

    fn answer_ghost(&self, cx: &mut TestAppContext, index: usize, text: &str) {
        let item = InlineCompletionItem { insert_text: text.to_string(), filter_text: None, range: None, command: None, insert_text_format: None };
        self.answer_prediction(cx, index, EditPrediction::ghost(item));
    }

    fn answer_prediction(&self, cx: &mut TestAppContext, index: usize, prediction: EditPrediction) {
        let tx = self.fake.ghosts.borrow().get(index).cloned().expect("a prediction was asked for");
        let _ = tx.try_send(Some(prediction));
        self.settle(cx, 0);
    }

    /// The model rewrites the whole text as `new`: what the editor is given.
    fn answer_model(&self, cx: &mut TestAppContext, index: usize, new: &str) {
        let (text, cursor) = cx.read(|cx| (self.editor.read(cx).text().clone(), self.editor.read(cx).cursor()));
        let answer = super::prompt::Answer { range: 0..text.len(), new: new.to_string(), caret: None };
        let prediction = super::prompt::prediction(&text, cursor, answer).expect("a change");
        self.answer_prediction(cx, index, super::shown(&text, cursor, prediction));
    }

    /// A space, the pause, and the model's whole new text.
    fn model_shown(&self, cx: &mut TestAppContext, new: &str) {
        let asked = self.asked().1;
        self.input(cx, " ");
        self.settle(cx, 200);
        assert_eq!(self.asked().1, asked + 1, "a prediction is asked after the pause");
        self.answer_model(cx, asked, new);
    }

    fn cursor(&self, cx: &mut TestAppContext) -> usize {
        cx.read(|cx| self.editor.read(cx).cursor())
    }

    /// The edits shown elsewhere, and where the first Tab goes.
    fn shown(&self, cx: &mut TestAppContext) -> Option<(Vec<(Range<usize>, String)>, Option<usize>)> {
        cx.read(|cx| self.editor.read(cx).edit_prediction_shown())
    }

    /// The prediction's inlays: offset and text.
    fn marks(&self, cx: &mut TestAppContext) -> Vec<(usize, String)> {
        cx.read(|cx| self.editor.read(cx).inlays().map(|i| (i.offset, i.text.to_string())).collect())
    }

    fn text(&self, cx: &mut TestAppContext) -> String {
        cx.read(|cx| self.editor.read(cx).text().to_string())
    }

    fn ghost(&self, cx: &mut TestAppContext) -> Option<String> {
        cx.read(|cx| self.editor.read(cx).inline_completion_text().map(str::to_string))
    }

    fn menu(&self, cx: &mut TestAppContext) -> bool {
        cx.read(|cx| self.editor.read(cx).completion_menu_state().open)
    }

    fn asked(&self) -> (usize, usize) {
        (self.fake.menus.borrow().len(), self.fake.ghosts.borrow().len())
    }

    /// A space, the pause, and the model's answer: the ghost alone.
    fn ghost_shown(&self, cx: &mut TestAppContext, text: &str) {
        let asked = self.asked().1;
        self.input(cx, " ");
        self.settle(cx, 200);
        assert_eq!(self.asked().1, asked + 1, "a prediction is asked after the pause");
        self.answer_ghost(cx, asked, text);
        assert_eq!(self.ghost(cx).as_deref(), Some(text));
        assert!(!self.menu(cx));
    }
}

#[gpui_kit::test]
fn with_only_a_prediction_tab_takes_it_esc_drops_it_enter_is_a_newline(cx: &mut TestAppContext) {
    let t = Test::new(cx, "let x =");
    t.ghost_shown(cx, "foo();");
    t.press(cx, "tab");
    assert_eq!(t.text(cx), "let x = foo();");
    assert_eq!(t.ghost(cx), None);

    t.ghost_shown(cx, "bar");
    t.press(cx, "escape");
    assert_eq!((t.text(cx), t.ghost(cx)), ("let x = foo(); ".to_string(), None));

    t.ghost_shown(cx, "baz");
    t.press(cx, "enter");
    assert_eq!((t.text(cx), t.ghost(cx)), ("let x = foo();  \n".to_string(), None));
    let asked = t.asked().1;
    t.settle(cx, 200);
    assert_eq!(t.asked().1, asked + 1, "the next line is predicted");
}

#[gpui_kit::test]
fn typing_what_the_ghost_shows_keeps_the_rest_and_asks_nothing(cx: &mut TestAppContext) {
    let t = Test::new(cx, "let x =");
    t.fake.letters.set(true);
    t.ghost_shown(cx, "foo();");
    let asked = t.asked();
    t.input(cx, "f");
    t.input(cx, "o");
    t.settle(cx, 400);
    assert_eq!(t.ghost(cx).as_deref(), Some("o();"));
    assert_eq!(t.asked(), asked, "no menu and no new prediction while the typing follows it");
    assert!(!t.menu(cx));
    t.press(cx, "tab");
    assert_eq!(t.text(cx), "let x = foo();");

    t.ghost_shown(cx, "bar");
    t.input(cx, "x");
    assert_eq!(t.ghost(cx), None, "typing something else drops it");
    assert_eq!(t.asked().0, asked.0 + 1, "and the menu is asked for the word");
}

#[gpui_kit::test]
fn with_the_menu_open_tab_and_enter_take_its_item_and_esc_closes_it(cx: &mut TestAppContext) {
    let t = Test::new(cx, "fn main() { al");
    for key in ["tab", "enter"] {
        t.ctrl_space(cx);
        t.answer_menu(cx, &["alpha", "alpine"]);
        assert!(t.menu(cx));
        t.press(cx, key);
        assert_eq!(t.text(cx), "fn main() { alpha", "{key} takes the first item in place of the word");
        assert!(!t.menu(cx));
        t.with(cx, |window, cx| {
            t.editor.update(cx, |state, cx| state.set_value("fn main() { al", window, cx));
            t.editor.update(cx, |state, cx| state.set_selected_range(14..14, cx));
        });
    }
    t.ctrl_space(cx);
    t.answer_menu(cx, &["alpha"]);
    t.press(cx, "escape");
    assert!(!t.menu(cx));
    assert_eq!(t.text(cx), "fn main() { al");
    t.settle(cx, 400);
    assert_eq!(t.asked().1, 0, "after Esc nothing is predicted until the next edit");
}

#[gpui_kit::test]
fn ctrl_space_drops_the_ghost_and_opens_only_the_menu(cx: &mut TestAppContext) {
    let t = Test::new(cx, "let v = s");
    t.ghost_shown(cx, "vec![]");
    t.ctrl_space(cx);
    assert_eq!(t.ghost(cx), None);
    t.answer_menu(cx, &["some", "self"]);
    assert!(t.menu(cx));
    t.settle(cx, 400);
    assert_eq!(t.asked().1, 1, "no prediction is asked while the menu is open");
    t.press(cx, "tab");
    assert_eq!(t.text(cx), "let v = s some");
}

#[gpui_kit::test]
fn a_prediction_waits_for_the_menu_and_comes_only_when_it_has_nothing(cx: &mut TestAppContext) {
    let t = Test::new(cx, "p");
    t.fake.letters.set(true);
    t.input(cx, ".");
    t.settle(cx, 400);
    assert_eq!(t.asked(), (1, 0), "the menu is being asked: no prediction meanwhile");
    t.answer_menu(cx, &["x", "y"]);
    assert!(t.menu(cx));
    t.settle(cx, 400);
    assert_eq!(t.asked(), (1, 0));
    // The typed word empties the menu: now a prediction may come.
    t.input(cx, "q");
    t.answer_menu(cx, &[]);
    assert!(!t.menu(cx));
    t.settle(cx, 400);
    assert_eq!(t.asked(), (2, 1));
    t.answer_ghost(cx, 0, "uit()");
    assert_eq!(t.ghost(cx).as_deref(), Some("uit()"));
}

#[gpui_kit::test]
fn a_late_prediction_never_shows_over_the_menu_nor_after_an_edit(cx: &mut TestAppContext) {
    let t = Test::new(cx, "let a =");
    t.input(cx, " ");
    t.settle(cx, 200);
    assert_eq!(t.asked(), (0, 1), "a prediction is on its way");
    t.ctrl_space(cx);
    t.answer_menu(cx, &["one"]);
    t.answer_ghost(cx, 0, "late");
    assert_eq!(t.ghost(cx), None);
    assert!(t.menu(cx));
    t.press(cx, "escape");

    t.input(cx, " ");
    t.settle(cx, 200);
    t.input(cx, "1");
    t.answer_ghost(cx, 1, "for the text before");
    assert_eq!(t.ghost(cx), None, "an answer about a text changed since is dropped");

    // The menu comes back after the ghost: the ghost goes.
    let asked = t.asked().1;
    t.input(cx, " ");
    t.settle(cx, 200);
    t.answer_ghost(cx, asked, "two");
    assert_eq!(t.ghost(cx).as_deref(), Some("two"));
    t.input(cx, ".");
    t.answer_menu(cx, &["len"]);
    assert!(t.menu(cx));
    assert_eq!(t.ghost(cx), None);
}

#[gpui_kit::test]
fn an_item_goes_in_as_its_edit_or_snippet_and_the_server_is_told(cx: &mut TestAppContext) {
    let t = Test::new(cx, "v.");
    t.ctrl_space(cx);
    let snippet = CompletionItem {
        label: "push".into(),
        insert_text: Some("push(${1:value})$0".into()),
        insert_text_format: Some(InsertTextFormat::SNIPPET),
        ..Default::default()
    };
    t.answer_menu_with(cx, vec![snippet]);
    t.press(cx, "enter");
    assert_eq!(t.text(cx), "v.push(value)", "after the `.`, not over it");
    assert_eq!(cx.read(|cx| t.editor.read(cx).selected_range()), 7..12, "the placeholder is selected");
    assert_eq!(t.fake.inserted.borrow().as_slice(), &[("push".to_string(), 2..2, "v.push(value)".to_string())]);

    // An empty edit, the rest left to the server's command (kotlin-lsp).
    t.with(cx, |window, cx| {
        t.editor.update(cx, |state, cx| state.set_value("p.", window, cx));
        t.editor.update(cx, |state, cx| state.set_selected_range(2..2, cx));
    });
    t.ctrl_space(cx);
    let commanded = CompletionItem {
        label: "x".into(),
        text_edit: Some(lsp_types::CompletionTextEdit::Edit(lsp_types::TextEdit {
            range: lsp_types::Range::new(lsp_types::Position::new(0, 2), lsp_types::Position::new(0, 2)),
            new_text: String::new(),
        })),
        command: Some(lsp_types::Command { title: "Apply".into(), command: "apply".into(), arguments: None }),
        ..Default::default()
    };
    t.answer_menu_with(cx, vec![commanded]);
    t.press(cx, "tab");
    assert_eq!(t.fake.inserted.borrow().last().map(|(label, range, _)| (label.as_str(), range.clone())), Some(("x", 2..2)));
}

#[gpui_kit::test]
fn an_edit_elsewhere_on_the_line_shows_where_it_is_and_tab_makes_it(cx: &mut TestAppContext) {
    let t = Test::new(cx, "let c =");
    t.model_shown(cx, "const c = 3;");
    assert_eq!(t.ghost(cx), None, "a change before the cursor is no text typed at it");
    assert_eq!(t.shown(cx), Some((vec![(0..3, "const".to_string()), (8..8, "3;".to_string())], None)));
    assert_eq!(t.marks(cx), [(3, "const".to_string()), (8, "3;".to_string())], "the new text in green where it goes");
    let asked = t.asked().1;
    t.press(cx, "tab");
    assert_eq!(t.text(cx), "const c = 3;");
    assert_eq!(t.cursor(cx), 12, "the caret after the last change");
    assert_eq!((t.shown(cx), t.marks(cx)), (None, vec![]));
    t.settle(cx, 200);
    assert_eq!(t.asked().1, asked + 1, "the next is asked at once");
}

#[gpui_kit::test]
fn a_far_edit_is_gone_to_by_the_first_tab_and_made_by_the_second(cx: &mut TestAppContext) {
    let mut text = String::from("fn main() {\n    let total = 0;\n");
    for i in 0..30 {
        text += &format!("    step({i});\n");
    }
    text += "    report(total);\n}\n";
    let t = Test::new(cx, &text);
    let at = text.find("total").unwrap() + 5;
    t.with(cx, |_, cx| t.editor.update(cx, |state, cx| state.set_selected_range(at..at, cx)));
    t.input(cx, "s");
    t.settle(cx, 200);
    t.answer_model(cx, 0, &text.replace("total", "totals"));
    let far = text.rfind("total").unwrap() + 1 + 5;
    assert_eq!(t.shown(cx), Some((vec![(far..far, "s".to_string())], Some(far))));
    let eol = text.find('\n').unwrap() + 1 + "    let totals = 0;".len();
    assert!(t.marks(cx).contains(&(eol, "⇥ Tab: jump to line 33".to_string())), "{:?}", t.marks(cx));
    assert!(t.marks(cx).contains(&(far, "s".to_string())));

    t.press(cx, "tab");
    assert_eq!(t.cursor(cx), far, "the first Tab goes there");
    assert_eq!(t.text(cx), text.replacen("total", "totals", 1), "and changes nothing");
    assert_eq!(t.shown(cx), Some((vec![(far..far, "s".to_string())], None)));
    assert_eq!(t.marks(cx), [(far, "s".to_string())]);

    let asked = t.asked().1;
    t.press(cx, "tab");
    assert_eq!(t.text(cx), text.replace("total", "totals"), "the second makes it");
    assert_eq!(t.cursor(cx), far + 1);
    assert_eq!(t.shown(cx), None);
    t.settle(cx, 200);
    assert_eq!(t.asked().1, asked + 1, "and the next is asked");
}

#[gpui_kit::test]
fn lines_inserted_elsewhere_are_gone_to_and_shown_as_gray_text_there(cx: &mut TestAppContext) {
    let t = Test::new(cx, "fn a() {\n    b();\n}\n");
    t.with(cx, |_, cx| t.editor.update(cx, |state, cx| state.set_selected_range(8..8, cx)));
    t.model_shown(cx, "fn a() { \n    b();\n    c();\n}\n");
    assert_eq!(t.shown(cx), Some((vec![(18..18, "\n    c();".to_string())], Some(18))), "lines are not shown far from the caret");
    assert!(t.marks(cx).contains(&(9, "⇥ Tab: jump to line 2".to_string())), "{:?}", t.marks(cx));
    t.press(cx, "tab");
    assert_eq!(t.cursor(cx), 18);
    assert_eq!(t.ghost(cx).as_deref(), Some("\n    c();"));
    t.press(cx, "tab");
    assert_eq!(t.text(cx), "fn a() { \n    b();\n    c();\n}\n");
    assert_eq!(t.cursor(cx), 27);
}

#[gpui_kit::test]
fn typing_the_ghost_keeps_the_edits_elsewhere_and_tab_makes_all(cx: &mut TestAppContext) {
    let t = Test::new(cx, "let a = 1;\nlet c =");
    t.model_shown(cx, "let a = 10;\nlet c = 3;");
    assert_eq!(t.ghost(cx).as_deref(), Some("3;"));
    assert_eq!(t.shown(cx), Some((vec![(9..9, "0".to_string()), (19..19, "3;".to_string())], None)));
    assert_eq!(t.marks(cx), [(9, "0".to_string())], "the ghost's change is the gray text, not a mark");
    let asked = t.asked();
    t.input(cx, "3");
    t.settle(cx, 400);
    assert_eq!(t.ghost(cx).as_deref(), Some(";"));
    assert_eq!(t.shown(cx), Some((vec![(9..9, "0".to_string()), (20..20, ";".to_string())], None)));
    assert_eq!(t.asked(), asked, "nothing asked while the typing follows it");
    t.press(cx, "tab");
    assert_eq!(t.text(cx), "let a = 10;\nlet c = 3;");
    assert_eq!(t.cursor(cx), 22);
}

#[gpui_kit::test]
fn esc_typing_something_else_and_enter_drop_the_edits_elsewhere(cx: &mut TestAppContext) {
    let t = Test::new(cx, "let a = 1;\nlet c =");
    t.model_shown(cx, "let a = 2;\nlet c = 3;");
    assert!(t.shown(cx).is_some());
    t.press(cx, "escape");
    assert_eq!((t.shown(cx), t.ghost(cx), t.marks(cx)), (None, None, vec![]));
    assert_eq!(t.text(cx), "let a = 1;\nlet c = ");

    t.model_shown(cx, "let a = 2;\nlet c = 3;");
    assert!(t.shown(cx).is_some());
    t.input(cx, "x");
    assert_eq!((t.shown(cx), t.ghost(cx), t.marks(cx)), (None, None, vec![]), "an edit it does not foresee makes it stale");

    t.model_shown(cx, "let a = 2;\nlet c =  x 3;");
    assert!(t.shown(cx).is_some());
    t.press(cx, "enter");
    assert_eq!((t.shown(cx), t.marks(cx)), (None, vec![]));
    assert_eq!(t.text(cx), "let a = 1;\nlet c =  x \n");
}

#[gpui_kit::test]
fn the_menu_wins_over_the_edits_elsewhere(cx: &mut TestAppContext) {
    let t = Test::new(cx, "let a = 1;\nlet c = v");
    t.model_shown(cx, "let a = 2;\nlet c = v 3;");
    assert!(t.shown(cx).is_some());
    t.input(cx, ".");
    t.answer_menu(cx, &["len"]);
    assert!(t.menu(cx));
    assert_eq!((t.shown(cx), t.marks(cx)), (None, vec![]));
    t.press(cx, "tab");
    assert_eq!(t.text(cx), "let a = 1;\nlet c = v .len", "Tab takes the menu's item");

    // One asked before the menu came, answered while it is open: not shown.
    t.press(cx, "escape");
    t.input(cx, " ");
    t.settle(cx, 200);
    let asked = t.asked().1;
    t.ctrl_space(cx);
    t.answer_menu(cx, &["one"]);
    t.answer_model(cx, asked - 1, "let a = 2;\nlet c = v .len 3;");
    assert!(t.menu(cx));
    assert_eq!((t.shown(cx), t.marks(cx)), (None, vec![]));
}

#[gpui_kit::test]
fn the_gray_text_goes_in_first_then_tab_goes_to_the_far_edit(cx: &mut TestAppContext) {
    let mut text = String::from("fn main() {\n    let total =");
    let rest: String = (0..30).map(|i| format!("\n    step({i});")).collect::<String>() + "\n    report(total);\n}\n";
    text += &rest;
    let t = Test::new(cx, &text);
    let at = text.find(" =").unwrap() + 2;
    t.with(cx, |_, cx| t.editor.update(cx, |state, cx| state.set_selected_range(at..at, cx)));
    let typed = text.replacen(" =", " = ", 1);
    t.model_shown(cx, &typed.replacen(" = ", " = 0;", 1).replace("report(total)", "report(total, 0)"));
    assert_eq!(t.ghost(cx).as_deref(), Some("0;"));
    let far = typed.find("report(total").unwrap() + "report(total".len();
    assert_eq!(t.shown(cx).and_then(|s| s.1), Some(far));
    let jump_told = |t: &Test, cx: &mut TestAppContext| t.marks(cx).iter().any(|(_, mark)| mark.starts_with('⇥'));
    assert!(!jump_told(&t, cx), "the jump is told once the gray text is in");

    t.press(cx, "tab");
    let after = typed.replacen(" = ", " = 0;", 1);
    assert_eq!((t.text(cx), t.cursor(cx), t.ghost(cx)), (after.clone(), at + 3, None), "the gray text is in, the caret after it");
    assert_eq!(t.shown(cx), Some((vec![(far + 2..far + 2, ", 0".to_string())], Some(far + 2))));
    assert!(jump_told(&t, cx));
    t.press(cx, "tab");
    assert_eq!(t.cursor(cx), far + 2);
    t.press(cx, "tab");
    assert_eq!(t.text(cx), after.replace("report(total)", "report(total, 0)"));
}

#[test]
fn snippets_become_text_and_a_caret() {
    let at = gpui_kit::base::input::expand_snippet;
    assert_eq!(at("push(${1:value})$0"), ("push(value)".into(), 5..10));
    assert_eq!(at("foo($0)"), ("foo()".into(), 4..4));
    assert_eq!(at("plain"), ("plain".into(), 5..5));
    assert_eq!(at("if $2 {\n\t$1\n}"), ("if  {\n\t\n}".into(), 7..7));
    assert_eq!(at("${1:outer ${2:inner}}"), ("outer inner".into(), 0..11));
    assert_eq!(at("${1|one,two|} x"), ("one x".into(), 0..3));
    assert_eq!(at("a\\$1b\\}c$1"), ("a$1b}c".into(), 6..6));
    assert_eq!(at("${TM_FILENAME:main.rs} $HOME"), ("main.rs ".into(), 8..8));
    assert_eq!(at("cost $"), ("cost $".into(), 6..6));
    assert_eq!(at("你${1:好}"), ("你好".into(), 3..6));
}
