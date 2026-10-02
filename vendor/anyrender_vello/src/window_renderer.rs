use anyrender::{
    RegisterResourceErrorKind, RenderContext, ResourceId, WindowHandle, WindowRenderer,
};
use debug_timer::debug_timer;
use futures_channel::oneshot;
use peniko::{Color, ImageData};
use rustc_hash::FxHashMap;
use std::future::Future;
use std::sync::Arc;
use vello::{
    AaConfig, AaSupport, RenderParams, Renderer as VelloRenderer, RendererOptions,
    Scene as VelloScene,
};
use wgpu::{Features, Limits, PresentMode, Texture, TextureFormat, TextureUsages};
use wgpu_context::{
    DeviceHandle, SurfaceRenderer, SurfaceRendererConfiguration, TextureConfiguration, WGPUContext,
};

use anyrender::PaintScene;
use crate::compositor::Compositor;
use crate::{DEFAULT_THREADS, VelloScenePainter};

/// Drive the wgpu init future. On wasm32 we spawn it onto the JS microtask
/// queue (blocking is not allowed). On native we drive it inline with
/// `pollster::block_on` — there's no ambient async runtime to spawn onto, and
/// `on_ready` then fires before `resume` returns.
#[cfg(target_arch = "wasm32")]
fn spawn_init<F: Future<Output = ()> + 'static>(f: F) {
    wasm_bindgen_futures::spawn_local(f);
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn_init<F: Future<Output = ()>>(f: F) {
    pollster::block_on(f);
}

struct ActiveRenderState {
    renderer: VelloRenderer,
    render_surface: SurfaceRenderer<'static>,
    /// FTS: the kept base and the layer pass (see `crate::compositor`).
    compositor: Compositor,
}

/// Result of a successful asynchronous resume; both the active state and the
/// `WGPUContext` are returned so the renderer can reclaim the context.
struct InitOutput {
    active: ActiveRenderState,
}

#[allow(clippy::large_enum_variant)]
enum RenderState {
    Suspended,
    Pending {
        receiver: oneshot::Receiver<InitOutput>,
    },
    Active(ActiveRenderState),
}

#[derive(Clone)]
pub struct VelloRendererOptions {
    pub features: Option<Features>,
    pub limits: Option<Limits>,
    pub base_color: Color,
    pub antialiasing_method: AaConfig,
}

impl Default for VelloRendererOptions {
    fn default() -> Self {
        Self {
            features: None,
            limits: None,
            base_color: Color::WHITE,
            antialiasing_method: AaConfig::Msaa16,
        }
    }
}

pub struct VelloWindowRenderer {
    // The fields MUST be in this order, so that the surface is dropped before the window
    // Window is cached even when suspended so that it can be reused when the app is resumed after being suspended
    render_state: RenderState,
    window_handle: Option<Arc<dyn WindowHandle>>,

    wgpu_context: WGPUContext,
    scene: VelloScene,
    config: VelloRendererOptions,

    // Resources
    texture_handles: FxHashMap<ResourceId, ImageData>,
    /// FTS: the same textures, viewable, for compositing them as layers.
    texture_views: FxHashMap<ResourceId, wgpu::TextureView>,
}

impl VelloWindowRenderer {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self::with_options(VelloRendererOptions::default())
    }

    pub fn with_options(config: VelloRendererOptions) -> Self {
        Self {
            render_state: RenderState::Suspended,
            wgpu_context: build_wgpu_context(&config),
            config,
            window_handle: None,
            scene: VelloScene::new(),
            texture_handles: FxHashMap::default(),
            texture_views: FxHashMap::default(),
        }
    }

    pub fn current_device_handle(&self) -> Option<&DeviceHandle> {
        match &self.render_state {
            RenderState::Active(active) => Some(&active.render_surface.device_handle),
            _ => None,
        }
    }
}

impl RenderContext for VelloWindowRenderer {
    fn try_register_custom_resource(
        &mut self,
        resource: Box<dyn std::any::Any>,
    ) -> Result<ResourceId, anyrender::RegisterResourceError> {
        let RenderState::Active(active) = &mut self.render_state else {
            return Err(RegisterResourceErrorKind::NotActive.into());
        };

        if let Ok(texture) = resource.downcast::<Texture>() {
            let id = ResourceId::new();
            self.texture_views
                .insert(id, texture.create_view(&wgpu::TextureViewDescriptor::default()));
            self.texture_handles
                .insert(id, active.renderer.register_texture(*texture));
            Ok(id)
        } else {
            Err(anyrender::RegisterResourceErrorKind::UnsupportedResourceKind.into())
        }
    }

    fn unregister_resource(&mut self, resource_id: ResourceId) {
        let RenderState::Active(active) = &mut self.render_state else {
            return;
        };

        if let Some(handle) = self.texture_handles.remove(&resource_id) {
            active.renderer.unregister_texture(handle);
        }
        self.texture_views.remove(&resource_id);
        active.compositor.forget(resource_id);
    }

    fn renderer_specific_context(&self) -> Option<Box<dyn std::any::Any>> {
        match &self.render_state {
            RenderState::Active(active) => {
                Some(Box::new(active.render_surface.device_handle.clone()))
            }
            RenderState::Pending { .. } => None,
            RenderState::Suspended => None,
        }
    }
}

fn build_wgpu_context(config: &VelloRendererOptions) -> WGPUContext {
    let features =
        config.features.unwrap_or_default() | Features::CLEAR_TEXTURE | Features::PIPELINE_CACHE;
    WGPUContext::with_features_and_limits(Some(features), config.limits.clone())
}

impl WindowRenderer for VelloWindowRenderer {
    type ScenePainter<'a>
        = VelloScenePainter<'a, 'a>
    where
        Self: 'a;

    fn is_active(&self) -> bool {
        matches!(self.render_state, RenderState::Active { .. })
    }

    fn is_pending(&self) -> bool {
        matches!(self.render_state, RenderState::Pending { .. })
    }

    fn resume<F: FnOnce() + 'static>(
        &mut self,
        window_handle: Arc<dyn WindowHandle>,
        width: u32,
        height: u32,
        on_ready: F,
    ) {
        // Each `resume` must be preceded by `suspend` (or be the first call after
        // construction). Calling while `Pending` or `Active` is a state-machine bug
        // in the embedder: it would orphan the in-flight init's `WGPUContext` and
        // pay for a fresh adapter+device init on the fallback path below.
        if !matches!(self.render_state, RenderState::Suspended) {
            // #[cfg(feature = "tracing")]
            // tracing::warn!("WindowRenderer::resume called from non-Suspended state");
            return;
        }

        let (sender, receiver) = oneshot::channel();
        self.render_state = RenderState::Pending { receiver };
        self.window_handle = Some(window_handle.clone());

        let surface = self
            .wgpu_context
            .create_surface(window_handle)
            .expect("Error creating surface");
        let instance = self.wgpu_context.instance.clone();
        let extra_features = self.wgpu_context.extra_features();
        let override_limits = self.wgpu_context.override_limits();
        let existing_device_handle = self
            .wgpu_context
            .find_compatible_device_handle(Some(&surface));

        spawn_init(async move {
            let device_handle = match existing_device_handle {
                Some(device_handle) => device_handle,
                None => DeviceHandle::new_from_compatible_surface(
                    instance,
                    Some(&surface),
                    extra_features,
                    override_limits,
                )
                .await
                .expect("Error creating DeviceHandle"),
            };

            let render_surface = SurfaceRenderer::new(
                surface,
                SurfaceRendererConfiguration {
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    formats: vec![TextureFormat::Rgba8Unorm, TextureFormat::Bgra8Unorm],
                    width,
                    height,
                    present_mode: PresentMode::AutoVsync,
                    desired_maximum_frame_latency: 2,
                    alpha_mode: wgpu::CompositeAlphaMode::Auto,
                    view_formats: vec![],
                },
                Some(TextureConfiguration {
                    // FTS: RENDER_ATTACHMENT for the compositor's pass,
                    // which now draws the window's frame (vello renders
                    // into the compositor's base).
                    usage: TextureUsages::STORAGE_BINDING
                        | TextureUsages::TEXTURE_BINDING
                        | TextureUsages::RENDER_ATTACHMENT,
                }),
                device_handle,
            )
            .expect("Error creating SurfaceRenderer");

            let renderer = VelloRenderer::new(
                render_surface.device(),
                RendererOptions {
                    antialiasing_support: AaSupport::all(),
                    use_cpu: false,
                    num_init_threads: DEFAULT_THREADS,
                    pipeline_cache: None,
                },
            )
            .unwrap();

            let compositor = Compositor::new(render_surface.device());
            let _ = sender.send(InitOutput {
                active: ActiveRenderState {
                    renderer,
                    render_surface,
                    compositor,
                },
            });
            on_ready();
        });
    }

    fn complete_resume(&mut self) -> bool {
        match &mut self.render_state {
            RenderState::Active { .. } => true,
            RenderState::Suspended => false,
            RenderState::Pending { receiver } => match receiver.try_recv() {
                Ok(Some(InitOutput { active })) => {
                    let device_handle = active.render_surface.device_handle.clone();
                    self.wgpu_context.device_pool.push(device_handle);
                    self.render_state = RenderState::Active(active);
                    anyrender::composite::set_supported(
                        std::env::var_os("FTS_NO_COMPOSITE").is_none(),
                    );
                    true
                }
                _ => false,
            },
        }
    }

    fn suspend(&mut self) {
        if let RenderState::Active(active) = &mut self.render_state {
            // Unregister all textures on suspend
            for (_id, handle) in self.texture_handles.drain() {
                active.renderer.unregister_texture(handle);
            }
            self.texture_views.clear();
            active.compositor.clear();
            anyrender::composite::set_supported(false);
        }
        self.render_state = RenderState::Suspended;
    }

    fn set_size(&mut self, width: u32, height: u32) {
        if let RenderState::Active(active) = &mut self.render_state {
            active.render_surface.resize(width, height);
        };
    }

    fn render<F: FnOnce(&mut Self::ScenePainter<'_>)>(&mut self, draw_fn: F) {
        let RenderState::Active(state) = &mut self.render_state else {
            return;
        };

        let render_surface = &mut state.render_surface;
        let size = (render_surface.config.width, render_surface.config.height);

        debug_timer!(timer, feature = "log_frame_times");

        // FTS: reuse the kept base when the painter says nothing but its
        // layers changed; otherwise paint and rasterise the page into it.
        let reuse = anyrender::composite::reuse_requested() && state.compositor.has_base(size);
        anyrender::composite::set_reused(reuse);
        if !reuse {
            // Regenerate the vello scene
            draw_fn(&mut VelloScenePainter {
                inner: &mut self.scene,
                renderer: Some(&mut state.renderer),
                device_handle: Some(&render_surface.device_handle),
                texture_handles: Some(&mut self.texture_handles),
                texture_views: Some(&mut self.texture_views),
            });
            timer.record_time("cmd");

            for handle in self.texture_handles.values() {
                state.renderer.mark_override_image_dirty(handle);
            }

            let base = state.compositor.base_view(render_surface.device(), size);
            state
                .renderer
                .render_to_texture(
                    render_surface.device(),
                    render_surface.queue(),
                    &self.scene,
                    &base,
                    &RenderParams {
                        base_color: self.config.base_color,
                        width: size.0,
                        height: size.1,
                        antialiasing_method: self.config.antialiasing_method,
                    },
                )
                .expect("failed to render to texture");
            timer.record_time("render");

            // Empty the Vello scene (memory optimisation)
            self.scene.reset();

            // FTS: the page content lifted over layers, rasterised with it.
            for layer in anyrender::composite::layers() {
                let anyrender::composite::Layer::Overlay { id, rect, scene: Some(content) } = layer else { continue };
                let origin = (rect.x0.floor(), rect.y0.floor());
                let overlay_size = ((rect.x1 - origin.0).ceil() as u32, (rect.y1 - origin.1).ceil() as u32);
                let view = state.compositor.overlay_view(render_surface.device(), id, overlay_size);
                VelloScenePainter {
                    inner: &mut self.scene,
                    renderer: Some(&mut state.renderer),
                    device_handle: Some(&render_surface.device_handle),
                    texture_handles: Some(&mut self.texture_handles),
                    texture_views: None,
                }
                .append_scene((*content).clone(), kurbo::Affine::translate((-origin.0, -origin.1)));
                state
                    .renderer
                    .render_to_texture(
                        render_surface.device(),
                        render_surface.queue(),
                        &self.scene,
                        &view,
                        &RenderParams {
                            base_color: Color::TRANSPARENT,
                            width: overlay_size.0.max(1),
                            height: overlay_size.1.max(1),
                            antialiasing_method: self.config.antialiasing_method,
                        },
                    )
                    .expect("failed to render an overlay");
                self.scene.reset();
            }
        }

        let Ok(texture_view) = render_surface.target_texture_view() else {
            // Skip frame in case of error trying to get current surface texture
            render_surface.clear_surface_texture();
            return;
        };
        let layers = anyrender::composite::layers();
        state.compositor.composite(
            render_surface.device(),
            render_surface.queue(),
            &texture_view,
            size,
            &layers,
            &self.texture_views,
        );
        timer.record_time("composite");
        drop(texture_view);

        if render_surface.maybe_blit_and_present().is_err() {
            return;
        }
        timer.record_time("present");

        // FTS: reclaim finished work without waiting for the GPU. Waiting
        // here serialised every frame's CPU work behind the last frame's
        // GPU work; presenting already bounds how far ahead we get.
        let _ = render_surface.device().poll(wgpu::PollType::Poll);

        timer.record_time("wait");
        timer.print_times("vello: ");
    }
}
