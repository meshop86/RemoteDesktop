//! Vẽ video lên màn hình qua wgpu, dùng chung bộ nhớ với bộ giải mã.

#[cfg(target_os = "macos")]
mod import_macos;
#[cfg(target_os = "windows")]
mod import_windows;

// `supports_10bit` là hàm chứ không phải `features().contains(NEEDED_FOR_10BIT)`
// ở nơi gọi: trên Windows hằng đó rỗng, mà tập rỗng thì `contains` luôn đúng —
// tức là sẽ trả lời "có" cho một đường không tồn tại.
#[cfg(target_os = "macos")]
pub use import_macos::{NEEDED_FOR_10BIT, supports_10bit};
#[cfg(target_os = "windows")]
pub use import_windows::{NEEDED_FOR_10BIT, supports_10bit};

use std::collections::HashMap;
use std::sync::Arc;

use eframe::egui_wgpu::{CallbackTrait, ScreenDescriptor};
use rd_codec::FrameFormat;

#[cfg(target_os = "macos")]
use rd_codec::videotoolbox::DecodedFrame;
#[cfg(target_os = "windows")]
use rd_codec::mediafoundation::DecodedFrame;

/// Số bộ texture giữ lại trong cache.
///
/// Bộ giải mã lấy buffer từ một pool nhỏ và xoay vòng, nên chỉ vài IOSurface
/// khác nhau xuất hiện. Giữ dư một ít để lúc pool giãn ra vẫn còn trúng cache.
/// Bản Windows không cần: khoá cache là chỉ số ô trong vòng texture cố định,
/// nên số mục không bao giờ vượt quá kích thước vòng.
#[cfg(target_os = "macos")]
const CACHE_LIMIT: usize = 8;

/// Hệ số kéo dải hẹp về [0,1] cho luma và về [-0.5,0.5] cho chroma.
///
/// Xem thêm phần giải thích trong `shader.wgsl`.
fn range_params(format: FrameFormat) -> [f32; 4] {
    match format {
        // 8-bit: luma [16,235], chroma [16,240] quanh tâm 128.
        FrameFormat::Nv12VideoRange => [255.0 / 219.0, -16.0 / 219.0, 255.0 / 224.0, -128.0 / 224.0],
        // 10-bit nằm ở phần cao của 16 bit: mã 10-bit c được lưu thành c<<6.
        // Luma [64,940] → [4096,60160]; chroma [64,960] quanh tâm 512 → tâm 32768.
        FrameFormat::P210VideoRange => [
            65535.0 / 56064.0,
            -4096.0 / 56064.0,
            65535.0 / 57344.0,
            -32768.0 / 57344.0,
        ],
    }
}

struct CachedPlanes {
    bind_group: wgpu::BindGroup,
    /// Giữ texture sống đúng bằng tuổi thọ của bind group. Bản Windows để
    /// trống: texture nằm trong vòng của `Importer`, và cache luôn bị xoá cùng
    /// lúc importer bị thay.
    _planes: Vec<wgpu::Texture>,
}

pub struct VideoRenderer {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniform: wgpu::Buffer,
    cache: HashMap<usize, CachedPlanes>,
    /// Bind group của frame sắp vẽ, do `prepare` đặt vào.
    current: Option<wgpu::BindGroup>,
    /// Frame đang được texture tham chiếu — giữ để IOSurface không bị thu hồi
    /// giữa lúc GPU còn đang đọc.
    _held: Option<Arc<DecodedFrame>>,
    /// Cầu nối D3D11 → D3D12. Dựng theo kích thước frame đầu tiên nên chỉ có
    /// sau khi frame đó tới.
    #[cfg(target_os = "windows")]
    importer: Option<import_windows::Importer>,
}

impl VideoRenderer {
    pub fn new(device: &wgpu::Device, target: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("video yuv->rgb"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("video planes"),
            entries: &[
                plane_entry(0),
                plane_entry(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("video"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("video"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(target.into())],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        // Lọc tuyến tính để phóng plane chroma (nhỏ hơn plane luma) cho mượt;
        // ClampToEdge tránh viền bị lấy mẫu vòng sang mép đối diện.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("video"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("video range"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline,
            layout,
            sampler,
            uniform,
            cache: HashMap::new(),
            current: None,
            _held: None,
            #[cfg(target_os = "windows")]
            importer: None,
        }
    }

    /// Vẽ frame đã nạp. Tách khỏi `CallbackTrait::paint` để test dựng render
    /// pass của riêng nó mà vẫn chạy đúng đoạn mã đang chạy thật.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        let Some(bind_group) = self.current.as_ref() else {
            return;
        };
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, bind_group, &[]);
        pass.draw(0..3, 0..1);
    }

    #[cfg(target_os = "macos")]
    pub fn bind_frame(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        frame: &Arc<DecodedFrame>,
    ) -> anyhow::Result<()> {
        let pixels = frame.pixel_buffer();
        let key = import_macos::surface_key(pixels)
            .ok_or_else(|| anyhow::anyhow!("frame không nằm trên IOSurface"))?;

        if !self.cache.contains_key(&key) {
            if self.cache.len() >= CACHE_LIMIT {
                self.cache.clear();
            }
            let planes = import_macos::import_planes(device, pixels, frame.format)?;
            let bind_group = build_bind_group(
                device,
                &self.layout,
                &self.sampler,
                &self.uniform,
                [&planes[0], &planes[1]],
            );
            self.cache.insert(
                key,
                CachedPlanes {
                    bind_group,
                    _planes: planes.into(),
                },
            );
        }

        self.finish(queue, key, frame);
        Ok(())
    }

    #[cfg(target_os = "windows")]
    pub fn bind_frame(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        frame: &Arc<DecodedFrame>,
    ) -> anyhow::Result<()> {
        // Vòng texture chia sẻ gắn cứng với một kích thước. Máy bên kia đổi độ
        // phân giải thì dựng lại cả vòng, và bind group cũ trỏ vào texture cũ
        // nên phải bỏ theo.
        if self
            .importer
            .as_ref()
            .is_none_or(|importer| !importer.matches(frame))
        {
            self.importer = Some(import_windows::Importer::new(device, frame)?);
            self.cache.clear();
        }

        let importer = self.importer.as_mut().expect("vừa dựng ở trên");
        // Khoá cache là chỉ số ô trong vòng — vòng cố định nên cache đầy sau
        // đúng `RING` frame rồi trúng mãi.
        let key = importer.stage(frame)?;

        if !self.cache.contains_key(&key) {
            let planes = importer.planes(key);
            let bind_group = build_bind_group(
                device,
                &self.layout,
                &self.sampler,
                &self.uniform,
                [&planes[0], &planes[1]],
            );
            self.cache.insert(
                key,
                CachedPlanes {
                    bind_group,
                    _planes: Vec::new(),
                },
            );
        }

        self.finish(queue, key, frame);
        Ok(())
    }

    /// Phần chung của hai bản `bind_frame`: nạp hệ số dải màu và chọn bind
    /// group sẽ vẽ.
    fn finish(&mut self, queue: &wgpu::Queue, key: usize, frame: &Arc<DecodedFrame>) {
        queue.write_buffer(&self.uniform, 0, bytemuck_cast(&range_params(frame.format)));
        self.current = Some(
            self.cache
                .get(&key)
                .expect("vừa chèn hoặc đã có sẵn")
                .bind_group
                .clone(),
        );
        self._held = Some(frame.clone());
    }
}

fn build_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    uniform: &wgpu::Buffer,
    planes: [&wgpu::Texture; 2],
) -> wgpu::BindGroup {
    let views = [
        planes[0].create_view(&Default::default()),
        planes[1].create_view(&Default::default()),
    ];
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("video planes"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&views[0]),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&views[1]),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: uniform.as_entire_binding(),
            },
        ],
    })
}

fn plane_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

/// `[f32; 4]` sang byte. Mảng f32 không có khoảng đệm nên đọc lại dạng byte là
/// hợp lệ, khỏi cần kéo thêm thư viện.
fn bytemuck_cast(values: &[f32; 4]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(values.as_ptr().cast::<u8>(), size_of::<[f32; 4]>()) }
}

/// Yêu cầu vẽ một frame, do egui gọi lại đúng lúc trong render pass của nó.
pub struct VideoCallback {
    pub frame: Arc<DecodedFrame>,
}

impl CallbackTrait for VideoCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut eframe::egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        if let Some(renderer) = resources.get_mut::<VideoRenderer>() {
            if let Err(err) = renderer.bind_frame(device, queue, &self.frame) {
                tracing::warn!(%err, "không nạp được frame vào GPU");
                renderer.current = None;
            }
        }
        Vec::new()
    }

    /// egui đã đặt sẵn viewport bằng đúng ô mà ta xin, nên ở đây chỉ việc vẽ:
    /// tam giác phủ kín viewport chính là phủ kín ô video.
    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        pass: &mut wgpu::RenderPass<'static>,
        resources: &eframe::egui_wgpu::CallbackResources,
    ) {
        let Some(renderer) = resources.get::<VideoRenderer>() else {
            return;
        };
        renderer.draw(pass);
    }
}
