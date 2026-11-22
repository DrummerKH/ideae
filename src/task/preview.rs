//! A file drawn as it is meant to be seen, in place of its text or beside
//! it: Markdown as it reads, an SVG as a picture, CSV and TSV as a table.
//! What is drawn follows the editor, unsaved edits too, once typing pauses.

use std::borrow::Cow;
use std::ops::Range;
use std::sync::LazyLock;

use gpui_kit::component::table::{Column, ColumnSort, DataTable, TableDelegate, TableState};

use super::*;

actions!(ideae_file, [CycleLook]);

/// The key context of a file drawn instead of its text, where its keys still work.
pub const PREVIEW_CONTEXT: &str = "FilePreview";

/// How a file that can be drawn is shown: its text, the drawing, or both side by side.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Look {
    #[default]
    Code,
    Split,
    Preview,
}

impl Look {
    /// As the session keeps it; the text alone is not kept.
    pub(super) fn name(self) -> Option<&'static str> {
        match self {
            Look::Code => None,
            Look::Split => Some("split"),
            Look::Preview => Some("preview"),
        }
    }

    pub(super) fn named(name: &str) -> Option<Look> {
        Some(match name {
            "code" => Look::Code,
            "split" => Look::Split,
            "preview" => Look::Preview,
            _ => return None,
        })
    }
}

/// What a file is drawn as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Markdown,
    Svg,
    /// A Mermaid diagram, drawn as an SVG is.
    Mermaid,
    /// Cells parted by this byte; None for a CSV, whose separator is guessed.
    Table(Option<u8>),
}

impl Kind {
    /// As Settings keeps how much of the room the text takes for it.
    fn name(self) -> &'static str {
        match self {
            Kind::Markdown => "markdown",
            Kind::Svg => "svg",
            Kind::Mermaid => "mermaid",
            Kind::Table(_) => "table",
        }
    }

    pub(super) fn of(path: &std::path::Path) -> Option<Kind> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "md" | "markdown" | "mdx" => Kind::Markdown,
            "svg" => Kind::Svg,
            "mmd" | "mermaid" => Kind::Mermaid,
            "csv" => Kind::Table(None),
            "tsv" | "tab" => Kind::Table(Some(b'\t')),
            _ => return None,
        })
    }
}

/// The line between a file's text and its drawing, dragged.
#[derive(Clone)]
pub(super) struct SplitDrag;

impl Render for SplitDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// Neither side narrower than this share of the room.
const LEAST_SHARE: f32 = 0.15;

/// The text's share of the room beside a drawing of this kind: as last dragged, else half.
pub(super) fn split_share(kind: Kind) -> f32 {
    crate::state::with_settings(|s| s.split_shares.get(kind.name()).copied()).unwrap_or(0.5).clamp(LEAST_SHARE, 1. - LEAST_SHARE)
}

/// The line to drag; a double click puts it back in the middle.
pub(super) fn split_handle(kind: Kind, cx: &App) -> impl IntoElement {
    div()
        .id("split-handle")
        .test_support()
        .flex_none()
        .w(px(5.))
        .h_full()
        .cursor_col_resize()
        .border_l_1()
        .border_color(cx.theme().border)
        .hover(|this| this.bg(cx.theme().drag_border.opacity(0.4)))
        .on_drag(SplitDrag, |drag, _, _, cx| cx.new(|_| drag.clone()))
        .on_click(move |event, window, _| {
            if event.click_count() == 2 {
                set_share(kind, 0.5);
                window.refresh();
            }
        })
}

/// How a file of this path opens, as last chosen for its kind.
pub(super) fn remembered_look(path: &std::path::Path) -> Option<Look> {
    let kind = Kind::of(path)?;
    crate::state::with_settings(|s| s.file_looks.get(kind.name()).and_then(|look| Look::named(look)))
}

fn remember_look(kind: Kind, look: Look) {
    let mut settings = crate::state::settings();
    settings.file_looks.insert(kind.name().to_string(), look.name().unwrap_or("code").to_string());
    crate::state::set_settings(settings);
}

fn set_share(kind: Kind, share: f32) {
    let mut settings = crate::state::settings();
    settings.split_shares.insert(kind.name().to_string(), share.clamp(LEAST_SHARE, 1. - LEAST_SHARE));
    crate::state::set_settings(settings);
}

impl FileView {
    /// The line followed while dragged: the share is kept for every file drawn as this one is.
    pub(super) fn drag_split(&mut self, event: &DragMoveEvent<SplitDrag>, cx: &mut Context<Self>) {
        let (Some(kind), bounds) = (self.preview_kind(), event.bounds) else { return };
        if bounds.size.width <= px(0.) {
            return;
        }
        set_share(kind, (event.event.position.x - bounds.left()) / bounds.size.width);
        cx.notify();
    }
}

/// A page the app does not draw itself, shown by the task's Chrome.
pub(super) fn is_page(path: &std::path::Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "html" | "htm" | "xhtml"))
}

pub(crate) struct Picture {
    pub image: Arc<RenderImage>,
    /// Its own size, as the file gives it.
    pub size: (f32, f32),
    /// The pixels it was drawn in.
    pub pixels: (u32, u32),
}

pub(super) enum Drawn {
    Markdown(SharedString),
    /// The last picture that could be drawn, and why the text now cannot be.
    Svg(Option<Arc<Picture>>, Option<SharedString>),
    Table(Arc<Sheet>),
}

pub(super) struct Preview {
    pub look: Look,
    /// What was drawn, from the text of this edit version.
    pub drawn: Option<(u64, Drawn)>,
    pub table: Option<Entity<TableState<SheetRows>>>,
    pub task: Task<()>,
    /// The drawing's, for the file's keys while its text is not shown.
    pub focus: Option<FocusHandle>,
    pub picture: Option<Entity<crate::picture::PictureView>>,
    /// Markdown's view, and the edit version of the text it was given.
    pub markdown: Option<(Entity<gpui_kit::component::text::TextViewState>, u64)>,
    /// The text's top line the drawing was last scrolled to, beside it.
    pub followed: Option<usize>,
    /// Where the text and the drawing were at the last frame, and the
    /// drawing's room: what moved since is what was scrolled.
    pub seen: Option<(Pixels, gpui_kit::ListOffset, Size<Pixels>)>,
    /// The side moved to follow the other: its move is not followed back.
    pub quiet: Option<Side>,
    /// The line the text goes to once it is laid out, back from the drawing alone.
    pub to_line: Option<usize>,
}

/// One side of Markdown shown beside its text.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Side {
    Text,
    Drawing,
}

impl Default for Preview {
    fn default() -> Self {
        Preview { look: Look::Code, drawn: None, table: None, task: Task::ready(()), focus: None, picture: None, markdown: None, followed: None, seen: None, quiet: None, to_line: None }
    }
}

impl FileView {
    pub(super) fn preview_kind(&self) -> Option<Kind> {
        Kind::of(&self.path)
    }

    /// Asked for: the text, when it is shown, has the keyboard.
    pub(super) fn pick_look(&mut self, look: Look, window: &mut Window, cx: &mut Context<Self>) {
        self.set_look(look, cx);
        if let Some(kind) = self.preview_kind() {
            remember_look(kind, look);
        }
        self.focus(window, cx);
    }

    /// The keyboard to the file: its text, or, drawn alone, the drawing (a picture's zoom keys).
    pub(super) fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.preview.look != Look::Preview || self.preview_kind().is_none() {
            return self.editor.read(cx).focus_handle(cx).focus(window, cx);
        }
        match &self.preview.picture {
            Some(picture) => picture.read(cx).focus_handle().clone().focus(window, cx),
            None => self.preview.focus.get_or_insert_with(|| cx.focus_handle()).focus(window, cx),
        }
    }

    /// Code, then Split, then Preview, for a file that can be drawn.
    pub(super) fn cycle_look(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.preview_kind().is_none() {
            return;
        }
        let next = match self.preview.look {
            Look::Code => Look::Split,
            Look::Split => Look::Preview,
            Look::Preview => Look::Code,
        };
        self.pick_look(next, window, cx);
    }

    pub(super) fn set_look(&mut self, look: Look, cx: &mut Context<Self>) {
        if self.preview.look == look {
            return;
        }
        // Back from the drawing alone, the text is where the reading got to.
        if self.preview.look == Look::Preview {
            self.preview.to_line = self.line_at_drawing(cx);
            self.preview.followed = self.preview.to_line;
        }
        (self.preview.seen, self.preview.quiet) = (None, None);
        self.preview.look = look;
        if look == Look::Code {
            // A picture can be large; it is drawn again when asked for.
            self.preview.drawn = None;
            self.preview.table = None;
            self.preview.picture = None;
            self.preview.markdown = None;
            self.preview.followed = None;
            self.preview.task = Task::ready(());
        } else {
            self.refresh_preview(false, cx);
        }
        cx.notify();
    }

    /// Draws the text again, after `typing` once it pauses: it is a copy
    /// of the whole file, and a parse.
    pub(super) fn refresh_preview(&mut self, typing: bool, cx: &mut Context<Self>) {
        let Some(kind) = self.preview_kind().filter(|_| self.preview.look != Look::Code) else { return };
        let dir = self.path.parent().map(PathBuf::from).unwrap_or_default();
        let colors = DiagramColors::of(cx);
        self.preview.task = cx.spawn(async move |this, cx| {
            if typing {
                cx.background_executor().timer(Duration::from_millis(300)).await;
            }
            let Ok((version, text, last)) = this.update(cx, |this, cx| {
                let last = match &this.preview.drawn {
                    Some((_, Drawn::Svg(picture, _))) => picture.clone(),
                    _ => None,
                };
                (this.edit_version, this.editor.read(cx).value(), last)
            }) else {
                return;
            };
            let drawn = cx
                .background_spawn(async move {
                    match kind {
                        Kind::Markdown => Drawn::Markdown(text),
                        Kind::Svg | Kind::Mermaid => {
                            let svg = if kind == Kind::Mermaid { mermaid_svg(&text, &colors).map(Cow::Owned) } else { Ok(Cow::Borrowed(text.as_ref())) };
                            match svg.and_then(|svg| rasterize(&svg, &dir, PREVIEW_PIXELS, Some(&colors.text))) {
                                Ok(picture) => Drawn::Svg(Some(Arc::new(picture)), None),
                                Err(why) => Drawn::Svg(last, Some(why.into())),
                            }
                        }
                        Kind::Table(separator) => Drawn::Table(Arc::new(Sheet::parse(text, separator))),
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.preview.drawn = Some((version, drawn));
                cx.notify();
            });
        });
    }

    /// Code, Split, Preview, in the file's header.
    pub(super) fn render_look_switch(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        use gpui_kit::component::button::ButtonGroup;
        self.preview_kind()?;
        let look = self.preview.look;
        let looks = [(Look::Code, "Code", "The text only"), (Look::Split, "Split", "The text and how it is drawn, side by side"), (Look::Preview, "Preview", "How it is drawn only")];
        Some(
            ButtonGroup::new("look")
                .xsmall()
                .ghost()
                .children(looks.iter().map(|&(each, label, tip)| {
                    Button::new(SharedString::from(format!("look-{}", label.to_ascii_lowercase())))
                        .label(label)
                        .tooltip(format!("{tip} ({} goes through them)", crate::keymap::label("cycle_look")))
                        .selected(look == each)
                }))
                .on_click(cx.listener(move |this, clicked: &Vec<usize>, window, cx| {
                    if let Some(&ix) = clicked.first() {
                        this.pick_look(looks[ix.min(2)].0, window, cx);
                    }
                }))
                .into_any_element(),
        )
    }

    /// Opens an HTML file in a Web tab of the task.
    pub(super) fn render_page_button(&self) -> Option<AnyElement> {
        if !is_page(&self.path) {
            return None;
        }
        let (task, url) = (self.task.clone(), crate::lsp::path_to_uri(&self.path));
        Some(
            Button::new("open-page")
                .xsmall()
                .ghost()
                .label("Open in Web tab")
                .tooltip("The page as the browser shows it, as saved, in a new tab")
                .on_click(move |_, window, cx| {
                    let _ = task.update(cx, |t, cx| t.open_browser(&url, window, cx));
                })
                .into_any_element(),
        )
    }

    pub(super) fn render_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let body = match self.preview.drawn.as_ref().map(|(_, drawn)| drawn) {
            None => div().into_any_element(),
            Some(Drawn::Markdown(text)) => {
                let (text, version) = (text.clone(), self.preview.drawn.as_ref().map_or(0, |(v, _)| *v));
                self.render_markdown(text, version, cx).into_any_element()
            }
            Some(Drawn::Svg(picture, why)) => {
                let (picture, why) = (picture.clone(), why.clone());
                self.render_picture(picture, why, window, cx)
            }
            Some(Drawn::Table(sheet)) => {
                let sheet = sheet.clone();
                self.render_sheet(sheet, window, cx)
            }
        };
        let focus = self.preview.focus.get_or_insert_with(|| cx.focus_handle()).clone();
        div()
            .id("file-preview")
            .test_support()
            .relative()
            .key_context(PREVIEW_CONTEXT)
            .track_focus(&focus)
            .on_mouse_down(gpui_kit::MouseButton::Left, move |_, window, cx| {
                if !focus.contains_focused(window, cx) {
                    focus.focus(window, cx);
                }
            })
            .flex_1().min_w_0().min_h_0().bg(cx.theme().background).child(body).into_any_element()
    }

    /// A link to another file of the worktree opens it here.
    fn render_markdown(&mut self, text: SharedString, version: u64, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui_kit::component::text::{TextView, TextViewState};
        let state = match self.preview.markdown.clone() {
            Some((state, shown)) => {
                if shown != version {
                    state.update(cx, |s, cx| s.set_text(&text, cx));
                    self.preview.quiet = Some(Side::Drawing);
                    self.preview.markdown = Some((state.clone(), version));
                }
                state.clone()
            }
            None => {
                let state = cx.new(|cx| TextViewState::markdown(&text, cx).scrollable(true));
                self.preview.markdown = Some((state.clone(), version));
                // Starts where the text is.
                self.preview.followed = None;
                let this = cx.weak_entity();
                cx.defer(move |cx| {
                    let _ = this.update(cx, |this, cx| this.follow_scroll(cx));
                });
                state
            }
        };
        let (task, dir) = (self.task.clone(), self.path.parent().map(PathBuf::from).unwrap_or_default());
        let images = dir.clone();
        TextView::new(&state)
            .image_source(move |uri| preview_image(&images, uri))
            .markdown_block_parser(|node, _| match node {
                gpui_kit::component::text::markdown_ast::Node::Code(code) if code.lang.as_deref().is_some_and(|lang| lang.eq_ignore_ascii_case("mermaid")) => {
                    Some(gpui_kit::component::text::MarkdownNode::new("mermaid", ()).text(code.value.clone()))
                }
                _ => None,
            })
            .markdown_block_renderer("mermaid", |node, _, cx| diagram(node.as_text(), cx))
            .scrollable(true)
            .selectable(true)
            .on_link_click(move |link, _, window, cx| {
                let (target, _) = link.split_once('#').unwrap_or((link.as_ref(), ""));
                let file = dir.join(target);
                if !target.is_empty() && !target.contains("://") && file.is_file() {
                    let _ = task.update(cx, |t, cx| t.open_file(file, 0, window, cx));
                } else if link.contains("://") {
                    cx.open_url(link);
                }
            })
            .size_full()
            .px_6()
            .py_4()
    }

    fn render_sheet(&mut self, sheet: Arc<Sheet>, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match &self.preview.table {
            Some(table) if Arc::ptr_eq(&table.read(cx).delegate().sheet, &sheet) => {}
            Some(table) => table.update(cx, |table, cx| {
                table.delegate_mut().show(sheet.clone());
                table.refresh(cx);
            }),
            None => {
                let rows = SheetRows::new(sheet.clone());
                self.preview.table = Some(cx.new(|cx| TableState::new(rows, window, cx).col_movable(false)));
            }
        }
        let Some(table) = &self.preview.table else { return div().into_any_element() };
        let told = format!(
            "{} rows · {} columns{}{}",
            sheet.rows(),
            sheet.columns,
            if sheet.cut { " · the first only" } else { "" },
            match sheet.separator {
                b'\t' => "",
                b',' => "",
                b';' => " · parted by ;",
                b'|' => " · parted by |",
                _ => "",
            }
        );
        v_flex()
            .size_full()
            .child(div().id("sheet").test_support().flex_1().min_h_0().child(DataTable::new(table).stripe(true).bordered(false)))
            .child(div().px_3().py_1().text_xs().text_color(cx.theme().muted_foreground).border_t_1().border_color(cx.theme().border).child(told))
            .into_any_element()
    }
}

impl FileView {
    /// Watches the scroll of Markdown beside its text, frame by frame: a
    /// scrollbar's drag, the wheel and the keys all move it, and only some tell.
    pub(super) fn scroll_watch(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let watched = self.preview_kind() == Some(Kind::Markdown) && (self.preview.look == Look::Split || self.preview.to_line.is_some());
        watched.then(|| {
            let file = cx.weak_entity();
            canvas(
                |_, _, _| (),
                move |_, _, window, cx| {
                    let file = file.clone();
                    window.defer(cx, move |_, cx| {
                        let _ = file.update(cx, |f, cx| f.sync_scroll(cx));
                    });
                },
            )
            .absolute()
            .size_0()
            .into_any_element()
        })
    }

    /// The side that moved since the last frame leads, the other follows.
    fn sync_scroll(&mut self, cx: &mut Context<Self>) {
        if let Some(line) = self.preview.to_line {
            let Some(height) = self.editor.read(cx).line_height().filter(|h| *h > px(0.)) else { return };
            self.preview.to_line = None;
            self.editor.update(cx, |e, cx| {
                let x = e.scroll_offset().x;
                e.set_scroll_offset(point(x, -(height * line as f32)), cx);
            });
            return;
        }
        if self.preview.look != Look::Split {
            return;
        }
        let Some((state, _)) = self.preview.markdown.clone() else { return };
        let list = state.read(cx).list_state();
        let now = (self.editor.read(cx).scroll_offset().y, list.logical_scroll_top(), list.viewport_bounds().size);
        let seen = self.preview.seen.replace(now);
        let quiet = self.preview.quiet.take();
        // A resize moves the drawing's top at its end; nobody scrolled.
        let Some(seen) = seen.filter(|seen| seen.2 == now.2) else { return };
        if now.0 != seen.0 && quiet != Some(Side::Text) {
            self.follow_scroll(cx);
        } else if now.1 != seen.1 && quiet != Some(Side::Drawing) {
            self.lead_from_preview(cx);
        }
    }

    /// Markdown drawn beside its text follows it: the line at the text's top
    /// is at the drawing's top, as near as its blocks tell.
    pub(super) fn follow_scroll(&mut self, cx: &mut Context<Self>) {
        if self.preview.look == Look::Code || self.preview_kind() != Some(Kind::Markdown) {
            return;
        }
        let Some((state, _)) = self.preview.markdown.clone() else { return };
        let editor = self.editor.read(cx);
        // From the scroll itself, not the last layout's rows, which are a frame behind it.
        let Some(line_height) = editor.line_height().filter(|h| *h > px(0.)) else { return };
        let top = ((-editor.scroll_offset().y) / line_height).max(0.).floor() as usize;
        if self.preview.followed == Some(top) {
            return;
        }
        self.preview.followed = Some(top);
        let text = editor.text();
        let offset = text.line_start_offset(top.min(text.lines_len().saturating_sub(1)));
        let before = state.read(cx).list_state().logical_scroll_top();
        state.update(cx, |s, cx| s.scroll_to_source(offset, cx));
        if state.read(cx).list_state().logical_scroll_top() != before {
            self.preview.quiet = Some(Side::Drawing);
        }
    }

    /// The line of the text at the drawing's top.
    fn line_at_drawing(&self, cx: &App) -> Option<usize> {
        let (state, _) = self.preview.markdown.as_ref()?;
        let offset = state.read(cx).source_at_top()?;
        let text = self.editor.read(cx).text();
        Some(text.offset_to_point(offset.min(text.len())).row as usize)
    }

    /// The other way: the drawing scrolled by hand, the text follows it.
    fn lead_from_preview(&mut self, cx: &mut Context<Self>) {
        let Some(line) = self.line_at_drawing(cx) else { return };
        let Some(line_height) = self.editor.read(cx).line_height().filter(|h| *h > px(0.)) else { return };
        if self.preview.followed == Some(line) {
            return;
        }
        self.preview.followed = Some(line);
        self.preview.quiet = Some(Side::Text);
        self.editor.update(cx, |e, cx| {
            let x = e.scroll_offset().x;
            e.set_scroll_offset(point(x, -(line_height * line as f32)), cx);
        });
    }
}

/// A picture of a Markdown preview: a path is a file beside the one shown
/// (README's `docs/images/a.png`); one with a scheme is fetched.
pub(super) fn preview_image(dir: &std::path::Path, uri: &gpui_kit::SharedUri) -> gpui_kit::ImageSource {
    let link: &str = uri.as_ref();
    if link.contains("://") {
        return gpui_kit::ImageSource::from(uri.clone());
    }
    let path = link.split(['#', '?']).next().unwrap_or_default();
    gpui_kit::ImageSource::from(dir.join(path))
}

impl FileView {
    /// The picture in the viewer of pictures, zoomed and on a background as chosen; why the text does not draw, below.
    fn render_picture(&mut self, picture: Option<Arc<Picture>>, why: Option<SharedString>, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let view = match self.preview.picture.clone() {
            Some(view) => view,
            None => {
                let view = cx.new(crate::picture::PictureView::new);
                // Drawn alone, it takes the keyboard the drawing had while it was being drawn.
                if self.preview.focus.as_ref().is_some_and(|f| f.is_focused(window)) {
                    view.read(cx).focus_handle().clone().focus(window, cx);
                }
                self.preview.picture = Some(view.clone());
                view
            }
        };
        if let Some(p) = &picture {
            let caption = match self.preview_kind() {
                Some(Kind::Svg) => format!("SVG · {} × {}", round(p.size.0), round(p.size.1)),
                _ => "Mermaid".into(),
            };
            // Drawn at twice its size or more: fit, it grows to half its pixels, sharp on a screen of double density.
            let most = size(px(p.pixels.0 as f32 / 2.), px(p.pixels.1 as f32 / 2.));
            let (image, own) = (ImageSource::Render(p.image.clone()), size(px(p.size.0), px(p.size.1)));
            view.update(cx, |v, cx| v.show(p.image.id.0 as u64, image, Some(own), Some(most), caption.into(), cx));
        }
        v_flex()
            .size_full()
            .when(picture.is_some(), |this| this.child(div().id("svg-picture").test_support().flex_1().min_h_0().child(view)))
            .when_some(why, |this, why| this.child(div().px_3().py_1().text_xs().text_color(gpui_kit::red()).child(why)))
            .into_any_element()
    }
}

fn round(n: f32) -> String {
    if n.fract().abs() < 0.01 { format!("{n:.0}") } else { format!("{n:.1}") }
}

/// Fonts for the text of pictures, read once.
static FONTS: LazyLock<Arc<resvg::usvg::fontdb::Database>> = LazyLock::new(|| {
    let mut fonts = resvg::usvg::fontdb::Database::new();
    fonts.load_system_fonts();
    Arc::new(fonts)
});

/// Pixels a preview's picture is drawn in, on its longer side: twice its
/// own size, not less than this, so a small icon is not blurred when shown
/// larger, and not more than that, which costs 16 MB.
const PREVIEW_PIXELS: std::ops::RangeInclusive<f32> = 1024.0..=2048.0;

/// The SVG drawn, its pictures' paths taken from beside it, in `pixels` on its longer side.
/// `ink` is what `currentColor` is, unless the SVG says: an icon drawn in
/// it shows as it would in the app.
pub(crate) fn rasterize(text: &str, dir: &std::path::Path, pixels: std::ops::RangeInclusive<f32>, ink: Option<&str>) -> Result<Picture, String> {
    use resvg::{tiny_skia, usvg};
    let style_sheet = ink.filter(|_| !names_its_color(text)).map(|ink| format!("svg {{ color: {ink} }}"));
    let options = usvg::Options { resources_dir: Some(dir.to_path_buf()), fontdb: FONTS.clone(), style_sheet, ..Default::default() };
    let tree = usvg::Tree::from_str(text, &options).map_err(|e| e.to_string())?;
    let size = tree.size();
    let (w, h) = (size.width(), size.height());
    let longer = w.max(h);
    if longer <= 0. {
        return Err("it has no size".into());
    }
    let scale = (longer * 2.).clamp(*pixels.start(), *pixels.end()) / longer;
    let pixels = (((w * scale).ceil() as u32).max(1), ((h * scale).ceil() as u32).max(1));
    let mut pixmap = tiny_skia::Pixmap::new(pixels.0, pixels.1).ok_or("it is too large to draw")?;
    resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    let mut bgra = pixmap.take();
    for pixel in bgra.chunks_exact_mut(4) {
        // tiny-skia's pixels are premultiplied RGBA; a frame's are plain BGRA.
        pixel.swap(0, 2);
        let a = pixel[3] as u32;
        if a > 0 && a < 255 {
            for c in &mut pixel[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    let frame = image::ImageBuffer::from_raw(pixels.0, pixels.1, bgra).ok_or("its pixels did not fit")?;
    Ok(Picture { image: Arc::new(RenderImage::new(vec![image::Frame::new(frame)])), size: (w, h), pixels })
}

/// The colors a diagram is drawn in: the app's theme's, where the diagram
/// names none of its own. Hex, as the SVG has them.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct DiagramColors {
    dark: bool,
    background: String,
    text: String,
    muted_text: String,
    border: String,
    fill: String,
    fill_alt: String,
    cluster: String,
    note: String,
    note_border: String,
    charts: [String; 6],
}

impl DiagramColors {
    pub(super) fn of(cx: &App) -> Self {
        let theme = cx.theme();
        let bg = theme.background;
        // A color with some see-through, as it shows on the background.
        let hex = |c: Hsla| {
            let (c, b) = (gpui_kit::Rgba::from(c), gpui_kit::Rgba::from(bg));
            let mix = |x: f32, y: f32| ((x * c.a + y * (1. - c.a)).clamp(0., 1.) * 255.).round() as u8;
            format!("#{:02x}{:02x}{:02x}", mix(c.r, b.r), mix(c.g, b.g), mix(c.b, b.b))
        };
        DiagramColors {
            dark: theme.mode.is_dark(),
            background: hex(bg),
            text: hex(theme.foreground),
            muted_text: hex(theme.muted_foreground),
            border: hex(theme.border),
            fill: hex(theme.secondary),
            fill_alt: hex(theme.muted),
            cluster: hex(theme.muted.opacity(0.5)),
            note: hex(theme.warning.opacity(0.18)),
            note_border: hex(theme.warning.opacity(0.6)),
            // Hues apart, toward the background: light text reads on them in a dark theme, dark in a light one.
            charts: [theme.blue, theme.green, theme.yellow, theme.magenta, theme.cyan, theme.red].map(|c| hex(c.opacity(0.55))),
        }
    }
}

/// The diagram colors of the theme in force, with their hash: made again
/// only when the theme's colors change, not for each diagram each frame.
#[derive(Default)]
struct ThemeColors {
    of: Option<([Hsla; 12], bool)>,
    colors: Option<(Arc<DiagramColors>, u64)>,
}

impl Global for ThemeColors {}

impl DiagramColors {
    fn current(cx: &mut App) -> (Arc<DiagramColors>, u64) {
        use std::hash::{Hash as _, Hasher as _};
        let t = cx.theme();
        let of = ([t.background, t.foreground, t.muted_foreground, t.border, t.secondary, t.muted, t.warning, t.blue, t.green, t.yellow, t.magenta, t.cyan], t.mode.is_dark());
        let kept = cx.default_global::<ThemeColors>();
        if kept.of.as_ref() == Some(&of) && let Some(colors) = &kept.colors {
            return colors.clone();
        }
        let colors = DiagramColors::of(cx);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        colors.hash(&mut hasher);
        let made = (Arc::new(colors), hasher.finish());
        let kept = cx.default_global::<ThemeColors>();
        (kept.of, kept.colors) = (Some(of), Some(made.clone()));
        made
    }
}

/// A Mermaid diagram as an SVG.
pub(super) fn mermaid_svg(source: &str, colors: &DiagramColors) -> Result<String, String> {
    use mermaid_rs_renderer::{RenderOptions, Theme};
    let mut options = RenderOptions::default();
    // Its git and label colors stay the preset's, of the same lightness.
    let mut theme = if colors.dark { Theme::dark() } else { Theme::modern() };
    let c = colors;
    theme.background = c.background.clone();
    theme.edge_label_background = c.background.clone();
    for text in [&mut theme.primary_text_color, &mut theme.text_color, &mut theme.pie_title_text_color, &mut theme.pie_section_text_color, &mut theme.pie_legend_text_color] {
        *text = c.text.clone();
    }
    theme.primary_color = c.fill.clone();
    theme.secondary_color = c.fill_alt.clone();
    theme.tertiary_color = c.background.clone();
    theme.primary_border_color = c.muted_text.clone();
    theme.line_color = c.muted_text.clone();
    theme.cluster_background = c.cluster.clone();
    theme.cluster_border = c.border.clone();
    theme.sequence_actor_fill = c.fill.clone();
    theme.sequence_actor_border = c.muted_text.clone();
    theme.sequence_actor_line = c.muted_text.clone();
    theme.sequence_note_fill = c.note.clone();
    theme.sequence_note_border = c.note_border.clone();
    theme.sequence_activation_fill = c.fill_alt.clone();
    theme.sequence_activation_border = c.muted_text.clone();
    theme.pie_stroke_color = c.background.clone();
    theme.pie_outer_stroke_color = c.border.clone();
    theme.pie_opacity = 1.;
    for (ix, slice) in theme.pie_colors.iter_mut().enumerate() {
        *slice = c.charts[ix % c.charts.len()].clone();
    }
    options.theme = theme;
    mermaid_rs_renderer::render_with_options(source, options).map_err(|e| e.to_string())
}

#[derive(Clone)]
enum Diagram {
    Drawing,
    Drawn(Arc<Picture>),
    Failed(SharedString),
}

/// Diagrams drawn, by their source and colors; bounded, as the pictures of
/// diffs are: past this many it starts again.
static DIAGRAMS: std::sync::Mutex<Option<HashMap<u64, Diagram>>> = std::sync::Mutex::new(None);
const MOST_DIAGRAMS: usize = 32;
/// A diagram's pixels on its longer side: twice its size, at most this.
const DIAGRAM_PIXELS: std::ops::RangeInclusive<f32> = 0.0..=2048.0;

/// A diagram of a Markdown preview: drawn off the UI thread once, then kept.
fn diagram(source: &str, cx: &mut App) -> AnyElement {
    use std::hash::{Hash as _, Hasher as _};
    let (colors, of_colors) = DiagramColors::current(cx);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (source, of_colors).hash(&mut hasher);
    let key = hasher.finish();
    let known = {
        let mut kept = DIAGRAMS.lock().unwrap_or_else(|e| e.into_inner());
        let kept = kept.get_or_insert_with(HashMap::default);
        let known = kept.get(&key).cloned();
        if known.is_none() {
            if kept.len() >= MOST_DIAGRAMS {
                kept.retain(|_, d| matches!(d, Diagram::Drawing));
            }
            kept.insert(key, Diagram::Drawing);
        }
        known
    };
    let muted = cx.theme().muted_foreground;
    match known {
        None | Some(Diagram::Drawing) => {
            if known.is_none() {
                let source = source.to_string();
                let drawn = cx.background_spawn(async move { mermaid_svg(&source, &colors).and_then(|svg| rasterize(&svg, std::path::Path::new(""), DIAGRAM_PIXELS, None)) });
                cx.spawn(async move |cx| {
                    let diagram = match drawn.await {
                        Ok(picture) => Diagram::Drawn(Arc::new(picture)),
                        Err(why) => Diagram::Failed(why.into()),
                    };
                    if let Some(kept) = DIAGRAMS.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                        kept.insert(key, diagram);
                    }
                    cx.update(|cx| cx.refresh_windows());
                })
                .detach();
            }
            div().py_2().text_sm().text_color(muted).child("Drawing the diagram…").into_any_element()
        }
        Some(Diagram::Failed(why)) => v_flex()
            .py_2()
            .gap_1()
            .text_sm()
            .child(div().text_color(gpui_kit::red()).child("The diagram does not draw:"))
            .child(div().text_color(muted).child(why))
            .into_any_element(),
        Some(Diagram::Drawn(picture)) => h_flex()
            .id("diagram")
            .test_support()
            .w_full()
            .py_2()
            .justify_center()
            .child(img(ImageSource::Render(picture.image.clone())).w(px(picture.size.0)).h(px(picture.size.1)).max_w_full().object_fit(ObjectFit::Contain))
            .into_any_element(),
    }
}

/// Whether the SVG sets `color` itself (`color="…"`, `color: …`): then its `currentColor` is its own.
fn names_its_color(text: &str) -> bool {
    text.match_indices("color").any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + 5..].trim_start().chars().next();
        !before.is_some_and(|c| c.is_alphanumeric() || c == '-' || c == '_') && matches!(after, Some('=' | ':'))
    })
}

/// The theme's text color, as an SVG has it: what `currentColor` is in a preview.
pub(crate) fn ink(cx: &App) -> String {
    let c = gpui_kit::Rgba::from(cx.theme().foreground);
    let byte = |x: f32| (x.clamp(0., 1.) * 255.).round() as u8;
    format!("#{:02x}{:02x}{:02x}", byte(c.r), byte(c.g), byte(c.b))
}

/// A CSV or TSV read: its cells are ranges of the text, unquoted when shown.
pub(super) struct Sheet {
    text: SharedString,
    cells: Vec<Range<u32>>,
    /// Where each line's cells start in `cells`, and where the last ends.
    starts: Vec<u32>,
    pub columns: usize,
    pub separator: u8,
    /// Longer than is read: only its first lines are.
    pub cut: bool,
}

/// Lines read of a table; past them it says it shows the first only.
const MOST_LINES: usize = 500_000;

impl Sheet {
    pub(super) fn parse(text: SharedString, separator: Option<u8>) -> Sheet {
        let separator = separator.unwrap_or_else(|| guess_separator(&text));
        let bytes = text.as_bytes();
        let (mut cells, mut starts, mut columns, mut cut) = (Vec::new(), vec![0u32], 0, false);
        let mut i = 0;
        while i < bytes.len() {
            if starts.len() > MOST_LINES {
                cut = true;
                break;
            }
            // One line: cells up to its end, a quoted one across line ends.
            loop {
                let start = i;
                if bytes.get(i) == Some(&b'"') {
                    i += 1;
                    while i < bytes.len() {
                        if bytes[i] == b'"' {
                            if bytes.get(i + 1) == Some(&b'"') {
                                i += 2;
                                continue;
                            }
                            i += 1;
                            break;
                        }
                        i += 1;
                    }
                }
                while i < bytes.len() && bytes[i] != separator && bytes[i] != b'\n' {
                    i += 1;
                }
                let end = if i > start && bytes.get(i) == Some(&b'\n') && bytes[i - 1] == b'\r' { i - 1 } else { i };
                cells.push(start as u32..end as u32);
                if i < bytes.len() && bytes[i] == separator {
                    i += 1;
                    continue;
                }
                i += 1;
                break;
            }
            let line = starts.last().copied().unwrap_or_default() as usize..cells.len();
            columns = columns.max(line.len());
            starts.push(cells.len() as u32);
        }
        Sheet { text, cells, starts, columns, separator, cut }
    }

    /// Lines, the first (the header) too.
    pub(super) fn lines(&self) -> usize {
        self.starts.len() - 1
    }

    /// Rows under the header.
    pub(super) fn rows(&self) -> usize {
        self.lines().saturating_sub(1)
    }

    pub(super) fn cell(&self, line: usize, column: usize) -> Cow<'_, str> {
        let (Some(&from), Some(&to)) = (self.starts.get(line), self.starts.get(line + 1)) else { return Cow::Borrowed("") };
        let Some(range) = self.cells[from as usize..to as usize].get(column) else { return Cow::Borrowed("") };
        let raw = self.text.get(range.start as usize..range.end as usize).unwrap_or_default();
        match raw.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
            Some(inner) if inner.contains("\"\"") => Cow::Owned(inner.replace("\"\"", "\"")),
            Some(inner) => Cow::Borrowed(inner),
            None => Cow::Borrowed(raw),
        }
    }
}

/// The separator of a CSV: the one of `,` `;` tab `|` its first line has most of, outside quotes.
fn guess_separator(text: &str) -> u8 {
    let mut counts = [0usize; 4];
    let mut quoted = false;
    for b in text.bytes() {
        match b {
            b'"' => quoted = !quoted,
            b'\n' if !quoted => break,
            _ if quoted => {}
            b',' => counts[0] += 1,
            b';' => counts[1] += 1,
            b'\t' => counts[2] += 1,
            b'|' => counts[3] += 1,
            _ => {}
        }
    }
    let seps = [b',', b';', b'\t', b'|'];
    let best = (0..4).max_by_key(|&i| (counts[i], 4 - i)).unwrap_or(0);
    if counts[best] == 0 { b',' } else { seps[best] }
}

/// A sheet as the table asks for it: its first line is the header; the
/// rows in the order sorted by a column's header.
pub(super) struct SheetRows {
    pub sheet: Arc<Sheet>,
    order: Vec<u32>,
    widths: Vec<f32>,
    numeric: Vec<bool>,
    /// The column sorted by and how, kept as the text changes.
    sorted: Option<(usize, ColumnSort)>,
}

impl SheetRows {
    fn new(sheet: Arc<Sheet>) -> Self {
        let mut rows = SheetRows { sheet: sheet.clone(), order: Vec::new(), widths: Vec::new(), numeric: Vec::new(), sorted: None };
        rows.show(sheet);
        rows
    }

    fn show(&mut self, sheet: Arc<Sheet>) {
        // Widths and kinds from the first lines: enough to tell, and the same however long it is.
        let sample = sheet.lines().min(200);
        self.widths = (0..sheet.columns)
            .map(|c| {
                let chars = (0..sample).map(|l| sheet.cell(l, c).chars().count()).max().unwrap_or(0);
                (chars.min(48) as f32 * 7.5 + 20.).max(56.)
            })
            .collect();
        self.numeric = (0..sheet.columns)
            .map(|c| {
                let values: Vec<_> = (1..sample.max(1)).map(|l| sheet.cell(l, c)).filter(|v| !v.trim().is_empty()).collect();
                !values.is_empty() && values.iter().all(|v| number(v).is_some())
            })
            .collect();
        self.order = (1..sheet.lines() as u32).collect();
        self.sheet = sheet;
        if let Some((column, sort)) = self.sorted.filter(|(column, _)| *column < self.sheet.columns) {
            self.sort(column, sort);
        } else {
            self.sorted = None;
        }
    }

    /// Each row's key read once, not at every comparison: a long table sorts at once.
    fn sort(&mut self, column: usize, sort: ColumnSort) {
        self.order.sort_unstable();
        if sort == ColumnSort::Default {
            return;
        }
        let sheet = &self.sheet;
        if self.numeric.get(column).copied().unwrap_or(false) {
            let keys: Vec<f64> = self.order.iter().map(|&line| number(&sheet.cell(line as usize, column)).unwrap_or(f64::NEG_INFINITY)).collect();
            let mut ix: Vec<u32> = (0..self.order.len() as u32).collect();
            ix.sort_by(|&a, &b| keys[a as usize].total_cmp(&keys[b as usize]));
            self.order = ix.into_iter().map(|i| self.order[i as usize]).collect();
        } else {
            self.order.sort_by(|&a, &b| sheet.cell(a as usize, column).cmp(&sheet.cell(b as usize, column)));
        }
        if sort == ColumnSort::Descending {
            self.order.reverse();
        }
    }
}

fn number(text: &str) -> Option<f64> {
    text.trim().parse::<f64>().ok()
}

impl TableDelegate for SheetRows {
    fn columns_count(&self, _: &App) -> usize {
        self.sheet.columns
    }

    fn rows_count(&self, _: &App) -> usize {
        self.order.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        let name = self.sheet.cell(0, col_ix).into_owned();
        let column = Column::new(SharedString::from(format!("c{col_ix}")), name).width(px(self.widths.get(col_ix).copied().unwrap_or(100.))).max_width(px(800.)).sortable();
        let column = match self.sorted {
            Some((sorted, sort)) if sorted == col_ix => column.sort(sort),
            _ => column,
        };
        if self.numeric.get(col_ix).copied().unwrap_or(false) { column.text_right() } else { column }
    }

    fn perform_sort(&mut self, col_ix: usize, sort: ColumnSort, _: &mut Window, _: &mut Context<TableState<Self>>) {
        self.sorted = (sort != ColumnSort::Default).then_some((col_ix, sort));
        self.sort(col_ix, sort);
    }

    fn render_td(&mut self, row_ix: usize, col_ix: usize, _: &mut Window, _: &mut Context<TableState<Self>>) -> impl IntoElement {
        let line = self.order.get(row_ix).copied().unwrap_or(0) as usize;
        div().truncate().child(SharedString::from(self.sheet.cell(line, col_ix).into_owned()))
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _: &App) -> String {
        let line = self.order.get(row_ix).copied().unwrap_or(0) as usize;
        self.sheet.cell(line, col_ix).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::{Kind, Sheet, is_page, rasterize};

    fn cells(sheet: &Sheet) -> Vec<Vec<String>> {
        (0..sheet.lines()).map(|l| (0..sheet.columns).map(|c| sheet.cell(l, c).into_owned()).collect()).collect()
    }

    #[test]
    fn a_csv_is_read_with_its_quotes_and_line_ends() {
        let sheet = Sheet::parse("a,b,c\r\n1,\"x, y\",\"say \"\"hi\"\"\"\n2,\"two\nlines\"\n".into(), None);
        assert_eq!(sheet.separator, b',');
        assert_eq!(cells(&sheet), [["a", "b", "c"], ["1", "x, y", "say \"hi\""], ["2", "two\nlines", ""]]);
        assert_eq!(sheet.rows(), 2);
    }

    #[test]
    fn a_sorted_table_stays_sorted_as_its_text_changes() {
        let mut rows = super::SheetRows::new(std::sync::Arc::new(Sheet::parse("n,name\n10,b\n9,a\n,c\n".into(), None)));
        let names = |rows: &super::SheetRows| rows.order.iter().map(|&l| rows.sheet.cell(l as usize, 1).into_owned()).collect::<Vec<_>>();
        rows.sorted = Some((0, super::ColumnSort::Descending));
        rows.sort(0, super::ColumnSort::Descending);
        assert_eq!(names(&rows), ["b", "a", "c"], "by number, not by text; an empty cell last");
        rows.show(std::sync::Arc::new(Sheet::parse("n,name\n10,b\n9,a\n11,d\n".into(), None)));
        assert_eq!(names(&rows), ["d", "b", "a"]);
        rows.sorted = Some((1, super::ColumnSort::Ascending));
        rows.sort(1, super::ColumnSort::Ascending);
        assert_eq!(names(&rows), ["a", "b", "d"]);
    }

    #[test]
    fn a_csv_s_separator_is_guessed_from_its_first_line() {
        assert_eq!(Sheet::parse("a;b;\"c,d\"\n1;2;3".into(), None).separator, b';');
        assert_eq!(Sheet::parse("one\n".into(), None).separator, b',');
        let tsv = Sheet::parse("a\tb\n1,5\t2".into(), Some(b'\t'));
        assert_eq!(cells(&tsv), [["a", "b"], ["1,5", "2"]]);
    }

    #[test]
    fn a_line_with_fewer_cells_has_empty_ones_and_no_last_newline_is_needed() {
        let sheet = Sheet::parse("a,b,c\n1".into(), None);
        assert_eq!(cells(&sheet), [["a", "b", "c"], ["1", "", ""]]);
        assert_eq!(Sheet::parse(String::new().into(), None).lines(), 0);
    }

    #[test]
    fn an_svg_is_drawn_sharp_and_a_broken_one_says_why() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="8"><rect width="16" height="8" fill="red"/></svg>"#;
        let picture = rasterize(svg, std::path::Path::new("/"), super::PREVIEW_PIXELS, None).unwrap();
        assert_eq!(picture.size, (16., 8.));
        assert_eq!(picture.pixels, (1024, 512), "a small icon is drawn large enough to be shown larger");
        let frame = picture.image.as_bytes(0).unwrap();
        assert_eq!(&frame[..4], &[0, 0, 255, 255], "red, in BGRA");
        assert!(rasterize("<svg", std::path::Path::new("/"), super::PREVIEW_PIXELS, None).is_err());
    }

    #[test]
    fn a_mermaid_diagram_is_drawn_in_the_app_s_colors_and_a_broken_one_says_why() {
        let gray = |n: u8| format!("#{n:02x}{n:02x}{n:02x}");
        let colors = super::DiagramColors {
            dark: true,
            background: "#101010".into(),
            text: gray(0xee),
            muted_text: gray(0x99),
            border: gray(0x33),
            fill: gray(0x22),
            fill_alt: gray(0x2a),
            cluster: gray(0x18),
            note: gray(0x30),
            note_border: gray(0x60),
            charts: ["#ff0000", "#00ff00", "#0000ff", "#ffff00", "#00ffff", "#ff00ff"].map(String::from),
        };
        let svg = super::mermaid_svg("flowchart LR\n  A[Open] --> B{Drawn?}", &colors).unwrap();
        assert!(svg.contains("#101010") && svg.contains("#222222") && svg.contains("Open"), "the app's background and fill: {svg}");
        let pie = super::mermaid_svg("pie\n  \"a\" : 1\n  \"b\" : 2", &colors).unwrap();
        assert!(pie.contains("#ff0000") && pie.contains("#00ff00"), "slices in the theme's chart colors: {pie}");
        let picture = rasterize(&svg, std::path::Path::new(""), super::DIAGRAM_PIXELS, None).unwrap();
        assert!(picture.size.0 > 50. && picture.pixels.0 as f32 >= picture.size.0 * 1.9);
        assert!(super::mermaid_svg("flowchart LR\n  A -->", &colors).is_err());
    }

    #[test]
    fn current_color_is_the_theme_s_text_unless_the_svg_names_its_own() {
        let icon = |root: &str| format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2" {root}><rect width="2" height="2" fill="currentColor"/></svg>"#);
        let first = |svg: &str| rasterize(svg, std::path::Path::new("/"), 2.0..=2.0, Some("#00ff00")).unwrap().image.as_bytes(0).unwrap()[..4].to_vec();
        assert_eq!(first(&icon("")), [0, 255, 0, 255], "the ink, in BGRA");
        assert_eq!(first(&icon(r##"color="#ff0000""##)), [0, 0, 255, 255], "its own");
        assert_eq!(first(&icon(r##"style="color: #ff0000""##)), [0, 0, 255, 255]);
        assert!(!super::names_its_color(&icon(r#"stop-color="red" data-color-x="1""#)));
    }

    #[gpui_kit::test]
    fn a_theme_s_diagram_colors_are_made_once_until_it_changes(cx: &mut gpui_kit::TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::init(cx);
            gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
            let (first, a) = super::DiagramColors::current(cx);
            let (again, b) = super::DiagramColors::current(cx);
            assert!(std::sync::Arc::ptr_eq(&first, &again) && a == b);
            gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Light, None, cx);
            let (light, c) = super::DiagramColors::current(cx);
            assert!(light.background != first.background && c != a);
        });
    }

    #[test]
    fn what_is_drawn_and_what_is_a_page() {
        assert_eq!(Kind::of(std::path::Path::new("flow.mmd")), Some(Kind::Mermaid));
        assert_eq!(Kind::of(std::path::Path::new("a/README.MD")), Some(Kind::Markdown));
        assert_eq!(Kind::of(std::path::Path::new("logo.svg")), Some(Kind::Svg));
        assert_eq!(Kind::of(std::path::Path::new("d.tsv")), Some(Kind::Table(Some(b'\t'))));
        assert_eq!(Kind::of(std::path::Path::new("main.rs")), None);
        assert!(is_page(std::path::Path::new("index.html")) && !is_page(std::path::Path::new("a.svg")));
    }
}
