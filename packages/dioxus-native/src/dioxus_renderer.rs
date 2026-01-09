use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use anyrender::WindowRenderer;

#[cfg(any(
    feature = "vello",
    all(
        not(feature = "alt-renderer"),
        not(all(target_os = "ios", target_abi = "sim"))
    )
))]
pub use anyrender_vello::{
    wgpu::{Features, Limits},
    CustomPaintSource, VelloRendererOptions, VelloWindowRenderer as InnerRenderer,
};

#[cfg(any(
    feature = "vello-cpu-base",
    all(
        not(feature = "alt-renderer"),
        all(target_os = "ios", target_abi = "sim")
    )
))]
use anyrender_vello_cpu::VelloCpuWindowRenderer as InnerRenderer;

#[cfg(feature = "vello-hybrid")]
use anyrender_vello_hybrid::VelloHybridWindowRenderer as InnerRenderer;

#[cfg(feature = "skia")]
use anyrender_skia::SkiaWindowRenderer as InnerRenderer;

#[cfg(any(
    feature = "vello",
    all(
        not(feature = "alt-renderer"),
        not(all(target_os = "ios", target_abi = "sim"))
    )
))]
pub fn use_wgpu<T: CustomPaintSource>(create_source: impl FnOnce() -> T) -> u64 {
    use dioxus_core::{consume_context, use_hook_with_cleanup};

    let (_renderer, id) = use_hook_with_cleanup(
        || {
            let renderer = consume_context::<DioxusNativeWindowRenderer>();
            let source = Box::new(create_source());
            let id = renderer.register_custom_paint_source(source);
            (renderer, id)
        },
        |(renderer, id)| {
            renderer.unregister_custom_paint_source(id);
        },
    );

    id
}

/// Hook to add a post-processor to the rendering pipeline.
///
/// Post-processors are applied after the main scene render but before
/// presentation to the screen. The post-processor is automatically removed
/// when the component is unmounted.
///
/// # Example
///
/// ```ignore
/// use dioxus_native::{use_post_processor, BoxBlurPostProcessor};
///
/// fn MyComponent() -> Element {
///     // Add a blur effect to the entire window
///     use_post_processor(|| BoxBlurPostProcessor::new(1.0));
///
///     rsx! { div { "Content with blur effect" } }
/// }
/// ```
#[cfg(any(
    feature = "vello",
    all(
        not(feature = "alt-renderer"),
        not(all(target_os = "ios", target_abi = "sim"))
    )
))]
pub fn use_post_processor<T: anyrender_vello::PostProcessor>(create_processor: impl FnOnce() -> T) {
    use dioxus_core::{consume_context, use_hook_with_cleanup};

    use_hook_with_cleanup(
        || {
            let renderer = consume_context::<DioxusNativeWindowRenderer>();
            let processor = Box::new(create_processor());
            renderer.add_post_processor(processor);
            renderer
        },
        |renderer| {
            // Note: Currently we can't remove individual post-processors,
            // so clearing all is the only option. This means only one component
            // should use post-processors, or they should all be added together.
            renderer.clear_post_processors();
        },
    );
}

#[derive(Clone)]
pub struct DioxusNativeWindowRenderer {
    inner: Rc<RefCell<InnerRenderer>>,
}

impl Default for DioxusNativeWindowRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl DioxusNativeWindowRenderer {
    pub fn new() -> Self {
        let vello_renderer = InnerRenderer::new();
        Self::with_inner_renderer(vello_renderer)
    }

    #[cfg(any(
        feature = "vello",
        all(
            not(feature = "alt-renderer"),
            not(all(target_os = "ios", target_abi = "sim"))
        )
    ))]
    pub fn with_features_and_limits(features: Option<Features>, limits: Option<Limits>) -> Self {
        let vello_renderer = InnerRenderer::with_options(VelloRendererOptions {
            features,
            limits,
            ..Default::default()
        });
        Self::with_inner_renderer(vello_renderer)
    }

    fn with_inner_renderer(vello_renderer: InnerRenderer) -> Self {
        Self {
            inner: Rc::new(RefCell::new(vello_renderer)),
        }
    }
}

#[cfg(any(
    feature = "vello",
    all(
        not(feature = "alt-renderer"),
        not(all(target_os = "ios", target_abi = "sim"))
    )
))]
impl DioxusNativeWindowRenderer {
    pub fn register_custom_paint_source(&self, source: Box<dyn CustomPaintSource>) -> u64 {
        self.inner.borrow_mut().register_custom_paint_source(source)
    }

    pub fn unregister_custom_paint_source(&self, id: u64) {
        self.inner.borrow_mut().unregister_custom_paint_source(id)
    }

    /// Add a post-processor to the rendering pipeline.
    ///
    /// Post-processors are applied after the main scene render but before
    /// presentation to the screen. Multiple post-processors are applied in
    /// the order they are added.
    pub fn add_post_processor(&self, processor: Box<dyn anyrender_vello::PostProcessor>) {
        self.inner.borrow_mut().add_post_processor(processor)
    }

    /// Clear all post-processors from the rendering pipeline.
    pub fn clear_post_processors(&self) {
        self.inner.borrow_mut().clear_post_processors()
    }

    /// Returns true if there are any active post-processors.
    pub fn has_post_processors(&self) -> bool {
        self.inner.borrow().has_post_processors()
    }
}

impl WindowRenderer for DioxusNativeWindowRenderer {
    type ScenePainter<'a>
        = <InnerRenderer as WindowRenderer>::ScenePainter<'a>
    where
        Self: 'a;

    fn resume(&mut self, window: Arc<dyn anyrender::WindowHandle>, width: u32, height: u32) {
        self.inner.borrow_mut().resume(window, width, height)
    }

    fn suspend(&mut self) {
        self.inner.borrow_mut().suspend()
    }

    fn is_active(&self) -> bool {
        self.inner.borrow().is_active()
    }

    fn set_size(&mut self, width: u32, height: u32) {
        self.inner.borrow_mut().set_size(width, height)
    }

    fn render<F: FnOnce(&mut Self::ScenePainter<'_>)>(&mut self, draw_fn: F) {
        self.inner.borrow_mut().render(draw_fn)
    }
}
