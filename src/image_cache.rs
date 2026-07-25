use std::{future::Future, sync::Arc};

use anyhow::anyhow;
use gpui::{App, Asset, ImageCacheError, RenderImage, SvgRenderer};
use image::{DynamicImage, Frame, RgbaImage};

use crate::media_cache::CachedAttachment;

const MAX_THUMBNAIL_WIDTH: u32 = 1_360;
const MAX_THUMBNAIL_HEIGHT: u32 = 840;

/// Decodes timeline media to at most twice its largest on-screen size. Animated
/// formats intentionally become a single-frame thumbnail.
#[derive(Clone)]
pub enum TimelineImageLoader {}

#[derive(Clone)]
pub enum PreviewImageLoader {}

impl Asset for TimelineImageLoader {
    type Source = CachedAttachment;
    type Output = Result<Arc<RenderImage>, ImageCacheError>;

    fn load(
        source: Self::Source,
        cx: &mut App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        let svg_renderer = cx.svg_renderer();
        async move {
            log::info!(
                "timeline image decode started attachment={:?} source_bytes={} empty={}",
                source.id(),
                source.len(),
                source.is_empty(),
            );
            let image = decode_timeline_attachment(&source, &svg_renderer)?;

            log::info!(
                "timeline image decode finished attachment={:?} source_bytes={} render_image_id={} size={:?}",
                source.id(),
                source.len(),
                image.id.0,
                image.size(0),
            );
            Ok(image)
        }
    }
}

impl Asset for PreviewImageLoader {
    type Source = CachedAttachment;
    type Output = Result<Arc<RenderImage>, ImageCacheError>;

    fn load(
        source: Self::Source,
        cx: &mut App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        let svg_renderer = cx.svg_renderer();
        async move {
            let image = decode_preview_attachment(&source, &svg_renderer)?;
            log::info!(
                "preview image decode finished attachment={:?} source_bytes={} render_image_id={} size={:?}",
                source.id(),
                source.len(),
                image.id.0,
                image.size(0),
            );
            Ok(image)
        }
    }
}

fn decode_timeline_attachment(
    source: &CachedAttachment,
    svg_renderer: &SvgRenderer,
) -> Result<Arc<RenderImage>, ImageCacheError> {
    let bytes = source.bytes();
    if image::guess_format(bytes).is_ok() {
        let image = image::load_from_memory(bytes)?;
        Ok(thumbnail_from_image(
            image,
            MAX_THUMBNAIL_WIDTH,
            MAX_THUMBNAIL_HEIGHT,
        ))
    } else {
        let image = svg_renderer.render_single_frame(bytes, 1.0)?;
        downsample_render_image(image, MAX_THUMBNAIL_WIDTH, MAX_THUMBNAIL_HEIGHT)
    }
}

fn decode_preview_attachment(
    source: &CachedAttachment,
    svg_renderer: &SvgRenderer,
) -> Result<Arc<RenderImage>, ImageCacheError> {
    let bytes = source.bytes();
    if image::guess_format(bytes).is_ok() {
        Ok(render_image_from_dynamic(image::load_from_memory(bytes)?))
    } else {
        Ok(svg_renderer.render_single_frame(bytes, 1.0)?)
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
    use std::{
        hash::{DefaultHasher, Hash, Hasher},
        io::Cursor,
    };

    use gpui::SvgRenderer;
    use image::{ImageFormat, Rgba, RgbaImage};
    use local_rpc::{
        bulk::BulkFinished,
        ids::FileTransferId,
        model::{AttachmentDescriptor, AttachmentId, BulkTransferId, MediaKind},
    };

    use super::*;
    use crate::media_cache::MediaCache;

    fn cached_attachment(bytes: &[u8]) -> CachedAttachment {
        let descriptor = AttachmentDescriptor {
            id: AttachmentId {
                timestamp_ms: 1,
                transfer_id: FileTransferId(1),
            },
            file_name: "image.png".into(),
            media_kind: MediaKind::Image,
            content_type: "image/png".into(),
            byte_len: bytes.len() as u64,
            width: None,
            height: None,
        };
        let mut cache = MediaCache::new(bytes.len() as u64);
        cache.reserve(BulkTransferId(1), &descriptor).unwrap();
        cache.chunk(BulkTransferId(1), bytes).unwrap();
        cache
            .finish(BulkFinished {
                transfer_id: BulkTransferId(1),
            })
            .unwrap();
        cache.get(descriptor.id).unwrap()
    }

    fn png(width: u32, height: u32, color: Rgba<u8>) -> Vec<u8> {
        let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(width, height, color));
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        bytes
    }

    #[test]
    fn raster_decodes_from_cached_bytes_with_timeline_downsampling_and_bgra_conversion() {
        let source = cached_attachment(&png(1_600, 1_000, Rgba([255, 0, 0, 255])));
        let thumbnail =
            decode_timeline_attachment(&source, &SvgRenderer::new(Arc::new(()))).unwrap();

        assert_eq!(thumbnail.size(0).width.0, 1_344);
        assert_eq!(thumbnail.size(0).height.0, 840);
        assert_eq!(&thumbnail.as_bytes(0).unwrap()[..4], &[0, 0, 255, 255]);
    }

    #[test]
    fn svg_decodes_from_cached_bytes() {
        let source = cached_attachment(
            br#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10"><rect width="20" height="10" fill="red"/></svg>"#,
        );
        let image = decode_timeline_attachment(&source, &SvgRenderer::new(Arc::new(()))).unwrap();
        assert_eq!(image.size(0).width.0, 40);
        assert_eq!(image.size(0).height.0, 20);
    }

    #[test]
    fn preview_decodes_raster_at_natural_resolution() {
        let source = cached_attachment(&png(37, 19, Rgba([1, 2, 3, 255])));
        let image = decode_preview_attachment(&source, &SvgRenderer::new(Arc::new(()))).unwrap();
        assert_eq!(image.size(0).width.0, 37);
        assert_eq!(image.size(0).height.0, 19);
        assert_eq!(&image.as_bytes(0).unwrap()[..4], &[3, 2, 1, 255]);
    }

    #[test]
    fn malformed_cached_bytes_return_an_image_cache_error() {
        let source = cached_attachment(b"not an image");
        assert!(decode_preview_attachment(&source, &SvgRenderer::new(Arc::new(()))).is_err());
    }

    #[test]
    fn source_identity_distinguishes_revisions_of_one_attachment_id() {
        let descriptor = AttachmentDescriptor {
            id: AttachmentId {
                timestamp_ms: 1,
                transfer_id: FileTransferId(1),
            },
            file_name: "image.png".into(),
            media_kind: MediaKind::Image,
            content_type: "image/png".into(),
            byte_len: 3,
            width: None,
            height: None,
        };
        let mut cache = MediaCache::new(3);
        cache.reserve(BulkTransferId(1), &descriptor).unwrap();
        cache.chunk(BulkTransferId(1), b"one").unwrap();
        cache
            .finish(BulkFinished {
                transfer_id: BulkTransferId(1),
            })
            .unwrap();
        let first = cache.get(descriptor.id).unwrap();
        cache.clear();
        cache.reserve(BulkTransferId(2), &descriptor).unwrap();
        cache.chunk(BulkTransferId(2), b"two").unwrap();
        cache
            .finish(BulkFinished {
                transfer_id: BulkTransferId(2),
            })
            .unwrap();
        let second = cache.get(descriptor.id).unwrap();

        let hash = |source: &CachedAttachment| {
            let mut hasher = DefaultHasher::new();
            source.hash(&mut hasher);
            hasher.finish()
        };
        assert_ne!(hash(&first), hash(&second));
        assert_eq!(hash(&first), hash(&first.clone()));
    }
}
