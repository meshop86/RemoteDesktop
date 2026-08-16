//! Nguồn frame tổng hợp — thay cho màn hình thật.
//!
//! Có hai lý do cần nó, không chỉ để test:
//!
//! 1. Quyền quay màn hình do người dùng cấp bằng tay. Toàn bộ phần sau capture
//!    (encode, truyền, giải mã, render) phải phát triển và đo được ngay cả khi
//!    quyền chưa có.
//! 2. Nội dung sinh ra là *cố định*, nên số liệu đo giữa hai lần chạy so sánh
//!    được với nhau. Màn hình thật thì mỗi lần một khác.
//!
//! Frame nằm trên đúng loại bộ nhớ mà hệ thống thật trả về — IOSurface trên
//! macOS, `ID3D11Texture2D` trên Windows — nên đường đi xuống encoder không khác
//! gì lúc chạy thật. Chỉ khâu *đổ pixel vào* là qua CPU, và đó là chỗ duy nhất
//! khác: màn hình thật không có bước này.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::{
    CaptureConfig, CaptureError, CaptureStats, CapturedFrame, DisplayInfo, Result, ScreenCapturer,
};

/// Số buffer luân phiên. Cần ít nhất 2 để không ghi đè lên frame mà encoder
/// đang đọc; 3 cho encoder thêm một nhịp thở.
const RING: usize = 3;

pub struct SyntheticCapturer {
    config: CaptureConfig,
    width: u32,
    height: u32,
    ring: Ring,
    index: u64,
    started: Instant,
    stats: CaptureStats,
    stopped: bool,
}

// Bên trong là handle của hệ thống (`CVPixelBuffer` hoặc con trỏ COM D3D11).
// Cả hai đều đếm tham chiếu nguyên tử và không gắn với luồng nào, nhưng kiểu
// gốc không tự khai `Send`. Ta chuyển quyền sở hữu sang luồng encode chứ không
// dùng đồng thời hai luồng, nên chỉ `Send`, không `Sync`.
unsafe impl Send for SyntheticCapturer {}

impl SyntheticCapturer {
    fn frame_gap(&self) -> Duration {
        Duration::from_micros(1_000_000 / self.config.target_fps.max(1) as u64)
    }
}

impl ScreenCapturer for SyntheticCapturer {
    fn list_displays() -> Result<Vec<DisplayInfo>> {
        Ok(vec![DisplayInfo {
            id: 0,
            name: "Màn hình tổng hợp".into(),
            width: 1920,
            height: 1080,
            scale: 1.0,
            refresh_hz: 60,
            is_primary: true,
        }])
    }

    fn start(config: CaptureConfig) -> Result<Self> {
        // `max_dimension` ở đây quyết định luôn kích thước frame, vì không có
        // màn hình thật nào để lấy kích thước gốc.
        let (width, height) = match config.max_dimension {
            Some(long_edge) => (long_edge, long_edge * 9 / 16),
            None => (1920, 1080),
        };
        // Chiều rộng lẻ làm hỏng bước chia đôi chroma của bộ mã hoá.
        let width = width & !1;
        let height = height & !1;

        let ring = Ring::new(width, height, RING)?;

        Ok(Self {
            config,
            width,
            height,
            ring,
            index: 0,
            started: Instant::now(),
            stats: CaptureStats::default(),
            stopped: false,
        })
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<CapturedFrame> {
        if self.stopped {
            return Err(CaptureError::Stopped);
        }

        // Giữ đúng nhịp frame để phía sau chịu tải giống lúc chạy thật.
        let deadline = self.started + self.frame_gap() * self.index as u32;
        let now = Instant::now();
        if deadline > now {
            let wait = deadline - now;
            if wait > timeout {
                return Err(CaptureError::Timeout);
            }
            std::thread::sleep(wait);
        }

        let surface = self.ring.produce(self.index, self.width, self.height)?;
        let frame = CapturedFrame {
            width: self.width,
            height: self.height,
            capture_us: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros() as u64,
            frame_index: self.index,
            surface,
        };
        self.index += 1;
        self.stats.frames_delivered += 1;
        Ok(frame)
    }

    fn stats(&self) -> CaptureStats {
        self.stats
    }

    fn stop(&mut self) {
        self.stopped = true;
    }
}

/// Vẽ nội dung mô phỏng một desktop đang dùng, vào bộ đệm BGRA cho sẵn.
///
/// Nền chuyển màu tĩnh (gần như miễn phí sau frame đầu), một cửa sổ chạy ngang
/// (chuyển động thật để encoder có việc làm), và các sọc dọc 1 pixel mô phỏng
/// nét chữ — đây chính là chi tiết mà 4:2:0 làm nhoè còn 4:2:2 giữ được.
///
/// `stride` là số byte một hàng chiếm trong bộ đệm, thường lớn hơn
/// `width * 4` vì hệ thống căn hàng theo bội số nào đó.
fn paint(pixels: &mut [u8], stride: usize, width: u32, height: u32, index: u32) {
    let block_w = (width / 4).max(1);
    let block_h = (height / 4).max(1);
    let block_x = ((index * 7) % (width - block_w).max(1)) as usize;
    let block_y = ((index * 3) % (height - block_h).max(1)) as usize;
    let text_top = height as usize / 2;

    for y in 0..height as usize {
        let start = y * stride;
        let row = &mut pixels[start..start + width as usize * 4];
        let shade = (y * 255 / height as usize) as u8;
        let in_block_row = y >= block_y && y < block_y + block_h as usize;

        for x in 0..width as usize {
            let px = &mut row[x * 4..x * 4 + 4];
            let (b, g, r) = if in_block_row && x >= block_x && x < block_x + block_w as usize {
                (40u8, 200u8, 90u8)
            } else if y > text_top && (x + index as usize) % 3 == 0 {
                (250, 250, 250)
            } else {
                (shade / 3, shade / 2, shade)
            };
            px[0] = b;
            px[1] = g;
            px[2] = r;
            px[3] = 255;
        }
    }
}

#[cfg(target_os = "macos")]
mod backend {
    //! Vòng `CVPixelBuffer` trên IOSurface — cùng loại buffer mà
    //! ScreenCaptureKit giao cho ta.

    use std::ptr::{NonNull, null_mut};

    use objc2_core_foundation::{CFDictionary, CFRetained, CFType};
    use objc2_core_video::{
        CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
        CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
        kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey,
        kCVPixelFormatType_32BGRA,
    };

    use crate::macos::PixelSurface;
    use crate::{CaptureError, Result};

    pub(super) struct Ring {
        buffers: Vec<CFRetained<CVPixelBuffer>>,
    }

    impl Ring {
        pub(super) fn new(width: u32, height: u32, count: usize) -> Result<Self> {
            let mut buffers = Vec::with_capacity(count);
            for _ in 0..count {
                buffers.push(make_pixel_buffer(width, height)?);
            }
            Ok(Self { buffers })
        }

        pub(super) fn produce(
            &mut self,
            index: u64,
            width: u32,
            height: u32,
        ) -> Result<PixelSurface> {
            let buffer = &self.buffers[index as usize % self.buffers.len()];
            let flags = CVPixelBufferLockFlags(0);
            let status = unsafe { CVPixelBufferLockBaseAddress(buffer, flags) };
            if status != 0 {
                return Err(CaptureError::Platform(format!(
                    "không khoá được pixel buffer: CVReturn {status}"
                )));
            }

            let base = CVPixelBufferGetBaseAddress(buffer).cast::<u8>();
            let stride = CVPixelBufferGetBytesPerRow(buffer);
            if base.is_null() {
                unsafe { CVPixelBufferUnlockBaseAddress(buffer, flags) };
                return Err(CaptureError::Platform("pixel buffer không có địa chỉ".into()));
            }

            // An toàn: buffer đang bị khoá nên hệ thống không đụng tới, và
            // `stride * height` đúng bằng vùng CoreVideo cấp cho ảnh BGRA.
            let pixels =
                unsafe { std::slice::from_raw_parts_mut(base, stride * height as usize) };
            super::paint(pixels, stride, width, height, index as u32);

            unsafe { CVPixelBufferUnlockBaseAddress(buffer, flags) };
            Ok(PixelSurface::from_retained(buffer.clone()))
        }
    }

    /// `CVPixelBuffer` BGRA có nền IOSurface tương thích Metal.
    fn make_pixel_buffer(width: u32, height: u32) -> Result<CFRetained<CVPixelBuffer>> {
        let empty: CFRetained<CFDictionary<CFType, CFType>> = CFDictionary::from_slices(&[], &[]);
        let yes = objc2_core_foundation::CFBoolean::new(true);
        let keys: [&CFType; 2] = [
            unsafe { kCVPixelBufferIOSurfacePropertiesKey },
            unsafe { kCVPixelBufferMetalCompatibilityKey },
        ];
        let values: [&CFType; 2] = [&empty, yes];
        let attributes = CFDictionary::from_slices(&keys, &values);
        // Hàm C nhận CFDictionary không tham số kiểu; cả hai là cùng một struct rỗng.
        let attributes: &CFDictionary = unsafe {
            &*(&*attributes as *const CFDictionary<CFType, CFType> as *const CFDictionary)
        };

        let mut raw: *mut CVPixelBuffer = null_mut();
        let status = unsafe {
            CVPixelBufferCreate(
                None,
                width as usize,
                height as usize,
                kCVPixelFormatType_32BGRA,
                Some(attributes),
                NonNull::from(&mut raw),
            )
        };
        let Some(raw) = NonNull::new(raw).filter(|_| status == 0) else {
            return Err(CaptureError::Platform(format!(
                "không tạo được pixel buffer {width}x{height}: CVReturn {status}"
            )));
        };
        Ok(unsafe { CFRetained::from_raw(raw) })
    }
}

#[cfg(target_os = "windows")]
mod backend {
    //! Vòng `ID3D11Texture2D` — cùng loại texture mà Windows Graphics Capture
    //! giao cho ta, nên encoder không phân biệt được nguồn thật hay tổng hợp.

    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP,
    };
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11CreateDevice,
        ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

    use crate::windows::D3dSurface;
    use crate::{CaptureError, Result};

    pub(super) struct Ring {
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        textures: Vec<ID3D11Texture2D>,
        /// Bộ đệm CPU dùng lại giữa các frame — cấp phát 8 MB mỗi frame thì
        /// số đo độ trễ chỉ còn phản ánh bộ cấp phát.
        scratch: Vec<u8>,
        stride: usize,
    }

    impl Ring {
        pub(super) fn new(width: u32, height: u32, count: usize) -> Result<Self> {
            let (device, context) = create_device()?;
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };

            let mut textures = Vec::with_capacity(count);
            for _ in 0..count {
                let mut texture = None;
                unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }.map_err(
                    |err| {
                        CaptureError::Platform(format!(
                            "không tạo được texture {width}x{height}: {err}"
                        ))
                    },
                )?;
                textures.push(texture.expect("CreateTexture2D thành công thì phải có texture"));
            }

            let stride = width as usize * 4;
            Ok(Self {
                device,
                context,
                textures,
                scratch: vec![0; stride * height as usize],
                stride,
            })
        }

        pub(super) fn produce(&mut self, index: u64, width: u32, height: u32) -> Result<D3dSurface> {
            super::paint(&mut self.scratch, self.stride, width, height, index as u32);

            let texture = &self.textures[index as usize % self.textures.len()];
            // `UpdateSubresource` chép thẳng lên texture USAGE_DEFAULT, không
            // cần texture staging trung gian.
            unsafe {
                self.context.UpdateSubresource(
                    texture,
                    0,
                    None,
                    self.scratch.as_ptr().cast(),
                    self.stride as u32,
                    0,
                );
            }

            Ok(D3dSurface::new(
                texture.clone(),
                self.device.clone(),
                width,
                height,
            ))
        }
    }

    /// Mở device D3D11, thử GPU thật trước rồi mới tới bộ dựng hình phần mềm.
    ///
    /// WARP chậm hơn nhiều nhưng máy ảo và máy CI thường không có GPU — thà
    /// chạy chậm còn hơn không chạy được nguồn tổng hợp.
    fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
        let mut last = String::new();
        for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
            match try_create_device(driver) {
                Ok(pair) => return Ok(pair),
                Err(err) => last = err.to_string(),
            }
        }
        Err(CaptureError::Platform(format!(
            "không mở được device D3D11: {last}"
        )))
    }

    fn try_create_device(
        driver: D3D_DRIVER_TYPE,
    ) -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                None,
                driver,
                HMODULE::default(),
                // BGRA_SUPPORT là bắt buộc để dùng chung tài nguyên với Direct2D
                // và với bộ mã hoá Media Foundation.
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        Ok((
            device.expect("D3D11CreateDevice thành công thì phải có device"),
            context.expect("D3D11CreateDevice thành công thì phải có context"),
        ))
    }
}

use backend::Ring;

#[cfg(test)]
mod tests {
    use super::paint;

    const W: u32 = 64;
    const H: u32 = 32;
    /// Cố tình để thừa so với `W * 4`: hệ thống thật hay căn hàng, và bug hay
    /// nằm đúng chỗ dùng nhầm `width * 4` làm stride.
    const STRIDE: usize = W as usize * 4 + 16;

    fn render(index: u32) -> Vec<u8> {
        let mut pixels = vec![0u8; STRIDE * H as usize];
        paint(&mut pixels, STRIDE, W, H, index);
        pixels
    }

    /// Encoder chỉ có việc làm khi hình thực sự đổi. Hai frame giống hệt nhau
    /// biến phép đo thành đo tốc độ nén ảnh tĩnh.
    #[test]
    fn hai_frame_lien_tiep_khac_nhau() {
        assert_ne!(render(0), render(1));
    }

    /// Alpha phải đầy: pixel trong suốt lọt xuống encoder sẽ ra hình đen.
    #[test]
    fn moi_pixel_deu_duc() {
        let pixels = render(3);
        for y in 0..H as usize {
            for x in 0..W as usize {
                assert_eq!(pixels[y * STRIDE + x * 4 + 3], 255, "pixel ({x}, {y})");
            }
        }
    }

    /// Phần đệm cuối mỗi hàng không thuộc về ảnh — ghi vào đó là đang tràn
    /// sang hàng sau trên bộ đệm có stride khít.
    #[test]
    fn khong_ghi_ra_ngoai_phan_dem_cuoi_hang() {
        let pixels = render(5);
        for y in 0..H as usize {
            let padding = &pixels[y * STRIDE + W as usize * 4..(y + 1) * STRIDE];
            assert!(padding.iter().all(|&b| b == 0), "hàng {y} ghi tràn");
        }
    }
}
