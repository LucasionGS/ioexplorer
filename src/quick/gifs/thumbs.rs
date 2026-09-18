//! Small animated thumbnails for the GIF grid.
//!
//! Every frame is decoded once, on a worker thread, and scaled down to the
//! size of a cell as it goes, so a long GIF costs a few megabytes in the grid
//! rather than the hundreds its full-size frames would. The frames then play
//! from a tick callback on each cell, which GTK only runs while the cell is on
//! screen.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    io::Cursor,
    path::{Path, PathBuf},
    rc::Rc,
};

use gtk::{gdk, gio, glib, prelude::*};
use image::{
    AnimationDecoder, DynamicImage, RgbaImage,
    codecs::{gif::GifDecoder, webp::WebPDecoder},
    imageops::{self, FilterType},
};

use super::library::ImageKind;

/// Most frames kept of one image; the rest of a very long one is dropped.
const MAX_FRAMES: usize = 400;
/// Most decoded bytes kept of one image, whatever its frame count.
const MAX_BYTES: usize = 48 * 1024 * 1024;
/// What browsers use for a frame that asks for no delay, or next to none.
const DEFAULT_DELAY_MS: u32 = 100;

/// Decoded frames, still plain bytes so they can cross from the worker.
pub struct Decoded {
    frames: Vec<(RgbaImage, u32)>,
}

/// Frames ready to paint.
pub struct Thumb {
    frames: Vec<gdk::Texture>,
    /// When each frame ends, in milliseconds from the start of the loop.
    ends: Vec<u32>,
}

impl Thumb {
    fn from_decoded(decoded: Decoded) -> Option<Self> {
        let mut ends = Vec::with_capacity(decoded.frames.len());
        let mut elapsed = 0_u32;
        let frames = decoded
            .frames
            .into_iter()
            .map(|(image, delay)| {
                elapsed = elapsed.saturating_add(delay);
                ends.push(elapsed);
                let (width, height) = image.dimensions();
                gdk::MemoryTexture::new(
                    width as i32,
                    height as i32,
                    gdk::MemoryFormat::R8g8b8a8,
                    &glib::Bytes::from_owned(image.into_raw()),
                    width as usize * 4,
                )
                .upcast()
            })
            .collect::<Vec<gdk::Texture>>();
        (!frames.is_empty()).then_some(Self { frames, ends })
    }

    pub fn first(&self) -> &gdk::Texture {
        &self.frames[0]
    }

    pub fn is_animated(&self) -> bool {
        self.frames.len() > 1
    }

    /// The frame showing `ms` milliseconds into playback.
    pub fn frame_at(&self, ms: u64) -> usize {
        let total = u64::from(*self.ends.last().unwrap_or(&0));
        if total == 0 {
            return 0;
        }
        let into_loop = (ms % total) as u32;
        self.ends
            .partition_point(|end| *end <= into_loop)
            .min(self.frames.len() - 1)
    }

    pub fn frame(&self, index: usize) -> &gdk::Texture {
        &self.frames[index.min(self.frames.len() - 1)]
    }
}

/// Decodes `bytes` into frames no larger than `bounds`. Blocking.
pub fn decode(bytes: &[u8], bounds: (u32, u32)) -> Result<Decoded, String> {
    let kind = ImageKind::sniff(bytes).ok_or("not an image this can show")?;
    let error = |error: image::ImageError| error.to_string();

    let frames = match kind {
        ImageKind::Gif => {
            let decoder = GifDecoder::new(Cursor::new(bytes)).map_err(error)?;
            collect_frames(decoder.into_frames(), bounds)
        }
        ImageKind::Webp => {
            let decoder = WebPDecoder::new(Cursor::new(bytes)).map_err(error)?;
            if decoder.has_animation() {
                collect_frames(decoder.into_frames(), bounds)
            } else {
                vec![still(
                    DynamicImage::from_decoder(decoder).map_err(error)?,
                    bounds,
                )]
            }
        }
        ImageKind::Png | ImageKind::Jpeg => {
            vec![still(
                image::load_from_memory(bytes).map_err(error)?,
                bounds,
            )]
        }
    };

    if frames.is_empty() {
        return Err("the image has no frames".to_string());
    }
    Ok(Decoded { frames })
}

fn still(image: DynamicImage, bounds: (u32, u32)) -> (RgbaImage, u32) {
    (fit(&image.to_rgba8(), bounds), 0)
}

fn collect_frames(frames: image::Frames<'_>, bounds: (u32, u32)) -> Vec<(RgbaImage, u32)> {
    let mut kept = Vec::new();
    let mut bytes = 0_usize;
    for frame in frames.take(MAX_FRAMES) {
        // A frame that fails to decode ends the animation where it is; the
        // frames before it still play.
        let Ok(frame) = frame else { break };
        let (numerator, denominator) = frame.delay().numer_denom_ms();
        let delay = numerator.checked_div(denominator).unwrap_or(0);
        let delay = if delay < 20 { DEFAULT_DELAY_MS } else { delay };
        let image = fit(frame.buffer(), bounds);
        bytes += image.as_raw().len();
        kept.push((image, delay));
        if bytes > MAX_BYTES {
            break;
        }
    }
    kept
}

/// Scaled down to fit `bounds`, keeping its shape. Never scaled up.
fn fit(image: &RgbaImage, (max_width, max_height): (u32, u32)) -> RgbaImage {
    let (width, height) = image.dimensions();
    let scale = (f64::from(max_width) / f64::from(width))
        .min(f64::from(max_height) / f64::from(height))
        .min(1.0);
    if scale >= 1.0 {
        return image.clone();
    }
    let width = ((f64::from(width) * scale).round() as u32).max(1);
    let height = ((f64::from(height) * scale).round() as u32).max(1);
    imageops::resize(image, width, height, FilterType::Triangle)
}

/// Decodes an image that is not a file yet, such as one on the clipboard.
pub async fn decode_bytes(bytes: Vec<u8>, bounds: (u32, u32)) -> Option<Rc<Thumb>> {
    let decoded = gio::spawn_blocking(move || decode(&bytes, bounds))
        .await
        .ok()?
        .ok()?;
    Thumb::from_decoded(decoded).map(Rc::new)
}

type Waiter = Box<dyn FnOnce(Option<Rc<Thumb>>)>;

/// Decoded thumbnails by path, decoded once each for the life of the menu.
pub struct Thumbs {
    bounds: (u32, u32),
    ready: RefCell<HashMap<PathBuf, Option<Rc<Thumb>>>>,
    waiting: RefCell<HashMap<PathBuf, Vec<Waiter>>>,
}

impl Thumbs {
    pub fn new(bounds: (u32, u32)) -> Rc<Self> {
        Rc::new(Self {
            bounds,
            ready: RefCell::new(HashMap::new()),
            waiting: RefCell::new(HashMap::new()),
        })
    }

    /// Calls `done` with the thumbnail for `path`, right away when it is
    /// decoded already, otherwise once it is. `None` when it cannot be read.
    pub fn get(self: &Rc<Self>, path: &Path, done: impl FnOnce(Option<Rc<Thumb>>) + 'static) {
        if let Some(thumb) = self.ready.borrow().get(path) {
            done(thumb.clone());
            return;
        }
        let first = {
            let mut waiting = self.waiting.borrow_mut();
            let waiters = waiting.entry(path.to_path_buf()).or_default();
            waiters.push(Box::new(done));
            waiters.len() == 1
        };
        if !first {
            return;
        }

        let this = Rc::clone(self);
        let path = path.to_path_buf();
        let bounds = self.bounds;
        glib::spawn_future_local(async move {
            let worker_path = path.clone();
            let decoded = gio::spawn_blocking(move || {
                let bytes = std::fs::read(&worker_path).map_err(|error| error.to_string())?;
                decode(&bytes, bounds)
            })
            .await
            .unwrap_or_else(|_| Err("the decoder stopped".to_string()));
            let thumb = match decoded {
                Ok(decoded) => Thumb::from_decoded(decoded).map(Rc::new),
                Err(error) => {
                    tracing::info!(path = %path.display(), %error, "cannot show the image");
                    None
                }
            };
            this.ready.borrow_mut().insert(path.clone(), thumb.clone());
            let waiters = this.waiting.borrow_mut().remove(&path).unwrap_or_default();
            for waiter in waiters {
                waiter(thumb.clone());
            }
        });
    }

    /// Forgets `path`, after it was replaced or removed.
    pub fn forget(&self, path: &Path) {
        self.ready.borrow_mut().remove(path);
    }
}

/// Plays a thumbnail in a `Picture` for as long as the picture is on screen.
pub struct Player {
    picture: gtk::Picture,
    thumb: RefCell<Option<Rc<Thumb>>>,
    shown: Cell<usize>,
}

impl Player {
    pub fn new(picture: gtk::Picture) -> Rc<Self> {
        let player = Rc::new(Self {
            picture: picture.clone(),
            thumb: RefCell::new(None),
            shown: Cell::new(0),
        });
        let weak = Rc::downgrade(&player);
        picture.add_tick_callback(move |_, clock| {
            let Some(player) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            player.tick(clock.frame_time());
            glib::ControlFlow::Continue
        });
        player
    }

    pub fn show(&self, thumb: Option<Rc<Thumb>>) {
        self.picture
            .set_paintable(thumb.as_ref().map(|thumb| thumb.first()));
        self.shown.set(0);
        *self.thumb.borrow_mut() = thumb;
    }

    fn tick(&self, frame_time_us: i64) {
        let thumb = self.thumb.borrow();
        let Some(thumb) = thumb.as_ref().filter(|thumb| thumb.is_animated()) else {
            return;
        };
        // Every animation runs off the same clock, which keeps cells that show
        // the same image in step.
        let index = thumb.frame_at((frame_time_us / 1000).max(0) as u64);
        if index != self.shown.get() {
            self.shown.set(index);
            self.picture.set_paintable(Some(thumb.frame(index)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Delay, Frame, codecs::gif::GifEncoder};

    fn animated_gif(frames: u32, size: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut bytes);
            for index in 0..frames {
                let shade = (index * 60 % 255) as u8;
                let image = RgbaImage::from_pixel(size, size, image::Rgba([shade, 0, 0, 255]));
                encoder
                    .encode_frame(Frame::from_parts(
                        image,
                        0,
                        0,
                        Delay::from_numer_denom_ms(50, 1),
                    ))
                    .unwrap();
            }
        }
        bytes
    }

    #[test]
    fn frames_are_decoded_scaled_and_timed() {
        let decoded = decode(&animated_gif(3, 200), (100, 80)).unwrap();
        assert_eq!(decoded.frames.len(), 3);
        for (image, delay) in &decoded.frames {
            assert_eq!(image.dimensions(), (80, 80));
            assert_eq!(*delay, 50);
        }
    }

    #[test]
    fn small_images_are_not_scaled_up() {
        let decoded = decode(&animated_gif(1, 16), (100, 80)).unwrap();
        assert_eq!(decoded.frames[0].0.dimensions(), (16, 16));
    }

    #[test]
    fn not_an_image_is_an_error() {
        assert!(decode(b"<html>", (10, 10)).is_err());
    }
}
