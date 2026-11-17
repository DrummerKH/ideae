//! The completion menu and the prediction's ghost text in one editor: which
//! one shows, which one a key acts on, and what an answer that comes late
//! does (docs/features.md, "Completion and edit prediction").

use std::{cell::RefCell, ops::Range, rc::Rc, time::Duration};

use gpui_kit::component::input::{CompletionProvider, Editor, EditorState, Rope, ShowCompletions};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Entity, Focusable as _, IntoElement, ParentElement as _, Point, Render, Styled as _, Task,
    TestAppContext, Window, WindowBounds, WindowOptions, div, px, size,
};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionResponse, InlineCompletionContext, InlineCompletionItem, InlineCompletionResponse,
    InsertTextFormat,
};

type Answer<T> = async_channel::Sender<T>;

/// A language server and a model that answer when the test says so.
#[derive(Default)]
struct Fake {
    menus: RefCell<Vec<Answer<Vec<CompletionItem>>>>,
    ghosts: RefCell<Vec<Answer<Option<String>>>>,
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

    fn inline_completion(&self, _: &Rope, _: usize, _: InlineCompletionContext, _: &mut Window, cx: &mut App) -> Task<anyhow::Result<InlineCompletionResponse>> {
        let (tx, rx) = async_channel::bounded(1);
        self.0.ghosts.borrow_mut().push(tx);
        cx.background_spawn(async move {
            let text = rx.recv().await.ok().flatten();
            let items = text.map(|insert_text| InlineCompletionItem { insert_text, filter_text: None, range: None, command: None, insert_text_format: None });
            Ok(InlineCompletionResponse::Array(items.into_iter().collect()))
        })
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
        let tx = self.fake.ghosts.borrow().get(index).cloned().expect("a prediction was asked for");
        let _ = tx.try_send(Some(text.to_string()));
        self.settle(cx, 0);
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
