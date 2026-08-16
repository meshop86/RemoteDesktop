//! Kiểm chứng đường vẽ bằng số: màu vào phải bằng màu ra.
//!
//! Đi đúng chuỗi thật — BGRA → mã hoá HEVC 4:2:2 10-bit → giải mã → nhập
//! IOSurface vào wgpu → shader chuyển màu → đọc ngược pixel. Sai một hệ số dải
//! hẹp hay đảo Cb/Cr thì ảnh vẫn "trông có vẻ đúng" với mắt thường nhưng lệch
//! hàng chục đơn vị ở đây.

#![cfg(target_os = "macos")]

use std::ptr::{NonNull, null_mut};
use std::time::Duration;

use objc2_core_foundation::{CFDictionary, CFRetained, CFType};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey,
    kCVPixelFormatType_32BGRA,
};
use rd_codec::videotoolbox::{VtDecoder, VtEncoder};
use rd_codec::{ChromaSubsampling, Codec, EncoderConfig};
use rd_viewer::video::{NEEDED_FOR_10BIT, VideoRenderer};

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;

/// Khung đích cố tình nhỏ: shader vẽ một tam giác phủ kín viewport nên ảnh gốc
/// vẫn được lấy mẫu toàn bộ. 64 pixel × 4 byte = 256 byte, vừa đúng bội số mà
/// `copy_texture_to_buffer` đòi hỏi nên khỏi phải chèn đệm khi đọc về.
const OUT_W: u32 = 64;
const OUT_H: u32 = 36;

/// Ba dải màu bão hoà. Bão hoà là trường hợp khó nhất cho 4:2:2: chroma bị giảm
/// một nửa theo chiều ngang, còn dải hẹp thì cắt cụt hai đầu thang giá trị.
const BANDS: [[u8; 3]; 3] = [
    [220, 40, 40],  // đỏ
    [40, 200, 60],  // xanh lá
    [50, 70, 230],  // xanh dương
];

/// Sai số cho phép trên mỗi kênh, thang 0-255.
///
/// Nguồn sai: lượng tử hoá của bộ mã hoá, chroma 4:2:2, và làm tròn 10-bit.
/// Đảo Cb/Cr hay quên bù dải hẹp đều lệch trên 40 nên ngưỡng này vẫn bắt được.
const TOLERANCE: i32 = 14;

#[test]
fn mau_qua_shader_khop_voi_mau_goc() {
    let Some((device, queue)) = open_device() else {
        eprintln!("bỏ qua: máy không có adapter wgpu dùng được");
        return;
    };
    if !device.features().contains(NEEDED_FOR_10BIT) {
        eprintln!("bỏ qua: card đồ hoạ không có texture 16-bit chuẩn hoá");
        return;
    }

    let source = make_bgra_bands();
    let mut encoder = VtEncoder::new(EncoderConfig {
        width: WIDTH,
        height: HEIGHT,
        codec: Codec::Hevc,
        chroma: ChromaSubsampling::Yuv422,
        target_bitrate_kbps: 60_000,
        target_fps: 30,
        keyframe_interval_secs: 1,
    })
    .expect("tạo bộ mã hoá");
    let mut decoder =
        VtDecoder::new(Codec::Hevc, encoder.actual_chroma()).expect("tạo bộ giải mã");

    // Vài frame giống hệt nhau để bộ mã hoá qua giai đoạn dò bitrate, rồi mới
    // lấy frame cuối làm mẫu đo.
    let mut decoded = None;
    for index in 0..4u64 {
        encoder
            .submit(&source, index * 33_333)
            .expect("gửi frame vào bộ mã hoá");
        let encoded = encoder
            .next_frame(Duration::from_millis(500))
            .expect("nhận frame đã mã hoá");
        decoded = decoder
            .decode(&encoded.data, encoded.pts_us)
            .expect("giải mã frame");
    }
    let decoded = decoded.expect("bộ giải mã phải trả ra frame");

    let mut renderer = VideoRenderer::new(&device, wgpu::TextureFormat::Rgba8Unorm);
    let frame = std::sync::Arc::new(decoded);
    renderer
        .bind_frame(&device, &queue, &frame)
        .expect("nạp frame lên GPU");

    let pixels = render_readback(&device, &queue, &renderer);

    // Lấy mẫu giữa mỗi dải, tránh mép nơi bộ mã hoá làm nhoè ranh giới.
    for (band, expected) in BANDS.iter().enumerate() {
        let y = OUT_H * (2 * band as u32 + 1) / 6;
        let x = OUT_W / 2;
        let offset = ((y * OUT_W + x) * 4) as usize;
        let got = [pixels[offset], pixels[offset + 1], pixels[offset + 2]];
        for channel in 0..3 {
            let diff = got[channel] as i32 - expected[channel] as i32;
            assert!(
                diff.abs() <= TOLERANCE,
                "dải {band} kênh {channel}: mong {} nhận {} (lệch {diff}), cả pixel {:?} vs {:?}",
                expected[channel],
                got[channel],
                got,
                expected,
            );
        }
    }
}

/// Mở device wgpu trần, không cần cửa sổ.
fn open_device() -> Option<(wgpu::Device, wgpu::Queue)> {
    let instance = wgpu::Instance::default();
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        ..Default::default()
    }))
    .ok()?;
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("test"),
        required_features: NEEDED_FOR_10BIT & adapter.features(),
        required_limits: wgpu::Limits::default(),
        ..Default::default()
    }))
    .ok()?;
    Some((device, queue))
}

/// Vẽ frame đang nạp vào một texture rời rồi kéo pixel về CPU.
fn render_readback(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    renderer: &VideoRenderer,
) -> Vec<u8> {
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("readback"),
        size: wgpu::Extent3d {
            width: OUT_W,
            height: OUT_H,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&Default::default());

    let size = (OUT_W * OUT_H * 4) as u64;
    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&Default::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("video"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        renderer.draw(&mut pass);
    }
    encoder.copy_texture_to_buffer(
        target.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &staging,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(OUT_W * 4),
                rows_per_image: Some(OUT_H),
            },
        },
        wgpu::Extent3d {
            width: OUT_W,
            height: OUT_H,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([encoder.finish()]);

    let slice = staging.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .expect("chờ GPU");
    let data = slice.get_mapped_range().expect("ánh xạ buffer").to_vec();
    staging.unmap();
    data
}

/// Ảnh nguồn: ba dải màu ngang, đúng định dạng mà ScreenCaptureKit trả về.
fn make_bgra_bands() -> CFRetained<CVPixelBuffer> {
    let yes = objc2_core_foundation::CFBoolean::new(true);
    let empty: CFRetained<CFDictionary<CFType, CFType>> =
        CFDictionary::from_slices(&[], &[]);
    let keys: [&CFType; 2] = [
        unsafe { kCVPixelBufferIOSurfacePropertiesKey },
        unsafe { kCVPixelBufferMetalCompatibilityKey },
    ];
    let values: [&CFType; 2] = [&empty, yes];
    let attributes = CFDictionary::from_slices(&keys, &values);
    // Hàm C nhận CFDictionary không tham số kiểu; cả hai là cùng một struct rỗng.
    let attributes: &CFDictionary =
        unsafe { &*(&*attributes as *const CFDictionary<CFType, CFType> as *const CFDictionary) };

    let mut raw: *mut CVPixelBuffer = null_mut();
    let status = unsafe {
        CVPixelBufferCreate(
            None,
            WIDTH as usize,
            HEIGHT as usize,
            kCVPixelFormatType_32BGRA,
            Some(attributes),
            NonNull::from(&mut raw),
        )
    };
    assert_eq!(status, 0, "CVPixelBufferCreate lỗi {status}");
    let buffer = unsafe { CFRetained::from_raw(NonNull::new(raw).expect("buffer rỗng")) };

    let flags = CVPixelBufferLockFlags(0);
    assert_eq!(
        unsafe { CVPixelBufferLockBaseAddress(&buffer, flags) },
        0,
        "không khoá được buffer"
    );
    let stride = CVPixelBufferGetBytesPerRow(&buffer);
    let base = CVPixelBufferGetBaseAddress(&buffer).cast::<u8>();
    assert!(!base.is_null(), "không lấy được địa chỉ buffer");
    for y in 0..HEIGHT as usize {
        let color = BANDS[(y * 3 / HEIGHT as usize).min(2)];
        let row = unsafe { base.add(y * stride) };
        for x in 0..WIDTH as usize {
            unsafe {
                let pixel = row.add(x * 4);
                pixel.write(color[2]); // B
                pixel.add(1).write(color[1]); // G
                pixel.add(2).write(color[0]); // R
                pixel.add(3).write(255); // A
            }
        }
    }
    unsafe {
        CVPixelBufferUnlockBaseAddress(&buffer, flags);
    }
    buffer
}
