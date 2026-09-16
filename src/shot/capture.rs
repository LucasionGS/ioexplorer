//! Freezing the screens, and turning a frozen frame into a finished image.
//!
//! The freeze is a real capture of every output, taken before anything of ours
//! is mapped. Everything afterwards — the overlay, the selection, the final
//! crop — works from those images rather than grabbing the screen again, so the
//! shot is exactly the moment the key was pressed and can never contain the
//! overlay itself.
//!
//! Capture goes through `grim` (`wlr-screencopy`), one process per output in
//! parallel. Per output rather than one full-layout grab, because a layout grab
//! is rendered at the *largest* scale present, which up-samples every low-DPI
//! screen; per output, each image keeps its native pixels and composition only
//! resamples when a selection genuinely spans two densities.

use std::{
    io::Read,
    process::{Command, Stdio},
    thread,
};

use gtk::{cairo, gdk, glib, prelude::*};

use super::{geometry::Rect, tools::Annotation};

/// One output, frozen.
#[derive(Clone, Debug)]
pub struct FrozenOutput {
    /// Connector name, e.g. `DP-1`.
    pub name: String,
    /// Global logical geometry, as the compositor lays it out.
    pub rect: Rect,
    pub image: gdk::Texture,
}

impl FrozenOutput {
    /// Device pixels per logical pixel, measured from the image rather than
    /// asked for. That is the only number that is certainly right: it already
    /// accounts for fractional scaling and rotation, which a reported scale
    /// would each have to be corrected for.
    pub fn pixel_scale(&self) -> f64 {
        if self.rect.width <= 0.0 {
            return 1.0;
        }
        f64::from(self.image.width()) / self.rect.width
    }
}

/// An output as GDK describes it, before capture.
#[derive(Clone, Debug)]
pub struct OutputInfo {
    pub name: String,
    pub rect: Rect,
}

/// The live outputs, in enumeration order.
pub fn outputs(display: &gdk::Display) -> Vec<OutputInfo> {
    display
        .monitors()
        .iter::<gdk::Monitor>()
        .flatten()
        .enumerate()
        .map(|(index, monitor)| {
            let geometry = monitor.geometry();
            OutputInfo {
                name: monitor
                    .connector()
                    .map(|connector| connector.to_string())
                    .unwrap_or_else(|| format!("output-{index}")),
                rect: Rect::new(
                    f64::from(geometry.x()),
                    f64::from(geometry.y()),
                    f64::from(geometry.width()),
                    f64::from(geometry.height()),
                ),
            }
        })
        .collect()
}

/// Captures every output in parallel.
///
/// All or nothing: a frame missing one screen cannot be told apart from a black
/// screen once it is on the overlay, so a single failure fails the freeze.
pub fn freeze(outputs: &[OutputInfo]) -> Result<Vec<FrozenOutput>, String> {
    if outputs.is_empty() {
        return Err("no outputs to capture".to_string());
    }

    let workers: Vec<_> = outputs
        .iter()
        .map(|output| {
            let name = output.name.clone();
            thread::spawn(move || grim(&name))
        })
        .collect();

    let mut frozen = Vec::with_capacity(outputs.len());
    for (output, worker) in outputs.iter().zip(workers) {
        let bytes = worker
            .join()
            .map_err(|_| format!("capture of {} panicked", output.name))??;
        let image = gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes))
            .map_err(|error| format!("cannot decode the capture of {}: {error}", output.name))?;
        frozen.push(FrozenOutput {
            name: output.name.clone(),
            rect: output.rect,
            image,
        });
    }

    Ok(frozen)
}

/// Runs `grim` for one output, returning the PNG it wrote.
///
/// Compression level 0: the PNG only lives long enough to be decoded, so
/// spending CPU to shrink it would just make the freeze slower.
fn grim(output: &str) -> Result<Vec<u8>, String> {
    let mut child = Command::new("grim")
        .args(["-l", "0", "-o", output, "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => {
                "grim is not installed; ioexplorer-shot needs it to capture the screen".to_string()
            }
            _ => format!("cannot run grim: {error}"),
        })?;

    let mut bytes = Vec::new();
    if let Some(mut stdout) = child.stdout.take() {
        stdout
            .read_to_end(&mut bytes)
            .map_err(|error| format!("cannot read grim's output: {error}"))?;
    }
    let status = child
        .wait_with_output()
        .map_err(|error| format!("grim did not finish: {error}"))?;

    if !status.status.success() || bytes.is_empty() {
        return Err(format!(
            "grim could not capture {output}: {}",
            String::from_utf8_lossy(&status.stderr).trim()
        ));
    }
    Ok(bytes)
}

/// Renders `area` of the frozen layout, with annotations drawn over it.
///
/// The image is rendered at the highest pixel density among the outputs the
/// area touches, so a crop of a single screen is pixel-for-pixel what that
/// screen showed. Any part of `area` no output covers — the gaps in an
/// irregular multi-monitor layout — stays transparent.
pub fn compose(
    outputs: &[FrozenOutput],
    area: Rect,
    annotations: &[&dyn Annotation],
) -> Result<gdk::Texture, String> {
    let area = area.snapped_out();
    let touched: Vec<&FrozenOutput> = outputs
        .iter()
        .filter(|output| output.rect.intersects(&area))
        .collect();
    if area.is_empty() || touched.is_empty() {
        return Err("the selection does not cover any screen".to_string());
    }

    let scale = touched
        .iter()
        .map(|output| output.pixel_scale())
        .fold(1.0_f64, f64::max);
    let width = (area.width * scale).round().max(1.0) as i32;
    let height = (area.height * scale).round().max(1.0) as i32;

    let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, width, height)
        .map_err(|error| format!("cannot allocate a {width}×{height} image: {error}"))?;

    {
        let cr = cairo::Context::new(&surface).map_err(|error| error.to_string())?;
        cr.scale(scale, scale);
        cr.translate(-area.x, -area.y);

        for output in &touched {
            let source = texture_to_surface(&output.image)?;
            let factor = 1.0 / output.pixel_scale();

            cr.save().map_err(|error| error.to_string())?;
            cr.rectangle(
                output.rect.x,
                output.rect.y,
                output.rect.width,
                output.rect.height,
            );
            cr.clip();
            cr.translate(output.rect.x, output.rect.y);
            cr.scale(factor, factor);
            cr.set_source_surface(&source, 0.0, 0.0)
                .map_err(|error| error.to_string())?;
            // One device pixel per output pixel: nearest keeps it bit-exact,
            // where any smoothing filter would soften the whole shot.
            if (output.pixel_scale() - scale).abs() < f64::EPSILON {
                cr.source().set_filter(cairo::Filter::Nearest);
            } else {
                cr.source().set_filter(cairo::Filter::Good);
            }
            cr.paint().map_err(|error| error.to_string())?;
            cr.restore().map_err(|error| error.to_string())?;
        }

        for annotation in annotations {
            if annotation.bounds().intersects(&area) {
                cr.save().map_err(|error| error.to_string())?;
                annotation.draw(&cr);
                cr.restore().map_err(|error| error.to_string())?;
            }
        }
    }

    surface.flush();
    let stride = surface.stride() as usize;
    let data = surface
        .data()
        .map_err(|error| format!("cannot read the rendered image: {error}"))?
        .to_vec();

    Ok(gdk::MemoryTexture::new(
        width,
        height,
        CAIRO_MEMORY_FORMAT,
        &glib::Bytes::from_owned(data),
        stride,
    )
    .upcast())
}

/// Cairo's `ARGB32` is a native-endian `0xAARRGGBB` word, which is a different
/// byte order depending on the machine.
#[cfg(target_endian = "little")]
const CAIRO_MEMORY_FORMAT: gdk::MemoryFormat = gdk::MemoryFormat::B8g8r8a8Premultiplied;
#[cfg(target_endian = "big")]
const CAIRO_MEMORY_FORMAT: gdk::MemoryFormat = gdk::MemoryFormat::A8r8g8b8Premultiplied;

fn texture_to_surface(texture: &gdk::Texture) -> Result<cairo::ImageSurface, String> {
    let width = texture.width();
    let height = texture.height();
    let stride = cairo::Format::ARgb32
        .stride_for_width(width as u32)
        .map_err(|error| error.to_string())?;
    let mut data = vec![0_u8; stride as usize * height as usize];
    // `download` writes exactly cairo's ARGB32 layout.
    texture.download(&mut data, stride as usize);
    cairo::ImageSurface::create_for_data(data, cairo::Format::ARgb32, width, height, stride)
        .map_err(|error| format!("cannot wrap a captured image: {error}"))
}

/// Encodes a finished shot as PNG.
///
/// Through gdk-pixbuf rather than `gdk::Texture::save_to_png_bytes`, because
/// only this path takes a compression level, and the level is most of the
/// cost: at zlib's default a full multi-monitor layout takes about a second to
/// encode and at level 1 about an eighth of that, for a file a quarter larger —
/// screenshots are mostly flat colour, which even light compression handles
/// well.
pub fn encode_png(texture: &gdk::Texture) -> Result<Vec<u8>, String> {
    let mut downloader = gdk::TextureDownloader::new(texture);
    // Straight alpha, which is what a pixbuf holds.
    downloader.set_format(gdk::MemoryFormat::R8g8b8a8);
    let (bytes, stride) = downloader.download_bytes();

    let pixbuf = gdk_pixbuf::Pixbuf::from_bytes(
        &bytes,
        gdk_pixbuf::Colorspace::Rgb,
        true,
        8,
        texture.width(),
        texture.height(),
        stride as i32,
    );
    pixbuf
        .save_to_bufferv("png", &[("compression", PNG_COMPRESSION)])
        .map_err(|error| format!("cannot encode the image: {error}"))
}

/// zlib level for saved shots.
const PNG_COMPRESSION: &str = "1";

#[cfg(test)]
mod tests {
    use super::*;

    /// A solid texture of one colour, `width`×`height` device pixels.
    fn solid(width: i32, height: i32, rgba: [u8; 4]) -> gdk::Texture {
        let data: Vec<u8> = (0..width * height).flat_map(|_| rgba).collect();
        gdk::MemoryTexture::new(
            width,
            height,
            gdk::MemoryFormat::R8g8b8a8,
            &glib::Bytes::from_owned(data),
            width as usize * 4,
        )
        .upcast()
    }

    fn output(name: &str, rect: Rect, image: gdk::Texture) -> FrozenOutput {
        FrozenOutput {
            name: name.to_string(),
            rect,
            image,
        }
    }

    /// Reads one pixel back as straight RGBA.
    fn pixel(texture: &gdk::Texture, x: i32, y: i32) -> [u8; 4] {
        let mut downloader = gdk::TextureDownloader::new(texture);
        downloader.set_format(gdk::MemoryFormat::R8g8b8a8);
        let (bytes, stride) = downloader.download_bytes();
        let offset = y as usize * stride + x as usize * 4;
        [
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ]
    }

    #[test]
    fn a_crop_of_one_screen_keeps_its_native_resolution() {
        let hidpi = output(
            "eDP-1",
            Rect::new(0.0, 0.0, 100.0, 50.0),
            solid(200, 100, [255, 0, 0, 255]),
        );

        let shot = compose(&[hidpi], Rect::new(10.0, 10.0, 20.0, 20.0), &[]).expect("a shot");

        assert_eq!((shot.width(), shot.height()), (40, 40));
        assert_eq!(pixel(&shot, 5, 5), [255, 0, 0, 255]);
    }

    #[test]
    fn a_span_across_screens_takes_both_and_leaves_gaps_transparent() {
        let left = output(
            "DP-1",
            Rect::new(0.0, 0.0, 10.0, 10.0),
            solid(10, 10, [255, 0, 0, 255]),
        );
        // Offset downwards, so the band's top-right corner covers no screen.
        let right = output(
            "DP-2",
            Rect::new(10.0, 5.0, 10.0, 10.0),
            solid(10, 10, [0, 0, 255, 255]),
        );

        let shot = compose(&[left, right], Rect::new(0.0, 0.0, 20.0, 10.0), &[]).expect("a shot");

        assert_eq!((shot.width(), shot.height()), (20, 10));
        assert_eq!(pixel(&shot, 2, 2), [255, 0, 0, 255]);
        assert_eq!(pixel(&shot, 15, 8), [0, 0, 255, 255]);
        assert_eq!(pixel(&shot, 15, 2)[3], 0, "the gap is transparent");
    }

    #[test]
    fn a_selection_off_every_screen_is_refused() {
        let only = output(
            "DP-1",
            Rect::new(0.0, 0.0, 10.0, 10.0),
            solid(10, 10, [0, 0, 0, 255]),
        );

        assert!(compose(&[only], Rect::new(50.0, 50.0, 10.0, 10.0), &[]).is_err());
    }

    #[test]
    fn the_png_round_trips() {
        let texture = solid(3, 2, [1, 2, 3, 255]);
        let png = encode_png(&texture).expect("encodes");

        assert!(png.starts_with(b"\x89PNG"));
        let decoded = gdk::Texture::from_bytes(&glib::Bytes::from_owned(png)).expect("decodes");
        assert_eq!((decoded.width(), decoded.height()), (3, 2));
    }
}
