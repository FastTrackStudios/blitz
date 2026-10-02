//! FTS: a widget's scene rendered into a texture of its own.
//!
//! A custom widget that paints a vector scene is part of the page: every
//! frame it moves, the whole page is painted and rasterised again. Rendered
//! into its own texture instead, it can be drawn over the page as a layer
//! (see [`anyrender::composite`]), and a frame where only it moved costs
//! its own small render. [`Rasterizer`] is that texture, the scene replayed
//! into it by vello — resources included: the widget registers its textures
//! (a shader's output) with the rasterizer, which hands vello them.
//!
//! One vello renderer per device is shared by every rasterizer on the
//! thread (a renderer's pipelines and buffers are not small).

use std::cell::RefCell;
use std::rc::Rc;

use anyrender::{PaintScene, RegisterResourceErrorKind, RenderContext, ResourceId};
use kurbo::Affine;
use peniko::{Color, ImageData};
use rustc_hash::FxHashMap;
use vello::{AaConfig, AaSupport, RenderParams, Renderer as VelloRenderer, RendererOptions};
use wgpu_context::DeviceHandle;

use crate::VelloScenePainter;

thread_local! {
    static SHARED: RefCell<Option<(wgpu::Device, Rc<RefCell<VelloRenderer>>)>> = const { RefCell::new(None) };
}

fn shared_renderer(device: &wgpu::Device) -> Option<Rc<RefCell<VelloRenderer>>> {
    SHARED.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some((d, r)) = slot.as_ref()
            && d == device
        {
            return Some(Rc::clone(r));
        }
        let renderer = VelloRenderer::new(
            device,
            RendererOptions {
                use_cpu: false,
                antialiasing_support: AaSupport::area_only(),
                num_init_threads: None,
                pipeline_cache: None,
            },
        )
        .ok()?;
        let r = Rc::new(RefCell::new(renderer));
        *slot = Some((device.clone(), Rc::clone(&r)));
        Some(r)
    })
}

/// Renders scenes into a texture of its own; see the module docs.
pub struct Rasterizer {
    device_handle: DeviceHandle,
    renderer: Rc<RefCell<VelloRenderer>>,
    /// Textures the widget registered, as vello knows them.
    textures: FxHashMap<ResourceId, ImageData>,
    target: Option<(wgpu::Texture, wgpu::TextureView, (u32, u32))>,
    scene: vello::Scene,
}

impl Rasterizer {
    /// On the device a renderer's `renderer_specific_context` hands over;
    /// `None` for a renderer with no wgpu device.
    #[must_use]
    pub fn new(context: Box<dyn std::any::Any>) -> Option<Self> {
        let device_handle = *context.downcast::<DeviceHandle>().ok()?;
        // FTS: Vello's compute stages need indirect execution (even in its
        // CPU mode, for the raster pass). A device without it (the iOS
        // simulator's Metal) gets no rasteriser, and the widget paints into
        // the page — drawn by whatever renderer the window has.
        let indirect = wgpu::DownlevelFlags::INDIRECT_EXECUTION;
        if !device_handle.adapter.get_downlevel_capabilities().flags.contains(indirect) {
            return None;
        }
        let renderer = shared_renderer(&device_handle.device)?;
        Some(Self {
            device_handle,
            renderer,
            textures: FxHashMap::default(),
            target: None,
            scene: vello::Scene::new(),
        })
    }

    /// Render `scene` into the texture, `size` device pixels; the texture,
    /// and whether it is a new one (first, or resized) to register again.
    pub fn render(&mut self, scene: anyrender::Scene, size: (u32, u32)) -> Option<(wgpu::Texture, bool)> {
        let size = (size.0.max(1), size.1.max(1));
        let fresh = self.target.as_ref().is_none_or(|(_, _, s)| *s != size);
        if fresh {
            let texture = self.device_handle.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("anyrender rasterizer"),
                size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::STORAGE_BINDING
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.target = Some((texture, view, size));
        }
        let mut renderer = self.renderer.borrow_mut();
        {
            let mut painter = VelloScenePainter {
                renderer: Some(&mut renderer),
                device_handle: Some(&self.device_handle),
                texture_handles: Some(&mut self.textures),
                texture_views: None,
                inner: &mut self.scene,
            };
            painter.append_scene(scene, Affine::IDENTITY);
        }
        for handle in self.textures.values() {
            renderer.mark_override_image_dirty(handle);
        }
        let (texture, view, _) = self.target.as_ref()?;
        let rendered = renderer.render_to_texture(
            &self.device_handle.device,
            &self.device_handle.queue,
            &self.scene,
            view,
            &RenderParams {
                base_color: Color::TRANSPARENT,
                width: size.0,
                height: size.1,
                antialiasing_method: AaConfig::Area,
            },
        );
        self.scene.reset();
        rendered.ok()?;
        Some((texture.clone(), fresh))
    }
}

impl RenderContext for Rasterizer {
    fn try_register_custom_resource(
        &mut self,
        resource: Box<dyn std::any::Any>,
    ) -> Result<ResourceId, anyrender::RegisterResourceError> {
        let Ok(texture) = resource.downcast::<wgpu::Texture>() else {
            return Err(RegisterResourceErrorKind::UnsupportedResourceKind.into());
        };
        let id = ResourceId::new();
        self.textures.insert(id, self.renderer.borrow_mut().register_texture(*texture));
        Ok(id)
    }

    fn unregister_resource(&mut self, resource_id: ResourceId) {
        if let Some(handle) = self.textures.remove(&resource_id) {
            self.renderer.borrow_mut().unregister_texture(handle);
        }
    }

    fn renderer_specific_context(&self) -> Option<Box<dyn std::any::Any>> {
        Some(Box::new(self.device_handle.clone()))
    }
}

impl Drop for Rasterizer {
    fn drop(&mut self) {
        let mut renderer = self.renderer.borrow_mut();
        for (_, handle) in self.textures.drain() {
            renderer.unregister_texture(handle);
        }
    }
}
