use crate::{
    App, Bounds, DevicePixels, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, ObjectFit, Pixels, Size, Style, StyleRefinement, Styled, Window,
};
#[cfg(target_os = "macos")]
use core_video::pixel_buffer::CVPixelBuffer;
use refineable::Refineable;
use std::{any::Any, fmt, sync::Arc};

/// Platform renderer-owned content that can be painted by a [`Surface`].
pub trait PlatformSurfaceSource: Any + Send + Sync {
    /// The most recently allocated backing texture size.
    fn size(&self) -> Size<DevicePixels>;

    /// Report the exact device-pixel size needed for the next backing texture.
    fn request_size(&self, size: Size<DevicePixels>);

    /// Access the concrete renderer source.
    fn as_any(&self) -> &dyn Any;
}

/// A type-erased, renderer-owned surface source.
#[derive(Clone)]
pub struct PlatformSurface(pub Arc<dyn PlatformSurfaceSource>);

impl fmt::Debug for PlatformSurface {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("PlatformSurface").finish()
    }
}

impl PartialEq for PlatformSurface {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for PlatformSurface {}

/// A source of a surface's content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SurfaceSource {
    /// A macOS image buffer from CoreVideo
    #[cfg(target_os = "macos")]
    Surface(CVPixelBuffer),
    /// A persistent texture managed by the active platform renderer.
    Platform(PlatformSurface),
}

impl From<PlatformSurface> for SurfaceSource {
    fn from(value: PlatformSurface) -> Self {
        SurfaceSource::Platform(value)
    }
}

#[cfg(target_os = "macos")]
impl From<CVPixelBuffer> for SurfaceSource {
    fn from(value: CVPixelBuffer) -> Self {
        SurfaceSource::Surface(value)
    }
}

/// A surface element.
pub struct Surface {
    source: SurfaceSource,
    object_fit: ObjectFit,
    style: StyleRefinement,
}

/// Create a new surface element.
pub fn surface(source: impl Into<SurfaceSource>) -> Surface {
    Surface {
        source: source.into(),
        object_fit: ObjectFit::Contain,
        style: Default::default(),
    }
}

impl Surface {
    /// Set the object fit for the image.
    pub fn object_fit(mut self, object_fit: ObjectFit) -> Self {
        self.object_fit = object_fit;
        self
    }
}

impl Element for Surface {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.refine(&self.style);
        let layout_id = window.request_layout(style, [], cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        #[cfg_attr(not(target_os = "macos"), allow(unused_variables))] bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        #[cfg_attr(not(target_os = "macos"), allow(unused_variables))] window: &mut Window,
        _: &mut App,
    ) {
        match &self.source {
            #[cfg(target_os = "macos")]
            SurfaceSource::Surface(surface) => {
                let size = crate::size(surface.get_width().into(), surface.get_height().into());
                let new_bounds = self.object_fit.get_bounds(bounds, size);
                // TODO: Add support for corner_radii
                window.paint_surface(new_bounds, surface.clone());
            }
            SurfaceSource::Platform(surface) => {
                #[cfg(not(target_os = "macos"))]
                {
                    let fitted_bounds = self.object_fit.get_bounds(bounds, surface.0.size());
                    window.paint_platform_surface(fitted_bounds, surface.clone());
                }
            }
        }
    }
}

impl IntoElement for Surface {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Styled for Surface {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}
