//! Images in diffs: a changed picture shows what it was and what it is,
//! side by side or one above the other, in place of "binary file". Any
//! other binary file has its size before and after.
//!
//! The two versions are git's blobs named on the patch's `index` line, read
//! off the UI thread and kept a while; a side git has no blob of (a file not
//! staged) is the file in the worktree.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use gpui_kit::{AnyElement, App, AppContext as _, InteractiveElement as _, TestSupportExt as _, Image, ImageFormat, ImageSource, IntoElement, ObjectFit, ParentElement, SharedString, Styled, StyledImage as _, Window, div, img, px};
use gpui_kit::component::{ActiveTheme, diff::DiffFile, h_flex, v_flex};

/// What kind of image a file is, by its name: its name for people and its
/// format. SVG is text, and its diff is too.
pub fn kind(path: &Path) -> Option<(&'static str, ImageFormat)> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => ("PNG", ImageFormat::Png),
        "jpg" | "jpeg" => ("JPEG", ImageFormat::Jpeg),
        "gif" => ("GIF", ImageFormat::Gif),
        "webp" => ("WebP", ImageFormat::Webp),
        "bmp" => ("BMP", ImageFormat::Bmp),
        "ico" => ("Icon", ImageFormat::Ico),
        _ => return None,
    })
}

/// The blobs of the two sides from the patch's `index a..b` line; a side
/// that is not there (all zeros, or no line) is None.
fn blobs(headers: &[SharedString]) -> (Option<String>, Option<String>) {
    let Some(range) = headers.iter().find_map(|line| line.strip_prefix("index ")) else { return (None, None) };
    let range = range.split_whitespace().next().unwrap_or_default();
    let (old, new) = range.split_once("..").unwrap_or((range, ""));
    let real = |hash: &str| (!hash.is_empty() && hash.chars().any(|c| c != '0')).then(|| hash.to_string());
    (real(old), real(new))
}

/// One side's picture, loaded.
#[derive(Clone)]
struct Picture {
    image: Drawn,
    size: Option<(f32, f32)>,
    bytes: usize,
}

#[derive(Clone)]
enum Drawn {
    Image(Arc<Image>),
    Pixels(Arc<gpui_kit::RenderImage>),
}

/// How a side is drawn: a picture's own format, or an SVG drawn here,
/// sharp however small it is.
#[derive(Clone)]
enum Format {
    Image(ImageFormat),
    /// With what its `currentColor` is.
    Svg(String),
}

#[derive(Clone)]
enum Slot {
    Loading,
    Ready(Picture),
    Missing,
}

/// Pictures read, by repository and blob (or worktree file and the blob git
/// worked out for it). Bounded: past this many it starts again.
static KEPT: Mutex<Option<HashMap<String, Slot>>> = Mutex::new(None);
const MOST_KEPT: usize = 48;
/// An SVG's side is drawn this large on its longer side: twice the box it is
/// shown in, less than 1 MB.
const SVG_PIXELS: std::ops::RangeInclusive<f32> = 480.0..=480.0;
/// Larger files are not drawn.
const MOST_BYTES: u64 = 24 << 20;

/// What a side is read from: a blob of the repository, and failing that a
/// file of the worktree.
struct Source {
    root: PathBuf,
    blob: Option<String>,
    file: Option<PathBuf>,
}

impl Source {
    /// Off the UI thread: its size, by git or the disk, without reading it.
    fn size(&self) -> Option<u64> {
        if let Some(blob) = &self.blob {
            let out = crate::procs::command("git").arg("-C").arg(&self.root).args(["cat-file", "-s", blob]).output().ok();
            if let Some(size) = out.filter(|out| out.status.success()).and_then(|out| String::from_utf8_lossy(&out.stdout).trim().parse().ok()) {
                return Some(size);
            }
        }
        Some(std::fs::metadata(self.file.as_ref()?).ok()?.len())
    }

    fn key(&self) -> String {
        format!("{}\0{}\0{}", self.root.display(), self.blob.as_deref().unwrap_or_default(), self.file.as_ref().map(|f| f.display().to_string()).unwrap_or_default())
    }

    /// Off the UI thread: git or the disk.
    fn read(&self) -> Option<Vec<u8>> {
        if let Some(blob) = &self.blob {
            let out = crate::procs::command("git").arg("-C").arg(&self.root).args(["cat-file", "blob", blob]).output().ok();
            if let Some(out) = out.filter(|out| out.status.success() && out.stdout.len() as u64 <= MOST_BYTES) {
                return Some(out.stdout);
            }
        }
        let file = self.file.as_ref()?;
        if std::fs::metadata(file).ok()?.len() > MOST_BYTES {
            return None;
        }
        std::fs::read(file).ok()
    }
}

fn picture(bytes: Vec<u8>, format: Format, dir: &Path) -> Option<Picture> {
    let len = bytes.len();
    let format = match format {
        Format::Image(format) => format,
        Format::Svg(ink) => {
            let drawn = crate::task::rasterize_svg(&String::from_utf8_lossy(&bytes), dir, SVG_PIXELS, Some(&ink)).ok()?;
            return Some(Picture { image: Drawn::Pixels(drawn.image), size: Some(drawn.size), bytes: len });
        }
    };
    let size = image::ImageReader::new(std::io::Cursor::new(&bytes)).with_guessed_format().ok().and_then(|reader| reader.into_dimensions().ok());
    Some(Picture { image: Drawn::Image(Arc::new(Image::from_bytes(format, bytes))), size: size.map(|(w, h)| (w as f32, h as f32)), bytes: len })
}

/// The picture of a side if it is read; else starts reading it and has the
/// windows drawn again when it is.
fn side(source: Source, format: Format, cx: &mut App) -> Slot {
    let key = match &format {
        Format::Svg(ink) => format!("{}\0{ink}", source.key()),
        Format::Image(_) => source.key(),
    };
    {
        let mut kept = KEPT.lock().unwrap();
        let kept = kept.get_or_insert_with(HashMap::new);
        if let Some(slot) = kept.get(&key) {
            return slot.clone();
        }
        if kept.len() >= MOST_KEPT {
            kept.retain(|_, slot| matches!(slot, Slot::Loading));
        }
        kept.insert(key.clone(), Slot::Loading);
    }
    let read = cx.background_spawn(async move { source.read().and_then(|bytes| picture(bytes, format, &source.root)) });
    cx.spawn(async move |cx| {
        let slot = read.await.map_or(Slot::Missing, Slot::Ready);
        if let Some(kept) = KEPT.lock().unwrap().as_mut() {
            kept.insert(key, slot);
        }
        cx.update(|cx| cx.refresh_windows());
    })
    .detach();
    Slot::Loading
}

/// Sizes read, as pictures are: None while read, Some(None) when there is
/// no telling.
static SIZES: Mutex<Option<HashMap<String, Option<Option<u64>>>>> = Mutex::new(None);

/// The size of a side if it is known; else starts asking and has the
/// windows drawn again when it is told.
fn size_of(source: Source, cx: &mut App) -> Option<Option<u64>> {
    let key = source.key();
    {
        let mut kept = SIZES.lock().unwrap();
        let kept = kept.get_or_insert_with(HashMap::new);
        if let Some(size) = kept.get(&key) {
            return *size;
        }
        if kept.len() >= MOST_KEPT * 4 {
            kept.retain(|_, size| size.is_none());
        }
        kept.insert(key.clone(), None);
    }
    let read = cx.background_spawn(async move { source.size() });
    cx.spawn(async move |cx| {
        let size = read.await;
        if let Some(kept) = SIZES.lock().unwrap().as_mut() {
            kept.insert(key, Some(size));
        }
        cx.update(|cx| cx.refresh_windows());
    })
    .detach();
    None
}

/// A side's size, as a binary file's diff tells it.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Size {
    /// The file is not on this side: added or deleted.
    Absent,
    Reading,
    Unknown,
    Bytes(u64),
}

impl Size {
    fn of(asked: Option<Option<u64>>) -> Self {
        match asked {
            None => Size::Reading,
            Some(None) => Size::Unknown,
            Some(Some(bytes)) => Size::Bytes(bytes),
        }
    }

    fn told(self) -> String {
        match self {
            Size::Absent => "none".into(),
            Size::Reading => "…".into(),
            Size::Unknown => "?".into(),
            Size::Bytes(bytes) => crate::task::human_size(bytes),
        }
    }
}

/// "Binary file changed: 1.2 KB → 1.3 KB (+100 B)", "Binary file added, 4 KB".
fn sizes_told(before: Size, after: Size) -> String {
    match (before, after) {
        (Size::Absent, a) => format!("Binary file added, {}", a.told()),
        (b, Size::Absent) => format!("Binary file deleted, was {}", b.told()),
        (Size::Bytes(b), Size::Bytes(a)) if a == b => format!("Binary file changed, {} as before", after.told()),
        (Size::Bytes(b), Size::Bytes(a)) => {
            let change = if a > b { format!("+{}", crate::task::human_size(a - b)) } else { format!("−{}", crate::task::human_size(b - a)) };
            format!("Binary file changed: {} → {} ({change})", before.told(), after.told())
        }
        (b, a) => format!("Binary file changed: {} → {}", b.told(), a.told()),
    }
}

/// What a binary file of a diff shows: an image's two versions, each with
/// its size; any other file its sizes. `root` is the repository the diff
/// is of; `worktree` is where the after side may be read from when git
/// has no blob of it (a change not staged, a file not tracked).
pub fn render(file: &DiffFile, split: bool, root: &Path, worktree: bool, window: &mut Window, cx: &mut App) -> Option<AnyElement> {
    let _ = window;
    let path = file.path().to_string();
    let svg = Path::new(&path).extension().is_some_and(|e| e.eq_ignore_ascii_case("svg"));
    let (name, format) = match kind(Path::new(&path)) {
        Some((name, format)) if file.is_binary() => (name, Format::Image(format)),
        // A text file has its diff; an SVG its pictures above it too.
        _ if svg && !file.is_binary() => ("SVG", Format::Svg(crate::task::ink(cx))),
        _ if file.is_binary() => return Some(sizes(file, root, worktree, cx)),
        _ => return None,
    };
    let (old_blob, new_blob) = blobs(file.extended_headers());
    let status = file.status();
    use gpui_kit::component::diff::DiffFileStatus as S;
    let before = (status != S::Added && old_blob.is_some()).then(|| side(Source { root: root.to_path_buf(), blob: old_blob, file: None }, format.clone(), cx));
    let after = (status != S::Deleted).then(|| {
        let modified = file.modified_path().map(|p| root.join(p.as_ref()));
        side(Source { root: root.to_path_buf(), blob: new_blob, file: modified.filter(|_| worktree) }, format, cx)
    });
    let muted = cx.theme().muted_foreground;
    let border = cx.theme().border;
    let pane = |label: &'static str, slot: Option<Slot>| {
        let shown = match &slot {
            None => "none",
            Some(Slot::Loading) => "reading",
            Some(Slot::Missing) => "missing",
            Some(Slot::Ready(_)) => "shown",
        };
        let id = SharedString::from(format!("image-diff-{}-{shown}", label.to_lowercase()));
        let (picture, caption): (AnyElement, String) = match slot {
            None => (div().text_color(muted).child("No file").into_any_element(), String::new()),
            Some(Slot::Loading) => (div().text_color(muted).child("Reading…").into_any_element(), String::new()),
            Some(Slot::Missing) => (div().text_color(muted).child("Not found").into_any_element(), String::new()),
            Some(Slot::Ready(p)) => {
                let size = p.size.map(|(w, h)| format!(" · {}×{}", w.round(), h.round())).unwrap_or_default();
                (
                    img(match p.image.clone() {
                        Drawn::Image(image) => ImageSource::from(image),
                        Drawn::Pixels(pixels) => ImageSource::Render(pixels),
                    })
                    .size_full().object_fit(ObjectFit::ScaleDown).into_any_element(),
                    format!("{name}{size} · {}", crate::task::human_size(p.bytes as u64)),
                )
            }
        };
        v_flex()
            .flex_1()
            .min_w_0()
            .gap_1()
            .child(h_flex().gap_2().text_xs().text_color(muted).child(label).child(caption))
            .child(div().id(id).test_support().h(px(240.)).w_full().p_1().border_1().border_color(border).rounded_sm().flex().items_center().justify_center().child(picture))
    };
    let (old, new) = (pane("Before", before), pane("After", after));
    let row = if split { h_flex().gap_3().child(old).child(new).into_any_element() } else { v_flex().gap_2().child(old).child(new).into_any_element() };
    Some(div().w_full().px_3().py_2().child(row).into_any_element())
}

/// A binary file that is not a picture: its headers, as the diff has
/// them, and how large it was and is.
fn sizes(file: &DiffFile, root: &Path, worktree: bool, cx: &mut App) -> AnyElement {
    use gpui_kit::component::diff::DiffFileStatus as S;
    let (old_blob, new_blob) = blobs(file.extended_headers());
    let status = file.status();
    let before = if status == S::Added || old_blob.is_none() {
        Size::Absent
    } else {
        Size::of(size_of(Source { root: root.to_path_buf(), blob: old_blob, file: None }, cx))
    };
    let after = if status == S::Deleted {
        Size::Absent
    } else {
        let modified = file.modified_path().map(|p| root.join(p.as_ref()));
        Size::of(size_of(Source { root: root.to_path_buf(), blob: new_blob, file: modified.filter(|_| worktree) }, cx))
    };
    let told = sizes_told(before, after);
    v_flex()
        .w_full()
        .px_3()
        .py_2()
        .gap_1()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .children(file.extended_headers().iter().filter(|line| !line.starts_with("Binary files ")).cloned().map(|line| div().child(line)))
        .child(div().id("binary-sizes").test_support().text_sm().child(told))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_blobs_come_from_the_index_line() {
        let line = |s: &str| vec![SharedString::from(s.to_string())];
        assert_eq!(blobs(&line("index 1a2b3c4..5d6e7f8 100644")), (Some("1a2b3c4".into()), Some("5d6e7f8".into())));
        assert_eq!(blobs(&line("index 0000000..5d6e7f8")), (None, Some("5d6e7f8".into())));
        assert_eq!(blobs(&line("index 1a2b3c4..0000000")), (Some("1a2b3c4".into()), None));
        assert_eq!(blobs(&[]), (None, None));
        assert_eq!(kind(Path::new("docs/a.PNG")).map(|k| k.0), Some("PNG"));
        assert!(kind(Path::new("logo.svg")).is_none());
    }

    struct Host(gpui_kit::Entity<gpui_kit::component::diff::DiffState>, PathBuf);

    impl gpui_kit::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut gpui_kit::Context<Self>) -> impl IntoElement {
            let root = self.1.clone();
            div().size_full().child(gpui_kit::component::diff::Diff::new(&self.0).size_full().binary_content(move |file, split, window, cx| render(file, split, &root, true, window, cx)))
        }
    }

    /// A changed PNG in a diff: the committed one before, the worktree's
    /// after, within the view in both modes, next to a line wider than it.
    #[gpui_kit::test]
    fn a_changed_image_shows_both_versions(cx: &mut gpui_kit::TestAppContext) {
        shows_both(gpui_kit::component::diff::DiffMode::Split, cx);
    }

    #[gpui_kit::test]
    fn a_changed_image_shows_both_versions_unified(cx: &mut gpui_kit::TestAppContext) {
        shows_both(gpui_kit::component::diff::DiffMode::Unified, cx);
    }

    /// An SVG is text, and its diff is: its two pictures are above it.
    #[gpui_kit::test]
    fn a_changed_svg_shows_both_versions_above_its_text(cx: &mut gpui_kit::TestAppContext) {
        shows_both_of("a.svg", gpui_kit::component::diff::DiffMode::Split, cx);
    }

    fn shows_both(mode: gpui_kit::component::diff::DiffMode, cx: &mut gpui_kit::TestAppContext) {
        shows_both_of("a.png", mode, cx);
    }

    fn shows_both_of(name: &str, mode: gpui_kit::component::diff::DiffMode, cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        use gpui_kit::{AppContext as _, Bounds, Point, WindowBounds, WindowOptions, size};
        let dir = std::env::temp_dir().join(format!("ideae-image-diff-ui-{}-{mode:?}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| std::process::Command::new("git").arg("-C").arg(&dir).args(["-c", "user.name=t", "-c", "user.email=t@example.com"]).args(args).output().unwrap();
        let svg = name.ends_with(".svg");
        let png = |w: u32| {
            if svg {
                return format!("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"4\">\n<rect width=\"{w}\" height=\"4\"/>\n</svg>\n").into_bytes();
            }
            let mut out = Vec::new();
            image::RgbaImage::new(w, 4).write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
            out
        };
        git(&["init", "-q"]);
        std::fs::write(dir.join(name), png(3)).unwrap();
        std::fs::write(dir.join("b.txt"), "short\n").unwrap();
        git(&["add", name, "b.txt"]);
        git(&["commit", "-qm", "a"]);
        std::fs::write(dir.join(name), png(5)).unwrap();
        std::fs::write(dir.join("b.txt"), format!("{}\n", "wide ".repeat(400))).unwrap();
        let patch = String::from_utf8(git(&["diff"]).stdout).unwrap();
        let files = DiffFile::parse(&patch).unwrap();
        assert_eq!(files[0].is_binary(), !svg, "{patch}");
        cx.update(gpui_kit::init);
        let root = dir.clone();
        let handle = cx.update(|cx| {
            let bounds = Bounds { origin: Point::default(), size: size(px(900.), px(600.)) };
            let options = WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() };
            gpui_kit::open_window(options, cx, |_, cx| {
                let state = cx.new(|cx| gpui_kit::component::diff::DiffState::new(files, cx).with_mode(mode));
                cx.new(|_| Host(state, root))
            })
            .expect("a test window")
            .0
        });
        let frame = |cx: &mut gpui_kit::TestAppContext| cx.update_window(handle, |_, window, cx| window.render_frame(cx)).unwrap();
        frame(cx);
        for _ in 0..50 {
            cx.run_until_parked();
            frame(cx);
            let shown = cx.update_window(handle, |_, window, _| window.try_find("image-diff-before-shown").is_some() && window.try_find("image-diff-after-shown").is_some()).unwrap();
            if shown {
                let right = cx.update_window(handle, |_, window, _| window.find("image-diff-after-shown").bounds().right()).unwrap();
                assert!(right <= px(900.), "the picture within the view: its right edge at {right:?}");
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let ids = cx.update_window(handle, |_, window, _| ["before", "after"].map(|side| ["reading", "missing", "none", "shown"].into_iter().find(|s| window.try_find(SharedString::from(format!("image-diff-{side}-{s}"))).is_some()))).unwrap();
        panic!("both sides drawn as pictures: {ids:?}");
    }

    #[test]
    fn a_binary_file_tells_its_sizes() {
        assert_eq!(sizes_told(Size::Absent, Size::Bytes(4096)), "Binary file added, 4 KB");
        assert_eq!(sizes_told(Size::Bytes(2048), Size::Absent), "Binary file deleted, was 2 KB");
        assert_eq!(sizes_told(Size::Bytes(1024), Size::Bytes(3072)), "Binary file changed: 1 KB → 3 KB (+2 KB)");
        assert_eq!(sizes_told(Size::Bytes(300), Size::Bytes(100)), "Binary file changed: 300 B → 100 B (−200 B)");
        assert_eq!(sizes_told(Size::Reading, Size::Bytes(5)), "Binary file changed: … → 5 B");
    }

    #[test]
    fn a_side_is_read_from_its_blob_or_else_the_worktree() {
        let dir = std::env::temp_dir().join(format!("ideae-image-diff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.as_path();
        let git = |args: &[&str]| assert!(std::process::Command::new("git").arg("-C").arg(root).args(args).output().unwrap().status.success(), "{args:?}");
        git(&["init", "-q"]);
        std::fs::write(root.join("a.png"), b"first").unwrap();
        git(&["add", "a.png"]);
        let blob = String::from_utf8(std::process::Command::new("git").arg("-C").arg(root).args(["rev-parse", ":a.png"]).output().unwrap().stdout).unwrap();
        std::fs::write(root.join("a.png"), b"second").unwrap();
        let old = Source { root: root.into(), blob: Some(blob.trim()[..7].to_string()), file: None };
        assert_eq!(old.read().as_deref(), Some(&b"first"[..]));
        // Not staged: git has no blob of what is in the worktree.
        let new = Source { root: root.into(), blob: Some("abcdef1".into()), file: Some(root.join("a.png")) };
        assert_eq!(new.read().as_deref(), Some(&b"second"[..]));
        assert_eq!((old.size(), new.size()), (Some(5), Some(6)), "sizes too, from git and from the disk");
        let gone = Source { root: root.into(), blob: Some("abcdef1".into()), file: None };
        assert_eq!(gone.read(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
