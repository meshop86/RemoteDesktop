//! Đưa frame đã giải mã vào wgpu **không sao chép một byte nào**.
//!
//! Chuỗi mắt xích: `CVPixelBuffer` → `IOSurface` → `MTLTexture` → `wgpu::Texture`.
//! IOSurface là vùng nhớ dùng chung giữa các tiến trình và giữa CPU/GPU; cả bộ
//! giải mã lẫn Metal đều trỏ vào đúng vùng nhớ đó. Frame 4K 10-bit nặng 25 MB —
//! ở 60 fps mà copy thì riêng việc chuyển dữ liệu đã ngốn 1,5 GB/s và cộng thêm
//! hàng mili giây cho mỗi frame.
//!
//! `wgpu::Device::create_texture_from_hal` là cửa duy nhất để đưa một texture có
//! sẵn vào wgpu, nên phần này buộc phải đi xuống lớp `wgpu::hal`.

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetHeightOfPlane, CVPixelBufferGetIOSurface,
    CVPixelBufferGetWidthOfPlane,
};
use objc2_metal::{
    MTLDevice, MTLPixelFormat, MTLStorageMode, MTLTextureDescriptor, MTLTextureType,
    MTLTextureUsage,
};
use rd_codec::FrameFormat;

/// Mô tả một plane cần nhập: chỉ số plane trong IOSurface và định dạng texture.
struct PlaneSpec {
    index: usize,
    metal: MTLPixelFormat,
    wgpu: wgpu::TextureFormat,
}

fn plane_specs(format: FrameFormat) -> [PlaneSpec; 2] {
    match format {
        FrameFormat::Nv12VideoRange => [
            PlaneSpec {
                index: 0,
                metal: MTLPixelFormat::R8Unorm,
                wgpu: wgpu::TextureFormat::R8Unorm,
            },
            PlaneSpec {
                index: 1,
                metal: MTLPixelFormat::RG8Unorm,
                wgpu: wgpu::TextureFormat::Rg8Unorm,
            },
        ],
        FrameFormat::P210VideoRange => [
            PlaneSpec {
                index: 0,
                metal: MTLPixelFormat::R16Unorm,
                wgpu: wgpu::TextureFormat::R16Unorm,
            },
            PlaneSpec {
                index: 1,
                metal: MTLPixelFormat::RG16Unorm,
                wgpu: wgpu::TextureFormat::Rg16Unorm,
            },
        ],
    }
}

/// Địa chỉ IOSurface, dùng làm khoá cache.
///
/// Bộ giải mã lấy buffer từ một pool và tái sử dụng vài IOSurface cố định, nên
/// khoá này lặp lại liên tục — nhờ đó cache gần như luôn trúng và ta không phải
/// tạo `MTLTexture` mới cho từng frame.
pub fn surface_key(pixels: &CVPixelBuffer) -> Option<usize> {
    let surface = CVPixelBufferGetIOSurface(Some(pixels))?;
    Some(&*surface as *const _ as usize)
}

/// Nhập hai plane của frame thành hai texture wgpu dùng chung bộ nhớ với frame.
pub fn import_planes(
    device: &wgpu::Device,
    pixels: &CVPixelBuffer,
    format: FrameFormat,
) -> anyhow::Result<[wgpu::Texture; 2]> {
    let surface = CVPixelBufferGetIOSurface(Some(pixels))
        .ok_or_else(|| anyhow::anyhow!("frame không nằm trên IOSurface"))?;

    // Mượn MTLDevice mà wgpu đang dùng. Tạo texture từ device khác sẽ không
    // dùng được trong render pass của wgpu.
    let metal_device: Retained<ProtocolObject<dyn MTLDevice>> = unsafe {
        device
            .as_hal::<wgpu::hal::api::Metal>()
            .ok_or_else(|| anyhow::anyhow!("wgpu không chạy trên Metal"))?
            .raw_device()
            .clone()
    };

    // Bộ nhớ thống nhất (Apple silicon) thì CPU và GPU nhìn chung một vùng nhớ;
    // GPU rời cần chế độ Managed để driver tự đồng bộ hai bản sao.
    let storage = if metal_device.hasUnifiedMemory() {
        MTLStorageMode::Shared
    } else {
        MTLStorageMode::Managed
    };

    let mut out: Vec<wgpu::Texture> = Vec::with_capacity(2);
    for spec in plane_specs(format) {
        let width = CVPixelBufferGetWidthOfPlane(pixels, spec.index) as u32;
        let height = CVPixelBufferGetHeightOfPlane(pixels, spec.index) as u32;
        if width == 0 || height == 0 {
            anyhow::bail!("plane {} rỗng", spec.index);
        }

        let descriptor = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                spec.metal,
                width as usize,
                height as usize,
                false,
            )
        };
        descriptor.setUsage(MTLTextureUsage::ShaderRead);
        descriptor.setStorageMode(storage);

        let metal_texture = metal_device
            .newTextureWithDescriptor_iosurface_plane(&descriptor, &surface, spec.index)
            .ok_or_else(|| anyhow::anyhow!("Metal từ chối bọc plane {}", spec.index))?;

        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        // SAFETY: texture vừa tạo từ chính MTLDevice của wgpu, đúng kích thước
        // và định dạng khai báo bên dưới; MTLTexture giữ IOSurface sống nên
        // vùng nhớ không biến mất khi frame gốc bị thả.
        let hal_texture = unsafe {
            wgpu::hal::metal::Device::texture_from_raw(
                metal_texture,
                spec.wgpu,
                MTLTextureType::Type2D,
                1,
                1,
                wgpu::hal::CopyExtent {
                    width,
                    height,
                    depth: 1,
                },
                None,
            )
        };

        let texture = unsafe {
            device.create_texture_from_hal::<wgpu::hal::api::Metal>(
                hal_texture,
                &wgpu::TextureDescriptor {
                    label: Some("video plane"),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: spec.wgpu,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                // Trạng thái thật của texture ngay lúc bọc: bộ giải mã đã ghi
                // xong và ta chỉ đọc. Khai báo UNINITIALIZED sẽ khiến wgpu tưởng
                // nội dung là rác và được phép vứt đi.
                wgpu::TextureUses::RESOURCE,
            )
        };
        out.push(texture);
    }

    let mut it = out.into_iter();
    Ok([
        it.next().expect("đã đẩy đúng 2 plane"),
        it.next().expect("đã đẩy đúng 2 plane"),
    ])
}

/// Texture 16-bit chuẩn hoá không phải nền tảng nào cũng có; thiếu nó thì đường
/// 10-bit 4:2:2 không dùng được và phải lùi về NV12 8-bit.
pub const NEEDED_FOR_10BIT: wgpu::Features = wgpu::Features::TEXTURE_FORMAT_16BIT_NORM;

pub fn supports_10bit(device: &wgpu::Device) -> bool {
    device.features().contains(NEEDED_FOR_10BIT)
}
