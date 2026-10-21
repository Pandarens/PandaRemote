//! Draws NV12 video frames with a wgpu shader inside egui: the GPU does the YUV -> RGB conversion,
//! and uploads are 1.5 bytes per pixel instead of 4.

use std::sync::{Arc, Mutex};

use eframe::{
    egui,
    egui_wgpu::{self, CallbackResources, CallbackTrait, RenderState, ScreenDescriptor},
    wgpu,
};

use crate::client::viewer::VideoFrame;

const SHADER: &str = r#"
@group(0) @binding(0) var y_tex: texture_2d<f32>;
@group(0) @binding(1) var uv_tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VOut {
    // One triangle covering the viewport (egui sets the viewport to the callback rect).
    var p = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    var o: VOut;
    o.pos = vec4(p[i], 0.0, 1.0);
    o.uv = vec2((p[i].x + 1.0) * 0.5, (1.0 - p[i].y) * 0.5);
    return o;
}

fn rgb(uv: vec2<f32>) -> vec3<f32> {
    // BT.601 limited range, matching the host's encoder input.
    let y = (textureSample(y_tex, samp, uv).r - 16.0 / 255.0) * 1.164;
    let c = textureSample(uv_tex, samp, uv).rg - vec2(0.5, 0.5);
    return clamp(vec3(y + 1.596 * c.y, y - 0.392 * c.x - 0.813 * c.y, y + 2.017 * c.x), vec3(0.0), vec3(1.0));
}

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    return vec4(rgb(in.uv), 1.0);
}

// For sRGB render targets the hardware re-encodes, so hand it linear values.
@fragment
fn fs_srgb(in: VOut) -> @location(0) vec4<f32> {
    return vec4(pow(rgb(in.uv), vec3(2.2)), 1.0);
}
"#;

/// Frame waiting to be uploaded at the next paint.
pub type PendingFrame = Arc<Mutex<Option<VideoFrame>>>;

struct Resources {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    textures: Option<Textures>,
}

struct Textures {
    size: (u32, u32),
    y: wgpu::Texture,
    uv: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

/// Creates the pipeline once; returns false if the app isn't running on wgpu.
pub fn init(render_state: Option<&RenderState>) -> bool {
    let Some(rs) = render_state else { return false };
    let resources = create_resources(&rs.device, rs.target_format);
    rs.renderer.write().callback_resources.insert(resources);
    true
}

fn create_resources(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Resources {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("nv12"),
        source: wgpu::ShaderSource::Wgsl(SHADER.into()),
    });
    let texture_entry = |binding| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    };
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("nv12"),
        entries: &[
            texture_entry(0),
            texture_entry(1),
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("nv12"),
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("nv12"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some(if target_format.is_srgb() { "fs_srgb" } else { "fs" }),
            compilation_options: Default::default(),
            targets: &[Some(target_format.into())],
        }),
        multiview_mask: None,
        cache: None,
    });
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("nv12"),
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    Resources { pipeline, layout, sampler, textures: None }
}

/// Paint callback: uploads a pending frame (if any) and draws the current one into `rect`.
pub fn paint(painter: &egui::Painter, rect: egui::Rect, pending: PendingFrame) {
    painter.add(egui_wgpu::Callback::new_paint_callback(rect, Nv12Callback { pending }));
}

struct Nv12Callback {
    pending: PendingFrame,
}

impl CallbackTrait for Nv12Callback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(frame) = self.pending.lock().unwrap().take() else {
            return Vec::new();
        };
        let Some(res) = resources.get_mut::<Resources>() else {
            return Vec::new();
        };
        let size = (frame.width as u32, frame.height as u32);
        if res.textures.as_ref().is_none_or(|t| t.size != size) {
            res.textures = Some(create_textures(device, res, size));
        }
        upload(queue, res.textures.as_ref().unwrap(), &frame);
        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        pass: &mut wgpu::RenderPass<'static>,
        resources: &CallbackResources,
    ) {
        let Some(res) = resources.get::<Resources>() else {
            return;
        };
        let Some(t) = &res.textures else { return };
        pass.set_pipeline(&res.pipeline);
        pass.set_bind_group(0, &t.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

fn upload(queue: &wgpu::Queue, t: &Textures, frame: &VideoFrame) {
    let (w, h) = t.size;
    let write = |texture: &wgpu::Texture, data: &[u8], bytes_per_row: u32, w: u32, h: u32| {
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(bytes_per_row), rows_per_image: Some(h) },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
    };
    write(&t.y, &frame.y, w, w, h);
    write(&t.uv, &frame.uv, w, w / 2, h / 2);
}

fn create_textures(device: &wgpu::Device, res: &Resources, (w, h): (u32, u32)) -> Textures {
    let make = |label, format, width, height| {
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        })
    };
    let y = make("nv12-y", wgpu::TextureFormat::R8Unorm, w, h);
    let uv = make("nv12-uv", wgpu::TextureFormat::Rg8Unorm, w / 2, h / 2);
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("nv12"),
        layout: &res.layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&y.create_view(&Default::default())),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&uv.create_view(&Default::default())),
            },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&res.sampler) },
        ],
    });
    Textures { size: (w, h), y, uv, bind_group }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::encoder::{Picture, to_nv12};

    /// Renders a solid NV12 frame through the real pipeline into an offscreen texture and checks the colour.
    #[test]
    fn shader_converts_nv12_to_rgb() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let Ok(adapter) = rt.block_on(instance.request_adapter(&Default::default())) else {
            eprintln!("no GPU adapter, skipping");
            return;
        };
        let (device, queue) = rt.block_on(adapter.request_device(&Default::default())).unwrap();
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let res = create_resources(&device, format);

        let (w, h) = (8u32, 8u32);
        let rgba: Vec<u8> = (0..w * h).flat_map(|_| [200u8, 100, 50, 255]).collect();
        let mut nv12 = Vec::new();
        to_nv12(&Picture { pixels: &rgba, width: w, height: h, bgra: false }, &mut nv12);
        let (y, uv) = nv12.split_at((w * h) as usize);
        let frame = VideoFrame { width: w as usize, height: h as usize, y: y.to_vec(), uv: uv.to_vec() };
        let textures = create_textures(&device, &res, (w, h));
        upload(&queue, &textures, &frame);

        let size = 64u32;
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&Default::default());
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (size * size * 4) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&res.pipeline);
            pass.set_bind_group(0, &textures.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(size * 4),
                    rows_per_image: Some(size),
                },
            },
            wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
        );
        queue.submit([encoder.finish()]);
        readback.slice(..).map_async(wgpu::MapMode::Read, |_| ());
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let data = readback.slice(..).get_mapped_range().unwrap();
        let center = (((size / 2) * size + size / 2) * 4) as usize;
        let px = &data[center..center + 4];
        for (got, want) in px.iter().zip([200u8, 100, 50, 255]) {
            assert!((*got as i32 - want as i32).abs() <= 4, "pixel {px:?}");
        }
    }
}
