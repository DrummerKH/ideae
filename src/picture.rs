//! A picture to look at: fit in its room or at a size of its own (the
//! buttons, Ctrl+wheel), on a background to see it against. An SVG's
//! preview and an image file's tab show theirs so.

use std::sync::atomic::{AtomicU8, Ordering};

use gpui_kit::component::{ActiveTheme as _, Selectable as _, Sizable as _, button::{Button, ButtonGroup, ButtonVariants as _}, h_flex, scroll::Scrollbar, v_flex};
use gpui_kit::{prelude::FluentBuilder as _, *};

actions!(ideae_picture, [PictureZoomIn, PictureZoomOut, PictureActualSize, PictureFit]);

/// The key context of a picture with the keyboard: its zoom keys over the
/// same keys elsewhere (an editor's fold, the text's size).
pub const KEY_CONTEXT: &str = "Picture";

/// What is behind the picture: what is clear in it shows this.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Back {
    /// The theme's background, as the picture would be in the app.
    #[default]
    Theme,
    Checker,
    Light,
    Dark,
}

impl Back {
    const ALL: [Back; 4] = [Back::Theme, Back::Checker, Back::Light, Back::Dark];
}

/// The last background chosen: a picture opened next starts on it.
static LAST_BACK: AtomicU8 = AtomicU8::new(0);

const LEAST_ZOOM: f32 = 0.05;
const MOST_ZOOM: f32 = 32.;

pub struct PictureView {
    image: Option<ImageSource>,
    /// What was shown last, as its owner tells it: the same one is not shown again.
    key: u64,
    /// Its own size, at 100 %.
    size: Option<Size<Pixels>>,
    /// Fit, it grows no larger than this.
    most: Option<Size<Pixels>>,
    caption: SharedString,
    /// None: fit in the room.
    zoom: Option<f32>,
    back: Back,
    /// Its size as fit, for zooming from there.
    fit: Option<Size<Pixels>>,
    scroll: ScrollHandle,
    focus: FocusHandle,
}

impl PictureView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let back = Back::ALL.get(LAST_BACK.load(Ordering::Relaxed) as usize).copied().unwrap_or_default();
        PictureView { image: None, key: 0, size: None, most: None, caption: SharedString::default(), zoom: None, back, fit: None, scroll: ScrollHandle::new(), focus: cx.focus_handle() }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// A picture by its owner's `key`, its size at 100 %, the most it grows to fit, a word on it.
    pub fn show(&mut self, key: u64, image: ImageSource, size: Option<Size<Pixels>>, most: Option<Size<Pixels>>, caption: SharedString, cx: &mut Context<Self>) {
        if self.image.is_some() && self.key == key && self.size == size && self.caption == caption {
            return;
        }
        (self.key, self.image, self.size, self.most, self.caption) = (key, Some(image), size, most, caption);
        cx.notify();
    }

    fn zoom_to(&mut self, zoom: Option<f32>, cx: &mut Context<Self>) {
        self.zoom = zoom.map(|z| z.clamp(LEAST_ZOOM, MOST_ZOOM));
        if self.zoom.is_none() {
            self.scroll.set_offset(point(px(0.), px(0.)));
        }
        cx.notify();
    }

    /// By the wheel: the point of the picture under the pointer stays under it.
    fn zoom_at(&mut self, steps: f32, pointer: Point<Pixels>, cx: &mut Context<Self>) {
        let before = self.zoom;
        self.zoom_by(steps, cx);
        let (Some(before), Some(after)) = (before, self.zoom) else { return };
        let at = pointer - self.scroll.bounds().origin;
        let offset = self.scroll.offset();
        let ratio = after / before;
        self.scroll.set_offset(point(at.x - (at.x - offset.x) * ratio, at.y - (at.y - offset.y) * ratio));
    }

    /// From the zoom shown, by steps of a tenth; from fit, from the scale it fits at.
    fn zoom_by(&mut self, steps: f32, cx: &mut Context<Self>) {
        let now = self.zoom.or_else(|| Some(self.fit?.width / self.size?.width)).unwrap_or(1.);
        self.zoom_to(Some(now * 1.1f32.powf(steps)), cx);
    }

    fn set_back(&mut self, back: Back, cx: &mut Context<Self>) {
        self.back = back;
        LAST_BACK.store(Back::ALL.iter().position(|b| *b == back).unwrap_or(0) as u8, Ordering::Relaxed);
        cx.notify();
    }

    fn render_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let zoom = match self.zoom {
            Some(z) => format!("{:.0} %", z * 100.),
            None => "Fit".into(),
        };
        let backs = [(Back::Theme, "Theme", "The theme's background"), (Back::Checker, "▦", "A checkerboard: what is clear shows"), (Back::Light, "Light", "White behind it"), (Back::Dark, "Dark", "Black behind it")];
        let back = self.back;
        h_flex()
            .w_full()
            .gap_2()
            .px_3()
            .py_1()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .border_t_1()
            .border_color(cx.theme().border)
            .child(div().flex_1().min_w_0().truncate().child(self.caption.clone()))
            .child(
                h_flex()
                    .gap_1()
                    .child(Button::new("picture-fit").xsmall().ghost().label("Fit").selected(self.zoom.is_none()).tooltip(format!("Fit in the room ({})", crate::keymap::label("picture_fit"))).on_click(crate::learn::clicked("picture_fit", cx.listener(|this, _, _, cx| this.zoom_to(None, cx)))))
                    .child(Button::new("picture-actual").xsmall().ghost().label("1:1").selected(self.zoom == Some(1.)).tooltip(format!("Its own size ({})", crate::keymap::label("picture_actual_size"))).on_click(crate::learn::clicked("picture_actual_size", cx.listener(|this, _, _, cx| this.zoom_to(Some(1.), cx)))))
                    .child(Button::new("picture-smaller").xsmall().ghost().label("−").tooltip(format!("Smaller ({}, Ctrl+wheel)", crate::keymap::label("picture_zoom_out"))).on_click(crate::learn::clicked("picture_zoom_out", cx.listener(|this, _, _, cx| this.zoom_by(-3., cx)))))
                    .child(div().id("picture-zoom").test_support().min_w(px(40.)).text_center().child(zoom))
                    .child(Button::new("picture-larger").xsmall().ghost().label("+").tooltip(format!("Larger ({}, Ctrl+wheel)", crate::keymap::label("picture_zoom_in"))).on_click(crate::learn::clicked("picture_zoom_in", cx.listener(|this, _, _, cx| this.zoom_by(3., cx))))),
            )
            .child(
                ButtonGroup::new("picture-back")
                    .xsmall()
                    .ghost()
                    .children(backs.iter().enumerate().map(|(ix, &(each, label, tip))| Button::new(("picture-back", ix)).label(label).tooltip(tip).selected(back == each)))
                    .on_click(cx.listener(move |this, clicked: &Vec<usize>, _, cx| {
                        if let Some(&(each, _, _)) = clicked.first().and_then(|&ix| backs.get(ix)) {
                            this.set_back(each, cx);
                        }
                    })),
            )
    }
}

/// Squares behind a picture, drawn for the part in view.
fn checker(cx: &App) -> impl IntoElement {
    let dark = cx.theme().mode.is_dark();
    let (a, b) = if dark { (hsla(0., 0., 0.22, 1.), hsla(0., 0., 0.3, 1.)) } else { (hsla(0., 0., 1., 1.), hsla(0., 0., 0.86, 1.)) };
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let side = px(8.);
            let shown = bounds.intersect(&window.content_mask().bounds);
            window.paint_quad(fill(bounds, a));
            if shown.size.width <= px(0.) || shown.size.height <= px(0.) {
                return;
            }
            let (from_x, from_y) = (((shown.left() - bounds.left()) / side).floor() as i64, ((shown.top() - bounds.top()) / side).floor() as i64);
            let (to_x, to_y) = (((shown.right() - bounds.left()) / side).ceil() as i64, ((shown.bottom() - bounds.top()) / side).ceil() as i64);
            for y in from_y..to_y {
                for x in from_x..to_x {
                    if (x + y) % 2 == 0 {
                        continue;
                    }
                    let origin = point(bounds.left() + side * x as f32, bounds.top() + side * y as f32);
                    let square = Bounds { origin, size: size(side, side) }.intersect(&bounds);
                    window.paint_quad(fill(square, b));
                }
            }
        },
    )
    .absolute()
    .size_full()
}

/// Shift+wheel goes sideways: a mouse has no other way to.
fn sideways(scroll: ScrollHandle, view: WeakEntity<PictureView>) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            window.on_mouse_event(move |event: &ScrollEvent, phase, _, cx| {
                if phase != DispatchPhase::Capture || !event.modifiers.shift || event.modifiers.control || !bounds.contains(&event.position) {
                    return;
                }
                cx.stop_propagation();
                let delta = event.delta.pixel_delta(px(20.));
                let (now, most) = (scroll.offset(), scroll.max_offset());
                scroll.set_offset(point((now.x + delta.x + delta.y).clamp(-most.x, px(0.)), now.y));
                let _ = view.update(cx, |_, cx| cx.notify());
            });
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

impl Render for PictureView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(image) = self.image.clone() else { return v_flex().size_full().into_any_element() };
        let behind = |pic: Stateful<Div>, cx: &App| match self.back {
            Back::Theme => pic,
            Back::Checker => pic.child(checker(cx)),
            Back::Light => pic.bg(gpui_kit::white()),
            Back::Dark => pic.bg(gpui_kit::black()),
        };
        let picture = img(image).size_full().object_fit(ObjectFit::Contain);
        let area = match (self.zoom, self.size) {
            (Some(zoom), Some(own)) => {
                let shown = size(own.width * zoom, own.height * zoom);
                // Its own size, not the room's: centered in the room, what is larger would stick out to the left and the top, out of reach.
                div().id("picture-scroll").test_support().size_full().flex().items_start().overflow_scroll().track_scroll(&self.scroll).child(
                    div().flex_none().min_w_full().min_h_full().p_4().flex().items_center().justify_center().child(
                        behind(div().id("picture").relative().flex_none().w(shown.width).h(shown.height), cx).test_support().child(picture),
                    ),
                )
            }
            _ => {
                // Fit: as large as the room, no larger than it may grow, its sides kept.
                let most = self.most.or(self.size);
                let this = cx.entity().downgrade();
                let pic = behind(div().id("picture").relative().size_full().when_some(most, |pic, most| pic.max_w(most.width).max_h(most.height)), cx).test_support();
                let pic = match self.size {
                    // The box takes the picture's proportions, so the background is the picture's own.
                    Some(own) if own.width > px(0.) && own.height > px(0.) => pic.child(picture).child(
                        canvas(
                            move |bounds, _, cx| {
                                let scale = (bounds.size.width / own.width).min(bounds.size.height / own.height);
                                let fit = size(own.width * scale, own.height * scale);
                                let _ = this.update(cx, |v, _| v.fit = Some(fit));
                            },
                            |_, _, _, _| (),
                        )
                        .absolute()
                        .size_full(),
                    ),
                    _ => pic.child(picture),
                };
                div().id("picture-scroll").test_support().size_full().p_4().flex().items_center().justify_center().child(pic)
            }
        };
        let focus = self.focus.clone();
        v_flex()
            .id("picture-view")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus)
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                if !focus.contains_focused(window, cx) {
                    focus.focus(window, cx);
                }
            })
            .on_action(cx.listener(|this, _: &PictureZoomIn, _, cx| this.zoom_by(3., cx)))
            .on_action(cx.listener(|this, _: &PictureZoomOut, _, cx| this.zoom_by(-3., cx)))
            .on_action(cx.listener(|this, _: &PictureActualSize, _, cx| this.zoom_to(Some(1.), cx)))
            .on_action(cx.listener(|this, _: &PictureFit, _, cx| this.zoom_to(None, cx)))
            .size_full()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .relative()
                    .on_scroll(cx.listener(|this, event: &ScrollEvent, _, cx| {
                        if !event.modifiers.control {
                            return;
                        }
                        cx.stop_propagation();
                        let steps = crate::zoom::steps(&event.delta, cx);
                        if steps != 0. {
                            this.zoom_at(steps, event.position, cx);
                        }
                    }))
                    .child(area)
                    .when(self.zoom.is_some(), |this| {
                        this.child(div().absolute().inset_0().child(Scrollbar::new(&self.scroll))).child(sideways(self.scroll.clone(), cx.entity().downgrade()))
                    }),
            )
            .child(self.render_bar(cx))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{Back, PictureView};
    use gpui_kit::{AppContext as _, px, size};

    #[gpui_kit::test]
    fn a_picture_zooms_from_where_it_fits_and_keeps_its_background(cx: &mut gpui_kit::TestAppContext) {
        let view = cx.new(PictureView::new);
        view.update(cx, |v, cx| {
            let image = gpui_kit::ImageSource::from(std::path::PathBuf::from("/a.png"));
            v.show(1, image, Some(size(px(200.), px(100.))), None, "PNG".into(), cx);
            v.fit = Some(size(px(100.), px(50.)));
            v.zoom_by(0., cx);
            assert_eq!(v.zoom, Some(0.5), "from fit, its scale there");
            v.zoom_by(1., cx);
            assert!((v.zoom.unwrap() - 0.55).abs() < 1e-4);
            v.zoom_to(Some(1000.), cx);
            assert_eq!(v.zoom, Some(super::MOST_ZOOM));
            v.set_back(Back::Checker, cx);
        });
        let next = cx.new(PictureView::new);
        next.update(cx, |v, _| assert_eq!(v.back, Back::Checker, "the next one starts on it"));
        next.update(cx, |v, cx| v.set_back(Back::Theme, cx));
    }
}
