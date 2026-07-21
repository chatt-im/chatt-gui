use std::{fs, future::Future, sync::Arc};

use anyhow::anyhow;
use gpui::{App, Asset, ImageCacheError, RenderImage, Resource};
use image::{DynamicImage, Frame, RgbaImage};

const MAX_THUMBNAIL_WIDTH: u32 = 1_360;
const MAX_THUMBNAIL_HEIGHT: u32 = 840;

/// Decodes timeline media to at most twice its largest on-screen size. Animated
/// formats intentionally become a single-frame thumbnail.
#[derive(Clone)]
pub enum TimelineImageLoader {}

#[derive(Clone)]
pub enum PreviewImageLoader {}

impl Asset for TimelineImageLoader {
    type Source = Resource;
    type Output = Result<Arc<RenderImage>, ImageCacheError>;

    fn load(
        source: Self::Source,
        cx: &mut App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        let svg_renderer = cx.svg_renderer();
        async move {
            let Resource::Path(path) = source else {
                return Err(anyhow!("timeline images must be local files").into());
            };
            log::info!("timeline image decode started path={path:?}");
            let bytes = fs::read(path.as_ref())?;
            let image = if image::guess_format(&bytes).is_ok() {
                let image = image::load_from_memory(&bytes)?;
                thumbnail_from_image(image, MAX_THUMBNAIL_WIDTH, MAX_THUMBNAIL_HEIGHT)
            } else {
                let image = svg_renderer.render_single_frame(&bytes, 1.0)?;
                downsample_render_image(image, MAX_THUMBNAIL_WIDTH, MAX_THUMBNAIL_HEIGHT)?
            };

            log::info!(
                "timeline image decode finished path={path:?} size={:?}",
                image.size(0),
            );
            Ok(image)
        }
    }
}

impl Asset for PreviewImageLoader {
    type Source = Resource;
    type Output = Result<Arc<RenderImage>, ImageCacheError>;

    fn load(
        source: Self::Source,
        cx: &mut App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        let svg_renderer = cx.svg_renderer();
        async move {
            let Resource::Path(path) = source else {
                return Err(anyhow!("preview images must be local files").into());
            };
            let bytes = fs::read(path.as_ref())?;
            if image::guess_format(&bytes).is_ok() {
                return Ok(render_image_from_dynamic(image::load_from_memory(&bytes)?));
            }
            Ok(svg_renderer.render_single_frame(&bytes, 1.0)?)
        }
    }
}

fn thumbnail_from_image(image: DynamicImage, max_width: u32, max_height: u32) -> Arc<RenderImage> {
    let mut buffer = image.thumbnail(max_width, max_height).into_rgba8();
    rgba_to_bgra(&mut buffer);
    Arc::new(RenderImage::new(vec![Frame::new(buffer)]))
}

fn render_image_from_dynamic(image: DynamicImage) -> Arc<RenderImage> {
    let mut buffer = image.into_rgba8();
    rgba_to_bgra(&mut buffer);
    Arc::new(RenderImage::new(vec![Frame::new(buffer)]))
}

fn downsample_render_image(
    image: Arc<RenderImage>,
    max_width: u32,
    max_height: u32,
) -> Result<Arc<RenderImage>, ImageCacheError> {
    let size = image.size(0);
    let width = u32::try_from(size.width.0).unwrap_or_default();
    let height = u32::try_from(size.height.0).unwrap_or_default();
    if width <= max_width && height <= max_height {
        return Ok(image);
    }

    let Some(bytes) = image.as_bytes(0) else {
        return Ok(image);
    };
    let mut pixels = bytes.to_vec();
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    let Some(buffer) = RgbaImage::from_raw(width, height, pixels) else {
        return Err(anyhow!("decoded SVG has an invalid pixel buffer").into());
    };
    Ok(thumbnail_from_image(
        DynamicImage::ImageRgba8(buffer),
        max_width,
        max_height,
    ))
}

fn rgba_to_bgra(buffer: &mut RgbaImage) {
    for pixel in buffer.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
}

#[cfg(test)]
mod tests {
    use image::{Rgba, RgbaImage};

    use super::*;

    #[test]
    fn thumbnail_is_bounded_and_converted_to_bgra() {
        let input = RgbaImage::from_pixel(40, 20, Rgba([255, 0, 0, 255]));
        let thumbnail = thumbnail_from_image(DynamicImage::ImageRgba8(input), 10, 10);

        assert_eq!(thumbnail.size(0).width.0, 10);
        assert_eq!(thumbnail.size(0).height.0, 5);
        assert_eq!(&thumbnail.as_bytes(0).unwrap()[..4], &[0, 0, 255, 255]);
    }
}
