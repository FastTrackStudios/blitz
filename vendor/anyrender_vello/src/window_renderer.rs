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
use wgpu::{
    CompositeAlphaMode, Features, Limits, PipelineCache, PresentMode, Texture, TextureFormat,
    TextureUsages,
};
use wgpu_context::{
    AlphaConversion, DeviceHandle, SurfaceRenderer, SurfaceRendererConfiguration,
    TextureConfiguration, WGPUContext,
};

use crate::compositor::Compositor;
use crate::{DEFAULT_THREADS, VelloScenePainter};
use anyrender::PaintScene;

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
#[non_exhaustive]
pub struct VelloRendererOptions {
    pub features: Option<Features>,
    pub limits: Option<Limits>,
    pub base_color: Color,
    pub antialiasing_method: AaConfig,
    /// Alpha mode used when compositing the window surface.
    pub composite_alpha_mode: anyrender::CompositeAlphaMode,
    /// Cache that wgpu may reuse compiled pipelines from, avoiding shader
    /// compilation on start-up. Creating it, persisting its data and deciding
    /// when it is stale are all the caller's responsibility.
    pub pipeline_cache: Option<PipelineCache>,
    /// Maximum number of frames the presentation engine may queue ahead of the
    /// display. Each queued frame requires a window-sized swapchain surface, so
    /// lower values reduce memory usage and input latency at the cost of less
    /// buffering to absorb slow frames.
    pub desired_maximum_frame_latency: u32,
}

impl Default for VelloRendererOptions {
    fn default() -> Self {
        // FTS: the surface's clear colour from `ANYRENDER_BASE_COLOR`
        // (see `base_color_from_env`); `new` is `const` and cannot read it.
        Self {
            base_color: base_color_from_env().unwrap_or(Color::WHITE),
            ..Self::new()
        }
    }
}

impl VelloRendererOptions {
    pub const fn new() -> Self {
        Self {
            features: None,
            limits: None,
            base_color: Color::WHITE,
            antialiasing_method: AaConfig::Msaa16,
            composite_alpha_mode: anyrender::CompositeAlphaMode::Auto,
            pipeline_cache: None,
            desired_maximum_frame_latency: 1,
        }
    }

    pub fn features(self, features: Features) -> Self {
        Self {
            features: Some(features),
            ..self
        }
    }

    pub fn limits(self, limits: Limits) -> Self {
        Self {
            limits: Some(limits),
            ..self
        }
    }

    pub fn base_color(self, base_color: Color) -> Self {
        Self { base_color, ..self }
    }

    pub fn antialiasing_method(self, antialiasing_method: AaConfig) -> Self {
        Self {
            antialiasing_method,
            ..self
        }
    }

    pub fn composite_alpha_mode(
        self,
        composite_alpha_mode: anyrender::types::CompositeAlphaMode,
    ) -> Self {
        Self {
            composite_alpha_mode,
            ..self
        }
    }

    pub fn pipeline_cache(self, pipeline_cache: PipelineCache) -> Self {
        Self {
            pipeline_cache: Some(pipeline_cache),
            ..self
        }
    }

    pub fn desired_maximum_frame_latency(self, desired_maximum_frame_latency: u32) -> Self {
        Self {
            desired_maximum_frame_latency,
            ..self
        }
    }
}

impl From<anyrender::RendererConfig> for VelloRendererOptions {
    fn from(config: anyrender::RendererConfig) -> Self {
        Self {
            // FTS: an explicit colour wins; `ANYRENDER_BASE_COLOR` is the
            // fallback before upstream's white.
            base_color: config
                .base_color
                .or_else(base_color_from_env)
                .unwrap_or(Color::WHITE),
            composite_alpha_mode: config.composite_alpha_mode.unwrap_or_default(),
            ..Default::default()
        }
    }
}

/// FTS: the colour the surface is cleared to, `ANYRENDER_BASE_COLOR=#rrggbb`.
/// What it shows is every pixel the page does not paint — on iOS the safe
/// area's strips outside the page's viewport (Blitz lays the page out
/// inside the insets and fills only the viewport with the root's
/// background). Upstream's white made a white strip along an iPad's
/// bottom edge under a dark app; the app sets its own ground here.
fn base_color_from_env() -> Option<Color> {
    let v = std::env::var("ANYRENDER_BASE_COLOR").ok()?;
    let hex = v.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let ch = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
    Some(Color::from_rgb8(ch(0)?, ch(2)?, ch(4)?))
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

    pub fn with_options(config: impl Into<VelloRendererOptions>) -> Self {
        let config = config.into();
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
            self.texture_views.insert(
                id,
                texture.create_view(&wgpu::TextureViewDescriptor::default()),
            );
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
        let mut composite_alpha_mode = match self.config.composite_alpha_mode {
            anyrender::CompositeAlphaMode::Auto => CompositeAlphaMode::Auto,
            anyrender::CompositeAlphaMode::Opaque => CompositeAlphaMode::Opaque,
            anyrender::CompositeAlphaMode::Transparent => {
                #[cfg(target_vendor = "apple")]
                {
                    // wgpu is lying in apple's case it uses PreMultiplied in reality
                    // (do not modify shaders for PostMultiplied)
                    CompositeAlphaMode::PostMultiplied
                }
                #[cfg(not(target_vendor = "apple"))]
                {
                    CompositeAlphaMode::PreMultiplied
                }
            }
        };
        let pipeline_cache = self.config.pipeline_cache.clone();
        let desired_maximum_frame_latency = self.config.desired_maximum_frame_latency;
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

            let adapter = &device_handle.adapter;
            let caps = surface.get_capabilities(adapter);
            let mut alpha_modes = caps.alpha_modes;

            if !alpha_modes.contains(&composite_alpha_mode) {
                alpha_modes.sort_unstable_by(
                    |first: &CompositeAlphaMode, second: &CompositeAlphaMode| {
                        let first_num = match *first {
                            CompositeAlphaMode::PreMultiplied => 0,
                            CompositeAlphaMode::PostMultiplied => 1,
                            CompositeAlphaMode::Opaque
                            | CompositeAlphaMode::Inherit
                            | CompositeAlphaMode::Auto => 2,
                        };
                        let second_num = match *second {
                            CompositeAlphaMode::PreMultiplied => 0,
                            CompositeAlphaMode::PostMultiplied => 1,
                            CompositeAlphaMode::Opaque
                            | CompositeAlphaMode::Inherit
                            | CompositeAlphaMode::Auto => 2,
                        };
                        first_num.cmp(&second_num)
                    },
                );
                composite_alpha_mode = alpha_modes
                    .first()
                    .copied()
                    .expect("Surface didn't report any alpha modes");
            }

            #[cfg(not(target_vendor = "apple"))]
            let texture_config = Some(TextureConfiguration {
                // FTS: RENDER_ATTACHMENT for the compositor's pass, which
                // draws the window's frame (vello renders into the
                // compositor's base).
                usage: TextureUsages::STORAGE_BINDING
                    | TextureUsages::TEXTURE_BINDING
                    | TextureUsages::RENDER_ATTACHMENT,
                format: TextureFormat::Rgba8Unorm,
                alpha_conversion: (composite_alpha_mode == CompositeAlphaMode::PreMultiplied)
                    .then_some(AlphaConversion::Premultiply),
            });
            // TODO: Remove below once gfx-rs/wgpu#9896 gets fixed
            #[cfg(target_vendor = "apple")]
            let texture_config = Some(TextureConfiguration {
                // FTS: RENDER_ATTACHMENT for the compositor's pass, which
                // draws the window's frame (vello renders into the
                // compositor's base).
                usage: TextureUsages::STORAGE_BINDING
                    | TextureUsages::TEXTURE_BINDING
                    | TextureUsages::RENDER_ATTACHMENT,
                format: TextureFormat::Rgba8Unorm,
                alpha_conversion: (composite_alpha_mode == CompositeAlphaMode::PostMultiplied
                    || composite_alpha_mode == CompositeAlphaMode::PreMultiplied)
                    .then_some(AlphaConversion::Premultiply),
            });

            // FTS: always through the intermediate texture — the
            // compositor renders the window's frame into it, so the
            // direct-surface path (`FTS_DIRECT_SURFACE`, measured to buy
            // nothing) went with the compositor's arrival.
            let surface_usage = wgpu::TextureUsages::RENDER_ATTACHMENT;
            let render_surface = SurfaceRenderer::new(
                surface,
                SurfaceRendererConfiguration {
                    usage: surface_usage,
                    formats: vec![TextureFormat::Rgba8Unorm, TextureFormat::Bgra8Unorm],
                    width,
                    height,
                    // FTS: switchable, so vsync can be ruled in or out by
                    // measurement rather than argued about. `AutoVsync`
                    // blocks on the compositor's cadence, which makes a
                    // slow frame and a frame that is merely WAITING look
                    // identical from the outside.
                    present_mode: match std::env::var("FTS_PRESENT").as_deref() {
                        Ok("immediate") => PresentMode::AutoNoVsync,
                        Ok("mailbox") => PresentMode::Mailbox,
                        _ => PresentMode::AutoVsync,
                    },
                    // FTS: `FTS_FRAME_LATENCY` overrides the configured
                    // latency (1 by default since anyrender_vello 0.15,
                    // which took the measurement this fork made: at 2560x1440
                    // acquire-and-present went from 6.7ms at a latency of
                    // two to 2.2ms at one), because it is the sort of number
                    // that wants re-measuring on a different compositor.
                    desired_maximum_frame_latency: std::env::var("FTS_FRAME_LATENCY")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(desired_maximum_frame_latency),
                    alpha_mode: composite_alpha_mode,
                    view_formats: vec![],
                },
                texture_config,
                device_handle,
            )
            .expect("Error creating SurfaceRenderer");

            let renderer = VelloRenderer::new(
                render_surface.device(),
                RendererOptions {
                    antialiasing_support: AaSupport::all(),
                    use_cpu: false,
                    num_init_threads: DEFAULT_THREADS,
                    pipeline_cache,
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
                let anyrender::composite::Layer::Overlay {
                    id,
                    rect,
                    scene: Some(content),
                } = layer
                else {
                    continue;
                };
                let origin = (rect.x0.floor(), rect.y0.floor());
                let overlay_size = (
                    (rect.x1 - origin.0).ceil() as u32,
                    (rect.y1 - origin.1).ceil() as u32,
                );
                let view = state
                    .compositor
                    .overlay_view(render_surface.device(), id, overlay_size);
                VelloScenePainter {
                    inner: &mut self.scene,
                    renderer: Some(&mut state.renderer),
                    device_handle: Some(&render_surface.device_handle),
                    texture_handles: Some(&mut self.texture_handles),
                    texture_views: None,
                }
                .append_scene(
                    (*content).clone(),
                    kurbo::Affine::translate((-origin.0, -origin.1)),
                );
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
            self.scene.reset();
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
            self.scene.reset();
            return;
        }
        timer.record_time("present");
        #[cfg(feature = "log_frame_times")]
        {
            use std::sync::atomic::{AtomicBool, Ordering};
            static SAID: AtomicBool = AtomicBool::new(false);
            if !SAID.swap(true, Ordering::Relaxed) {
                println!(
                    "vello: surface {:?} {}x{}, present {:?}, latency {}, blit {}",
                    render_surface.config.format,
                    render_surface.config.width,
                    render_surface.config.height,
                    render_surface.config.present_mode,
                    render_surface.config.desired_maximum_frame_latency,
                    // Always through the intermediate since the
                    // compositor (the direct-surface path is gone).
                    "yes",
                );
            }
        }

        // FTS: poll, do not WAIT.
        //
        // This used to be `wait_indefinitely`, which blocks the thread
        // until the GPU has finished everything it was given. That is a
        // hard barrier between the CPU and the GPU once a frame: the
        // CPU cannot start building frame N+1 until frame N has been
        // rasterised, so the two never overlap and a frame costs
        // CPU + GPU rather than max(CPU, GPU).
        //
        // `Poll` does the same housekeeping — running map callbacks and
        // freeing what the queue has released — without the barrier.
        // `FTS_GPU_BARRIER=1` puts the wait back, for the case where a
        // driver needs it to stay honest.
        let barrier = std::env::var_os("FTS_GPU_BARRIER").is_some();
        let _ = render_surface.device().poll(if barrier {
            wgpu::PollType::wait_indefinitely()
        } else {
            wgpu::PollType::Poll
        });

        timer.record_time("wait");
        timer.print_times("vello: ");
    }
}
