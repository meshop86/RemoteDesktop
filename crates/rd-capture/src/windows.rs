//! Capture màn hình Windows bằng Windows Graphics Capture (WGC).
//!
//! Luồng dữ liệu giống hệt bản macOS, chỉ khác tên API: WGC gọi callback trên
//! luồng riêng của nó và đưa cho ta một `ID3D11Texture2D` nằm trong VRAM. Ta
//! không đọc pixel; chỉ chép texture đó sang texture của mình bằng
//! `CopyResource` — một lệnh chạy hoàn toàn trên GPU — rồi đặt handle vào
//! [`FrameSlot`].
//!
//! **Vì sao phải chép mà không giữ thẳng texture của WGC:** frame pool tái sử
//! dụng một số texture cố định. Giữ nguyên handle thì vài frame sau WGC ghi đè
//! lên chính nó và encoder đọc phải hình lẫn lộn. Bản chép nằm trong vòng của
//! ta nên encoder có thời gian dùng xong.
//!
//! Hai điểm khác biệt so với macOS, cố ý để lộ ra chứ không giấu:
//!
//! - WGC chỉ cho ba định dạng RGBA/BGRA, không có NV12. Xin
//!   [`PixelFormat::Nv12`] sẽ bị từ chối thay vì lặng lẽ trả BGRA.
//! - WGC luôn quay ở phân giải gốc. Muốn thu nhỏ phải thêm một lượt vẽ trên
//!   GPU; chưa làm, nên `max_dimension` nhỏ hơn màn hình cũng bị từ chối.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows_capture::capture::{
    CaptureControl, Context, GraphicsCaptureApiError, GraphicsCaptureApiHandler,
};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::{
    Error as GraphicsCaptureError, GraphicsCaptureApi, InternalCaptureControl,
};
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

use crate::{
    CaptureConfig, CaptureError, CaptureStats, CapturedFrame, DisplayInfo, FrameSlot, PixelFormat,
    Result, ScreenCapturer,
};

/// Số texture luân phiên tối thiểu. Cần ít nhất 2 để không ghi đè lên frame mà
/// encoder đang đọc; 3 cho encoder thêm một nhịp thở.
const MIN_RING: usize = 3;

/// Handle tới texture GPU chứa frame. Không sao chép pixel.
///
/// Mang theo cả device vì bộ mã hoá phải được tạo trên **đúng** device đã sinh
/// ra texture — D3D11 không cho hai device dùng chung tài nguyên nếu tài nguyên
/// đó không được khai báo shared.
pub struct D3dSurface {
    texture: ID3D11Texture2D,
    device: ID3D11Device,
    width: u32,
    height: u32,
}

// An toàn: đây là con trỏ COM apartment-agnostic (free-threaded), đếm tham
// chiếu nguyên tử. Ta chỉ chuyển quyền sở hữu từ luồng callback sang luồng
// encode, không truy cập đồng thời từ hai luồng.
unsafe impl Send for D3dSurface {}

impl D3dSurface {
    pub(crate) fn new(
        texture: ID3D11Texture2D,
        device: ID3D11Device,
        width: u32,
        height: u32,
    ) -> Self {
        Self {
            texture,
            device,
            width,
            height,
        }
    }

    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.texture
    }

    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

/// Những gì luồng gọi truyền sang luồng callback của WGC.
struct Flags {
    slot: Arc<FrameSlot<CapturedFrame>>,
    ring_size: usize,
}

/// Bộ nhận callback của WGC.
struct Handler {
    slot: Arc<FrameSlot<CapturedFrame>>,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    ring: Vec<ID3D11Texture2D>,
    ring_size: usize,
    /// Kích thước của vòng texture hiện tại. Người dùng đổi phân giải giữa
    /// chừng thì WGC đổi kích thước frame và ta phải dựng lại vòng.
    size: Option<(u32, u32)>,
    index: u64,
}

impl Handler {
    /// Dựng lại vòng texture cho kích thước mới.
    fn rebuild_ring(&mut self, width: u32, height: u32) -> Result<()> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            // Khớp với `ColorFormat::Bgra8` xin của WGC; `CopyResource` đòi
            // hai texture cùng định dạng.
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            // Encoder đọc texture như shader resource; RENDER_TARGET để dành
            // cho bước thu nhỏ trên GPU về sau.
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };

        let mut ring = Vec::with_capacity(self.ring_size);
        for _ in 0..self.ring_size {
            let mut texture = None;
            unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut texture)) }.map_err(
                |err| {
                    CaptureError::Platform(format!(
                        "không tạo được texture {width}x{height}: {err}"
                    ))
                },
            )?;
            ring.push(texture.expect("CreateTexture2D thành công thì phải có texture"));
        }

        self.ring = ring;
        self.size = Some((width, height));
        Ok(())
    }
}

impl GraphicsCaptureApiHandler for Handler {
    type Flags = Flags;
    type Error = CaptureError;

    fn new(ctx: Context<Self::Flags>) -> std::result::Result<Self, Self::Error> {
        Ok(Self {
            slot: ctx.flags.slot,
            device: ctx.device,
            context: ctx.device_context,
            ring: Vec::new(),
            ring_size: ctx.flags.ring_size,
            size: None,
            index: 0,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        capture_control: InternalCaptureControl,
    ) -> std::result::Result<(), Self::Error> {
        let (width, height) = (frame.width(), frame.height());
        if self.size != Some((width, height)) {
            if let Err(err) = self.rebuild_ring(width, height) {
                // Không dựng được vòng thì capture vô nghĩa; dừng hẳn để bên
                // tiêu thụ nhận lỗi thay vì chờ mãi.
                self.slot.stop(Some(err.to_string()));
                capture_control.stop();
                return Err(err);
            }
        }

        let slot_index = (self.index as usize) % self.ring.len();
        let target = &self.ring[slot_index];
        // Lệnh chạy trên GPU, pixel không đi qua CPU.
        unsafe {
            self.context.CopyResource(target, frame.as_raw_texture());
        }

        self.index += 1;
        self.slot.put(CapturedFrame {
            width,
            height,
            capture_us: now_us(),
            frame_index: self.index,
            surface: D3dSurface {
                texture: target.clone(),
                device: self.device.clone(),
                width,
                height,
            },
        });
        Ok(())
    }

    fn on_closed(&mut self) -> std::result::Result<(), Self::Error> {
        self.slot.stop(None);
        Ok(())
    }
}

pub struct WgcScreenCapturer {
    /// `None` sau khi [`stop`](ScreenCapturer::stop) — `CaptureControl::stop`
    /// tiêu thụ chính nó nên phải lấy giá trị ra khỏi struct.
    control: Option<CaptureControl<Handler, CaptureError>>,
    slot: Arc<FrameSlot<CapturedFrame>>,
    display: DisplayInfo,
}

impl WgcScreenCapturer {
    pub fn display(&self) -> &DisplayInfo {
        &self.display
    }
}

/// Đổi lỗi khởi động của WGC thành lỗi của ta.
fn classify_start(err: GraphicsCaptureApiError<CaptureError>) -> CaptureError {
    match err {
        GraphicsCaptureApiError::NewHandlerError(err)
        | GraphicsCaptureApiError::FrameHandlerError(err) => err,
        GraphicsCaptureApiError::GraphicsCaptureApiError(GraphicsCaptureError::Unsupported) => {
            CaptureError::Unsupported
        }
        other => CaptureError::Platform(other.to_string()),
    }
}

/// Hỏi WGC xem một tuỳ chọn có dùng được trên bản Windows này không.
///
/// Cần hỏi trước vì WGC **báo lỗi lúc khởi động** khi ta xin một tuỳ chọn nó
/// không có, chứ không lặng lẽ bỏ qua — xin bừa là phần mềm không chạy nổi trên
/// Windows 10. Hỏi hỏng thì coi như không có: mất một tính năng phụ còn hơn mất
/// cả phiên điều khiển.
fn supported(query: fn() -> std::result::Result<bool, GraphicsCaptureError>) -> bool {
    query().unwrap_or(false)
}

fn describe(monitor: &Monitor, index: usize, target_fps: u32) -> DisplayInfo {
    // Mọi trường đều có thể đọc hỏng (màn hình vừa bị rút chẳng hạn); rơi về
    // giá trị hợp lý còn hơn làm hỏng cả danh sách vì một màn hình.
    DisplayInfo {
        id: index as u32,
        name: monitor
            .name()
            .unwrap_or_else(|_| format!("Màn hình {}", index + 1)),
        // Kích thước ở đây chỉ để hiển thị: kích thước thật của frame lấy từ
        // texture mà WGC giao, vì `GetMonitorInfo` bị DPI scaling bóp lại.
        width: monitor.width().unwrap_or(0),
        height: monitor.height().unwrap_or(0),
        scale: 1.0,
        refresh_hz: monitor.refresh_rate().unwrap_or(target_fps),
        is_primary: index == 0,
    }
}

impl ScreenCapturer for WgcScreenCapturer {
    fn list_displays() -> Result<Vec<DisplayInfo>> {
        let monitors = Monitor::enumerate()
            .map_err(|err| CaptureError::Platform(format!("không liệt kê được màn hình: {err}")))?;
        Ok(monitors
            .iter()
            .enumerate()
            .map(|(index, monitor)| describe(monitor, index, 60))
            .collect())
    }

    fn start(config: CaptureConfig) -> Result<Self> {
        if config.pixel_format == PixelFormat::Nv12 {
            return Err(CaptureError::Platform(
                "Windows Graphics Capture không trả NV12; dùng Bgra32 và để bộ mã hoá đổi màu"
                    .into(),
            ));
        }

        let index = config.display_id as usize;
        // `from_index` của windows-capture đếm từ **1**, còn `display_id` của ta
        // đếm từ 0 cho khớp chỉ số trong danh sách `list_displays` trả về. Quên
        // cộng 1 thì màn hình mặc định (id 0) không bao giờ mở được, mà lỗi đó
        // không nổ ra ở đâu cả: chuỗi mã hoá lặng lẽ lùi về nguồn tổng hợp và
        // người xem ngồi nhìn hình giả, tưởng phần mềm chạy đúng.
        let monitor = Monitor::from_index(index + 1)
            .map_err(|_| CaptureError::DisplayNotFound(config.display_id))?;
        let display = describe(&monitor, index, config.target_fps);

        if let Some(limit) = config.max_dimension {
            let long_edge = display.width.max(display.height);
            if long_edge > limit {
                return Err(CaptureError::Platform(format!(
                    "chưa thu nhỏ được trên Windows: màn hình {long_edge}px, xin {limit}px"
                )));
            }
        }

        if !supported(GraphicsCaptureApi::is_supported) {
            return Err(CaptureError::Unsupported);
        }

        let cursor = match (
            config.show_cursor,
            supported(GraphicsCaptureApi::is_cursor_settings_supported),
        ) {
            (_, false) => CursorCaptureSettings::Default,
            (true, true) => CursorCaptureSettings::WithCursor,
            (false, true) => CursorCaptureSettings::WithoutCursor,
        };
        // Viền vàng của WGC là thứ người ngồi tại host phải nhìn suốt phiên
        // điều khiển — tắt đi ở những bản Windows cho phép.
        let border = if supported(GraphicsCaptureApi::is_border_settings_supported) {
            DrawBorderSettings::WithoutBorder
        } else {
            DrawBorderSettings::Default
        };
        // Chặn trên của tốc độ frame. WGC vốn chỉ giao frame khi màn hình đổi,
        // nên đây là trần chứ không phải nhịp cố định.
        let interval = if supported(GraphicsCaptureApi::is_minimum_update_interval_supported) {
            MinimumUpdateIntervalSettings::Custom(Duration::from_micros(
                1_000_000 / config.target_fps.max(1) as u64,
            ))
        } else {
            MinimumUpdateIntervalSettings::Default
        };

        let slot = FrameSlot::new();
        let settings = Settings::new(
            monitor,
            cursor,
            border,
            SecondaryWindowSettings::Default,
            interval,
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            Flags {
                slot: Arc::clone(&slot),
                ring_size: (config.queue_depth as usize).max(MIN_RING),
            },
        );

        let control = Handler::start_free_threaded(settings).map_err(classify_start)?;
        Ok(Self {
            control: Some(control),
            slot,
            display,
        })
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<CapturedFrame> {
        self.slot.take(timeout)
    }

    fn stats(&self) -> CaptureStats {
        // WGC không giao frame nào khi màn hình đứng yên, nên `frames_idle`
        // luôn là 0 ở đây — khác macOS, nơi hệ thống vẫn đẩy frame kèm cờ
        // "không đổi".
        self.slot.stats()
    }

    fn stop(&mut self) {
        let Some(control) = self.control.take() else {
            return;
        };
        if let Err(err) = control.stop() {
            tracing::warn!(%err, "không dừng gọn được luồng capture");
        }
        self.slot.stop(None);
    }
}

impl Drop for WgcScreenCapturer {
    fn drop(&mut self) {
        self.stop();
    }
}
