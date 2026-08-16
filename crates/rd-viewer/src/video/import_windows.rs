//! Đưa frame đã giải mã vào wgpu mà không cho pixel rời VRAM.
//!
//! Chuỗi mắt xích: texture NV12 của bộ giải mã (D3D11) → texture NV12 chia sẻ
//! (D3D11) → NT handle → `ID3D12Resource` → hai `wgpu::Texture`. Mỗi mắt xích
//! có lý do riêng:
//!
//! 1. **Vì sao phải chép sang texture khác.** Bộ giải mã tự cấp mảng texture của
//!    nó, không có cờ chia sẻ, nên không tạo được handle từ đó. Ta chép một lần
//!    trong VRAM sang texture *của ta* có cờ chia sẻ. Frame NV12 1080p nặng 3 MB
//!    và phép chép nằm gọn trong GPU — rẻ hơn nhiều so với đi vòng qua RAM.
//! 2. **Vì sao giữ nguyên NV12 thay vì đổi sang BGRA.** Giữ NV12 thì ảnh nhỏ hơn
//!    2,7 lần, không phải chạy thêm bước chuyển màu, và shader dùng chung được
//!    với bản macOS — cùng hai plane Y/CbCr, cùng ma trận BT.709.
//! 3. **Vì sao hai `wgpu::Texture` cho một tài nguyên.** wgpu không có định dạng
//!    NV12; `with_plane_slice` của wgpu-hal sinh ra đúng để bọc từng plane của
//!    một tài nguyên đa plane thành một texture đơn plane.
//!
//! **Về cảnh báo aliasing của `with_plane_slice`:** hai texture bọc chung một
//! `ID3D12Resource` sẽ làm bộ theo dõi trạng thái của wgpu hiểu nhầm nếu chúng
//! *đổi* trạng thái khác nhau. Ở đây cả hai được khai báo `RESOURCE` ngay từ
//! đầu và suốt đời chỉ bị lấy mẫu trong shader, nên wgpu không phát ra một
//! resource barrier nào — trường hợp duy nhất mà cách bọc này an toàn.

use rd_codec::mediafoundation::DecodedFrame;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_FENCE_FLAG_NONE, D3D11_RESOURCE_MISC_SHARED,
    D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device,
    ID3D11Device5, ID3D11DeviceContext4, ID3D11Fence, ID3D11Texture2D,
};
use windows::Win32::Graphics::Direct3D12::{ID3D12Device, ID3D12Resource};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIResource1,
};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::core::Interface;

/// Số texture chia sẻ luân phiên.
///
/// [`Importer::stage`] đã chờ GPU chép xong mới trả về, nên về phía D3D11 một
/// texture là đủ. Vòng này lo phía bên kia: wgpu có thể còn đang đọc texture
/// của frame trước khi ta đã bắt đầu ghi frame sau, và không có đường nào để
/// D3D11 biết điều đó. Ba ô phủ dư sức độ trễ trình chiếu mà viewer cho phép
/// (`desired_maximum_frame_latency = 1`).
const RING: usize = 3;

/// Bản Windows không có đường 10-bit: bộ mã hoá phần cứng qua Media Foundation
/// chỉ nhận NV12 4:2:0 8-bit, nên không có tính năng GPU nào cần xin thêm.
pub const NEEDED_FOR_10BIT: wgpu::Features = wgpu::Features::empty();

pub fn supports_10bit(_device: &wgpu::Device) -> bool {
    false
}

/// Một ô trong vòng: texture NV12 chia sẻ và hai plane wgpu bọc quanh nó.
struct Slot {
    shared: ID3D11Texture2D,
    planes: [wgpu::Texture; 2],
}

pub struct Importer {
    context: ID3D11DeviceContext4,
    fence: ID3D11Fence,
    /// Giá trị fence của lần chép gần nhất. Tăng dần, không bao giờ lặp lại.
    fence_value: u64,
    /// Sự kiện để chờ fence. Tạo một lần rồi dùng lại — tạo mỗi frame là một
    /// lời gọi kernel thừa 60 lần mỗi giây.
    done: HANDLE,
    ring: Vec<Slot>,
    index: usize,
    width: u32,
    height: u32,
}

// An toàn: mọi thao tác chạm tới device context đều đi qua `&mut self`, nên
// không có hai luồng nào dùng nó cùng lúc. Bản thân device D3D11 là
// free-threaded.
unsafe impl Send for Importer {}
unsafe impl Sync for Importer {}

impl Importer {
    /// Dựng vòng texture theo đúng kích thước của frame đầu tiên.
    ///
    /// Device D3D11 lấy ngay từ texture của frame chứ không nhận từ ngoài: mọi
    /// texture chia sẻ phải nằm cùng device với bộ giải mã, và đây là cách chắc
    /// chắn nhất để không lấy nhầm device khác.
    pub fn new(device: &wgpu::Device, frame: &DecodedFrame) -> anyhow::Result<Self> {
        let (width, height) = (frame.width, frame.height);
        if width % 2 != 0 || height % 2 != 0 {
            anyhow::bail!("NV12 cần kích thước chẵn, nhận {width}x{height}");
        }

        let d3d11 = unsafe { frame.texture().GetDevice() }?;
        let context: ID3D11DeviceContext4 = unsafe { d3d11.GetImmediateContext() }?.cast()?;
        let device5: ID3D11Device5 = d3d11.cast()?;

        let mut fence = None;
        unsafe { device5.CreateFence(0, D3D11_FENCE_FLAG_NONE, &mut fence) }?;
        let fence: ID3D11Fence = fence.expect("CreateFence thành công thì phải có fence");

        // Sự kiện tự đặt lại (auto-reset): mỗi lần chờ xong nó tự về trạng thái
        // chưa báo, khỏi phải gọi `ResetEvent` giữa hai frame.
        let done = unsafe { CreateEventW(None, false, false, None) }?;

        let d3d12: ID3D12Device = unsafe {
            device
                .as_hal::<wgpu::hal::api::Dx12>()
                .ok_or_else(|| anyhow::anyhow!("wgpu không chạy trên Direct3D 12"))?
                .raw_device()
                .clone()
        };

        let mut ring = Vec::with_capacity(RING);
        for _ in 0..RING {
            ring.push(Slot::new(device, &d3d11, &d3d12, width, height)?);
        }

        Ok(Self {
            context,
            fence,
            fence_value: 0,
            done,
            ring,
            index: 0,
            width,
            height,
        })
    }

    pub fn matches(&self, frame: &DecodedFrame) -> bool {
        self.width == frame.width && self.height == frame.height
    }

    /// Chép frame vào ô kế tiếp và chờ GPU chép xong. Trả về chỉ số ô.
    ///
    /// Phải chờ thật: lệnh chép nằm ở hàng đợi của D3D11 còn lệnh vẽ nằm ở hàng
    /// đợi của D3D12, hai hàng đợi khác nhau nên không có thứ tự nào được bảo
    /// đảm giữa chúng. Không chờ thì thỉnh thoảng viewer vẽ nửa frame cũ nửa
    /// frame mới, và lỗi kiểu đó gần như không tài nào tái hiện được.
    pub fn stage(&mut self, frame: &DecodedFrame) -> anyhow::Result<usize> {
        if !self.matches(frame) {
            anyhow::bail!(
                "frame {}x{} không khớp vòng texture {}x{}",
                frame.width,
                frame.height,
                self.width,
                self.height
            );
        }

        let index = self.index % self.ring.len();
        self.index = self.index.wrapping_add(1);

        unsafe {
            // Định dạng phẳng thì một lời gọi chép cả plane Y lẫn plane CbCr;
            // `subresource` là lát mảng mà bộ giải mã đặt frame này vào.
            self.context.CopySubresourceRegion(
                &self.ring[index].shared,
                0,
                0,
                0,
                0,
                frame.texture(),
                frame.subresource(),
                None,
            );

            self.fence_value += 1;
            self.context.Signal(&self.fence, self.fence_value)?;
            // `Flush` mới thực sự đẩy lệnh xuống driver; thiếu nó thì fence có
            // thể nằm im trong bộ đệm lệnh và ta chờ vô hạn.
            self.context.Flush();
            self.fence.SetEventOnCompletion(self.fence_value, self.done)?;
            // Chờ hỏng thì frame chưa chắc đã chép xong; vẽ tiếp là vẽ rác nên
            // thà bỏ frame còn hơn.
            if WaitForSingleObject(self.done, INFINITE) != WAIT_OBJECT_0 {
                anyhow::bail!("chờ GPU chép frame thất bại");
            }
        }

        Ok(index)
    }

    pub fn planes(&self, index: usize) -> &[wgpu::Texture; 2] {
        &self.ring[index].planes
    }
}

impl Drop for Importer {
    fn drop(&mut self) {
        // Handle sự kiện là tài nguyên kernel, không phải đối tượng COM nên
        // không có ai đếm tham chiếu hộ.
        let _ = unsafe { CloseHandle(self.done) };
    }
}

impl Slot {
    fn new(
        device: &wgpu::Device,
        d3d11: &ID3D11Device,
        d3d12: &ID3D12Device,
        width: u32,
        height: u32,
    ) -> anyhow::Result<Self> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            // `SHARED_NTHANDLE` là thứ cho phép tạo handle mở được ở D3D12;
            // riêng nó không đủ, phải đi kèm `SHARED`.
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0)
                as u32,
        };

        let mut texture = None;
        unsafe { d3d11.CreateTexture2D(&desc, None, Some(&mut texture)) }?;
        let texture: ID3D11Texture2D =
            texture.expect("CreateTexture2D thành công thì phải có texture");

        let sharable: IDXGIResource1 = texture.cast()?;
        let handle = unsafe {
            sharable.CreateSharedHandle(
                None,
                DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                None,
            )
        }?;

        let mut resource: Option<ID3D12Resource> = None;
        let opened = unsafe { d3d12.OpenSharedHandle(handle, &mut resource) };
        // D3D12 đã giữ tham chiếu riêng, nên handle xong việc ngay tại đây.
        // Đóng cả khi mở hỏng, không thì rò một handle mỗi lần thử.
        let _ = unsafe { CloseHandle(handle) };
        opened?;
        let resource = resource.expect("OpenSharedHandle thành công thì phải có tài nguyên");

        let luma = plane(
            device,
            resource.clone(),
            wgpu::TextureFormat::R8Unorm,
            width,
            height,
            0,
            "video plane Y",
        );
        // Plane CbCr của NV12 nhỏ bằng nửa theo cả hai chiều, hai kênh xen kẽ.
        let chroma = plane(
            device,
            resource,
            wgpu::TextureFormat::Rg8Unorm,
            width / 2,
            height / 2,
            1,
            "video plane CbCr",
        );

        Ok(Self {
            shared: texture,
            planes: [luma, chroma],
        })
    }
}

/// Bọc một plane của tài nguyên NV12 thành texture wgpu đơn plane.
fn plane(
    device: &wgpu::Device,
    resource: ID3D12Resource,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
    slice: u32,
    label: &str,
) -> wgpu::Texture {
    let size = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };

    // SAFETY: tài nguyên vừa mở từ chính device D3D12 của wgpu, là texture 2D
    // NV12 một mip một lát đúng kích thước khai báo, và `slice` trỏ tới plane
    // có bố cục texel khớp `format`.
    let hal = unsafe {
        wgpu::hal::dx12::Device::texture_from_raw(
            resource,
            format,
            wgpu::TextureDimension::D2,
            size,
            1,
            1,
        )
    }
    .with_plane_slice(slice);

    unsafe {
        device.create_texture_from_hal::<wgpu::hal::api::Dx12>(
            hal,
            &wgpu::TextureDescriptor {
                label: Some(label),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
            // Bộ giải mã đã ghi xong và ta chỉ đọc. Khai báo `UNINITIALIZED` sẽ
            // cho wgpu quyền coi nội dung là rác và vứt đi.
            wgpu::TextureUses::RESOURCE,
        )
    }
}
