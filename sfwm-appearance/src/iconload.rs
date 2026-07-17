//! Background icon resolution + rasterization for the Icons tab preview.
//!
//! Like the wallpaper thumbnail loader, all decoding happens off the UI thread.
//! The UI sends an [`IconRequest`] (a theme dir-name + a slot's alternative
//! icon names); the worker resolves the first name that exists, decodes PNG
//! (via `image`) or rasterizes SVG (via `resvg`/`tiny-skia`) to RGBA, and sends
//! it back. SVG output is premultiplied; PNG output is straight (unmultiplied),
//! so each result carries a `premultiplied` flag for correct upload.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use crate::icons;

/// Longest side, in pixels, that preview icons are rasterized/decoded to.
pub const ICON_PX: u32 = 64;

/// A request to render one preview slot.
pub struct IconRequest {
    /// Theme directory name (identifier).
    pub theme_dir: String,
    /// Alternative icon names for this slot, tried in order.
    pub names: Vec<String>,
    /// Icon base dirs for resolution (cloned from the app).
    pub base_dirs: Vec<PathBuf>,
}

/// Stable cache key for a rendered slot: `(theme_dir, first-name)`.
pub type IconKey = (String, String);

/// A rendered (or failed) preview coming back from the worker.
pub struct IconResult {
    pub key: IconKey,
    /// `Some((w, h, rgba))` on success, `None` if nothing resolved/decoded.
    pub image: Option<(usize, usize, Vec<u8>)>,
    /// Whether `rgba` is premultiplied (SVG) or straight (PNG).
    pub premultiplied: bool,
}

/// Handle to the icon worker thread.
pub struct IconLoader {
    req_tx: Sender<IconRequest>,
    res_rx: Receiver<IconResult>,
}

impl IconLoader {
    pub fn spawn(ctx: egui::Context) -> IconLoader {
        let (req_tx, req_rx) = mpsc::channel::<IconRequest>();
        let (res_tx, res_rx) = mpsc::channel::<IconResult>();
        thread::Builder::new()
            .name("icon-loader".into())
            .spawn(move || {
                while let Ok(req) = req_rx.recv() {
                    let key: IconKey = (
                        req.theme_dir.clone(),
                        req.names.first().cloned().unwrap_or_default(),
                    );
                    let names: Vec<&str> = req.names.iter().map(|s| s.as_str()).collect();
                    let resolved = icons::resolve_slot(
                        &req.theme_dir,
                        &names,
                        &req.base_dirs,
                        ICON_PX as i32,
                    );
                    let (image, premultiplied) = match resolved {
                        Some(path) => render_icon(&path),
                        None => (None, false),
                    };
                    if res_tx
                        .send(IconResult {
                            key,
                            image,
                            premultiplied,
                        })
                        .is_err()
                    {
                        break;
                    }
                    ctx.request_repaint();
                }
            })
            .expect("spawn icon-loader thread");
        IconLoader { req_tx, res_rx }
    }

    pub fn request(&self, req: IconRequest) {
        let _ = self.req_tx.send(req);
    }

    pub fn drain(&self) -> Vec<IconResult> {
        self.res_rx.try_iter().collect()
    }
}

/// Decode/rasterize an icon file to RGBA at ~`ICON_PX`. Returns the pixels and
/// whether they're premultiplied.
fn render_icon(path: &Path) -> (Option<(usize, usize, Vec<u8>)>, bool) {
    let is_svg = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("svg"))
        .unwrap_or(false);
    if is_svg {
        (render_svg(path), true)
    } else {
        (decode_png(path), false)
    }
}

/// Decode a raster icon and downscale so its longest side is at most `ICON_PX`.
fn decode_png(path: &Path) -> Option<(usize, usize, Vec<u8>)> {
    let img = image::open(path).ok()?;
    let img = img.thumbnail(ICON_PX, ICON_PX);
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as usize, rgba.height() as usize);
    Some((w, h, rgba.into_raw()))
}

/// Rasterize an SVG to premultiplied RGBA sized so its longest side ≈ `ICON_PX`.
fn render_svg(path: &Path) -> Option<(usize, usize, Vec<u8>)> {
    let data = std::fs::read(path).ok()?;
    let opt = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_data(&data, &opt).ok()?;

    let size = tree.size();
    let (sw, sh) = (size.width(), size.height());
    if sw <= 0.0 || sh <= 0.0 {
        return None;
    }
    let scale = ICON_PX as f32 / sw.max(sh);
    let pw = (sw * scale).round().max(1.0) as u32;
    let ph = (sh * scale).round().max(1.0) as u32;

    let mut pixmap = resvg::tiny_skia::Pixmap::new(pw, ph)?;
    let transform = resvg::tiny_skia::Transform::from_scale(scale, scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    Some((pw as usize, ph as usize, pixmap.take()))
}
