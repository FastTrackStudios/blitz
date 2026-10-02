//! FTS: the window's last frame (the *base*) kept, and layers drawn over it.
//!
//! See [`anyrender::composite`]. Vello rasterises the page into [`Compositor`]'s
//! base texture instead of straight into the window's; every frame, reused
//! base or fresh, one render pass puts the base on the window's target and
//! draws each layer's texture over it, clipped. A frame where only layers
//! changed is then that one pass: no scene, no vello.

use anyrender::ResourceId;
use anyrender::composite::Layer;
use rustc_hash::FxHashMap;
use wgpu::util::DeviceExt;

const SHADER: &str = r"
struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) alpha: f32,
};

@vertex
fn vs(@location(0) pos: vec2<f32>, @location(1) uv: vec2<f32>, @location(2) alpha: f32) -> VOut {
    var out: VOut;
    out.pos = vec4<f32>(pos, 0.0, 1.0);
    out.uv = uv;
    out.alpha = alpha;
    return out;
}

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    let c = textureSample(tex, samp, in.uv);
    return vec4<f32>(c.rgb, c.a * in.alpha);
}
";

/// Rgba8Unorm, what vello renders and the window's intermediate target is.
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

pub(crate) struct Compositor {
    /// Replaces what is under it (the base).
    opaque: wgpu::RenderPipeline,
    /// Straight-alpha over (layers; vello's output is straight alpha).
    over: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    base: Option<Base>,
    /// A bind group per texture, kept while the texture is registered.
    groups: FxHashMap<ResourceId, wgpu::BindGroup>,
    /// Overlays as last rasterised, by id: where each texture's origin is.
    overlays: FxHashMap<u32, Overlay>,
}

struct Overlay {
    size: (u32, u32),
    view: wgpu::TextureView,
    group: wgpu::BindGroup,
}

struct Base {
    size: (u32, u32),
    view: wgpu::TextureView,
    group: wgpu::BindGroup,
}

impl Compositor {
    pub(crate) fn new(device: &wgpu::Device) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("anyrender compositor"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("anyrender compositor"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("anyrender compositor"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = |blend: Option<wgpu::BlendState>| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("anyrender compositor"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[wgpu::VertexBufferLayout {
                        array_stride: 20,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32],
                    }],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: FORMAT,
                        blend,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let opaque = pipeline(None);
        let over = pipeline(Some(wgpu::BlendState::ALPHA_BLENDING));
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("anyrender compositor"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Self { opaque, over, layout, sampler, base: None, groups: FxHashMap::default(), overlays: FxHashMap::default() }
    }

    fn group(&self, device: &wgpu::Device, view: &wgpu::TextureView) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("anyrender compositor"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        })
    }

    /// Whether a base of this size is kept (so a frame may reuse it).
    pub(crate) fn has_base(&self, size: (u32, u32)) -> bool {
        self.base.as_ref().is_some_and(|b| b.size == size)
    }

    /// The base texture to rasterise the page into, made (or remade) for
    /// `size`.
    pub(crate) fn base_view(&mut self, device: &wgpu::Device, size: (u32, u32)) -> wgpu::TextureView {
        if !self.has_base(size) {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("anyrender compositor base"),
                size: wgpu::Extent3d { width: size.0.max(1), height: size.1.max(1), depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: FORMAT,
                usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let group = self.group(device, &view);
            self.base = Some(Base { size, view, group });
        }
        self.base.as_ref().map(|b| b.view.clone()).expect("base just made")
    }

    /// Overlay `id`'s texture to rasterise into, `size` device pixels.
    pub(crate) fn overlay_view(&mut self, device: &wgpu::Device, id: u32, size: (u32, u32)) -> wgpu::TextureView {
        let size = (size.0.max(1), size.1.max(1));
        if self.overlays.get(&id).is_none_or(|o| o.size != size) {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("anyrender compositor overlay"),
                size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: FORMAT,
                usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let group = self.group(device, &view);
            self.overlays.insert(id, Overlay { size, view, group });
        }
        self.overlays[&id].view.clone()
    }

    /// Forget a texture's bind group (it was unregistered).
    pub(crate) fn forget(&mut self, id: ResourceId) {
        self.groups.remove(&id);
    }

    /// Forget everything tied to the device (suspended).
    pub(crate) fn clear(&mut self) {
        self.base = None;
        self.groups.clear();
        self.overlays.clear();
    }

    /// Put the base on `target` and draw `layers` over it, in order. A
    /// layer whose texture isn't registered (or is clipped away) is skipped.
    pub(crate) fn composite(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        target: &wgpu::TextureView,
        size: (u32, u32),
        layers: &[Layer],
        textures: &FxHashMap<ResourceId, wgpu::TextureView>,
    ) {
        // Textures unregistered since (through a scene painter) go too.
        self.groups.retain(|id, _| textures.contains_key(id));
        let Some(base) = self.base.as_ref().filter(|b| b.size == size) else { return };
        let (w, h) = (f64::from(size.0.max(1)), f64::from(size.1.max(1)));
        let ndc = |x: f64, y: f64| [(x / w * 2.0 - 1.0) as f32, (1.0 - y / h * 2.0) as f32];
        let quad = |x0: f64, y0: f64, x1: f64, y1: f64, alpha: f32| -> [[f32; 5]; 6] {
            let (a, b, c, d) = (ndc(x0, y0), ndc(x1, y0), ndc(x0, y1), ndc(x1, y1));
            [
                [a[0], a[1], 0.0, 0.0, alpha],
                [c[0], c[1], 0.0, 1.0, alpha],
                [b[0], b[1], 1.0, 0.0, alpha],
                [b[0], b[1], 1.0, 0.0, alpha],
                [c[0], c[1], 0.0, 1.0, alpha],
                [d[0], d[1], 1.0, 1.0, alpha],
            ]
        };
        // Overlays no longer drawn go.
        self.overlays.retain(|id, _| layers.iter().any(|l| matches!(l, Layer::Overlay { id: i, .. } if i == id)));
        // Each drawable layer: its bind group and its scissor.
        let mut draws: Vec<(Draw, [u32; 4])> = Vec::new();
        let mut verts: Vec<[f32; 5]> = Vec::with_capacity(6 * (layers.len() + 1));
        verts.extend(quad(0.0, 0.0, w, h, 1.0));
        let scissor = |r: kurbo::Rect| -> Option<[u32; 4]> {
            let r = r.intersect(kurbo::Rect::new(0.0, 0.0, w, h));
            let (x0, y0) = (r.x0.floor().max(0.0) as u32, r.y0.floor().max(0.0) as u32);
            let (x1, y1) = (r.x1.ceil().min(w) as u32, r.y1.ceil().min(h) as u32);
            (x1 > x0 && y1 > y0).then_some([x0, y0, x1 - x0, y1 - y0])
        };
        for layer in layers {
            match layer {
                Layer::Texture { resource, rect, clip, opacity } => {
                    let Some(view) = textures.get(resource) else { continue };
                    let Some(sc) = scissor(clip.intersect(*rect)) else { continue };
                    if !self.groups.contains_key(resource) {
                        let group = self.group(device, view);
                        self.groups.insert(*resource, group);
                    }
                    verts.extend(quad(rect.x0, rect.y0, rect.x1, rect.y1, *opacity));
                    draws.push((Draw::Texture(*resource), sc));
                }
                Layer::Overlay { id, rect, .. } => {
                    let Some(overlay) = self.overlays.get(id) else { continue };
                    let full = kurbo::Rect::new(
                        rect.x0.floor(),
                        rect.y0.floor(),
                        rect.x0.floor() + f64::from(overlay.size.0),
                        rect.y0.floor() + f64::from(overlay.size.1),
                    );
                    let Some(sc) = scissor(full) else { continue };
                    verts.extend(quad(full.x0, full.y0, full.x1, full.y1, 1.0));
                    draws.push((Draw::Overlay(*id), sc));
                }
            }
        }
        let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("anyrender compositor quads"),
            contents: bytemuck_cast(&verts),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("anyrender compositor") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("anyrender compositor"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_vertex_buffer(0, buffer.slice(..));
            pass.set_pipeline(&self.opaque);
            pass.set_bind_group(0, &base.group, &[]);
            pass.draw(0..6, 0..1);
            pass.set_pipeline(&self.over);
            for (i, (draw, [x, y, sw, sh])) in draws.iter().enumerate() {
                let group = match draw {
                    Draw::Texture(id) => self.groups.get(id),
                    Draw::Overlay(id) => self.overlays.get(id).map(|o| &o.group),
                };
                let Some(group) = group else { continue };
                pass.set_scissor_rect(*x, *y, *sw, *sh);
                pass.set_bind_group(0, group, &[]);
                let first = 6 * (i as u32 + 1);
                pass.draw(first..first + 6, 0..1);
            }
        }
        queue.submit([encoder.finish()]);
    }
}

/// What one quad draws from.
enum Draw {
    Texture(ResourceId),
    Overlay(u32),
}

/// `&[[f32; 5]]` as bytes (plain floats, no padding).
fn bytemuck_cast(v: &[[f32; 5]]) -> &[u8] {
    // SAFETY: `[f32; 5]` is 20 plain bytes with no padding or invalid bit
    // patterns, and the slice's length in bytes is 20 × its length.
    unsafe { std::slice::from_raw_parts(v.as_ptr().cast::<u8>(), std::mem::size_of_val(v)) }
}
