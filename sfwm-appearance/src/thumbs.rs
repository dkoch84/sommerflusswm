//! Background thumbnail decoding.
//!
//! Full-resolution JPEG/PNG decoding is far too slow to run on the UI thread
//! (the GPU-less test VM would visibly stall), so all decoding happens on a
//! dedicated worker thread. The UI thread sends a `PathBuf` request; the worker
//! decodes + downscales with the `image` crate and sends back raw RGBA, which
//! the UI thread then uploads to egui via `ctx.load_texture`.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

/// Longest side, in pixels, of a generated thumbnail.
pub const THUMB_MAX: u32 = 200;

/// A decoded (or failed) thumbnail coming back from the worker.
pub struct ThumbResult {
    pub path: PathBuf,
    /// `Some((width, height, rgba))` on success, `None` if decoding failed.
    pub image: Option<(usize, usize, Vec<u8>)>,
}

/// Handle to the worker thread: a request sender + a result receiver.
pub struct ThumbLoader {
    req_tx: Sender<PathBuf>,
    res_rx: Receiver<ThumbResult>,
}

impl ThumbLoader {
    /// Spawn the worker. `ctx` is cloned so the worker can request a repaint
    /// when a thumbnail is ready (egui::Context is cheap to clone and Send).
    pub fn spawn(ctx: egui::Context) -> ThumbLoader {
        let (req_tx, req_rx) = mpsc::channel::<PathBuf>();
        let (res_tx, res_rx) = mpsc::channel::<ThumbResult>();
        thread::Builder::new()
            .name("thumb-loader".into())
            .spawn(move || {
                // Exits when the request sender is dropped (app shutdown).
                while let Ok(path) = req_rx.recv() {
                    let image = decode_thumb(&path);
                    if res_tx.send(ThumbResult { path, image }).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            })
            .expect("spawn thumb-loader thread");
        ThumbLoader { req_tx, res_rx }
    }

    /// Queue a path for decoding. Errors (worker gone) are ignored.
    pub fn request(&self, path: PathBuf) {
        let _ = self.req_tx.send(path);
    }

    /// Drain all results that have arrived since the last call.
    pub fn drain(&self) -> Vec<ThumbResult> {
        self.res_rx.try_iter().collect()
    }
}

/// Decode `path` and downscale so its longest side is at most `THUMB_MAX`.
/// Returns `None` on any decode error (corrupt/unsupported file).
fn decode_thumb(path: &PathBuf) -> Option<(usize, usize, Vec<u8>)> {
    let img = image::open(path).ok()?;
    // `thumbnail` is a fast, quality-reduced resize suitable for previews.
    let img = img.thumbnail(THUMB_MAX, THUMB_MAX);
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as usize, rgba.height() as usize);
    Some((w, h, rgba.into_raw()))
}
