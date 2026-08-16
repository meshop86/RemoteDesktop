//! Chụp màn hình với đường đi zero-copy.
//!
//! Nguyên tắc xuyên suốt: **frame không bao giờ đi qua CPU**. Trên macOS,
//! ScreenCaptureKit trả về `CVPixelBuffer` nằm trên IOSurface (bộ nhớ chia sẻ
//! GPU); ta chuyển thẳng handle đó cho VideoToolbox. Trên Windows, Windows
//! Graphics Capture trả `ID3D11Texture2D` và đi thẳng vào encoder.
//!
//! Vì vậy kiểu [`Surface`] là kiểu riêng của từng nền tảng, không phải một
//! buffer chung — cố ép chúng về một `Vec<u8>` chung sẽ phá vỡ zero-copy và
//! cộng thêm hàng chục mili giây cho mỗi frame 4K.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
pub mod macos;

// Nguồn tổng hợp chạy trên cả hai nền tảng: viewer dùng nó khi chưa có quyền
// quay màn hình, và đó là tình huống xảy ra ở cả macOS lẫn Windows.
pub mod synthetic;

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "macos")]
pub use macos::{MacScreenCapturer as PlatformCapturer, PixelSurface as Surface};

#[cfg(target_os = "windows")]
pub use windows::{D3dSurface as Surface, WgcScreenCapturer as PlatformCapturer};

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("hệ điều hành từ chối quyền quay màn hình — cấp quyền trong System Settings > Privacy & Security > Screen Recording rồi chạy lại")]
    PermissionDenied,
    #[error("không tìm thấy màn hình có id {0}")]
    DisplayNotFound(u32),
    #[error("chưa có frame nào trong thời gian chờ")]
    Timeout,
    #[error("luồng capture đã dừng")]
    Stopped,
    #[error("lỗi từ hệ thống: {0}")]
    Platform(String),
    #[error("nền tảng này chưa được hỗ trợ")]
    Unsupported,
}

pub type Result<T> = std::result::Result<T, CaptureError>;

#[derive(Debug, Clone, PartialEq)]
pub struct DisplayInfo {
    pub id: u32,
    pub name: String,
    /// Kích thước tính bằng pixel thật (đã nhân scale Retina).
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    pub refresh_hz: u32,
    pub is_primary: bool,
}

/// Định dạng pixel mà capture trả về.
///
/// `Bgra32` giữ nguyên màu từng pixel nên chữ nét nhất và cho phép encoder
/// chọn 4:4:4 hoặc 4:2:2 về sau. `Nv12` đã bị giảm chroma ngay từ khâu capture,
/// đổi lại tiết kiệm băng thông bộ nhớ — dùng khi máy yếu hoặc mạng hẹp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra32,
    Nv12,
}

#[derive(Debug, Clone)]
pub struct CaptureConfig {
    pub display_id: u32,
    pub target_fps: u32,
    pub show_cursor: bool,
    pub pixel_format: PixelFormat,
    /// Giới hạn cạnh dài nhất (giữ tỉ lệ). `None` = giữ nguyên phân giải gốc.
    pub max_dimension: Option<u32>,
    /// Số frame hệ thống được phép giữ trong hàng đợi. Giá trị nhỏ = độ trễ
    /// thấp; ScreenCaptureKit khuyến nghị tối thiểu 3 để không sót frame.
    pub queue_depth: u32,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            display_id: 0,
            target_fps: 60,
            show_cursor: true,
            pixel_format: PixelFormat::Bgra32,
            max_dimension: None,
            queue_depth: 3,
        }
    }
}

/// Một frame vừa capture. `surface` là handle GPU, không phải dữ liệu pixel.
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    /// Micro giây kể từ Unix epoch, lấy ngay khi frame về tới tiến trình.
    pub capture_us: u64,
    pub frame_index: u64,
    pub surface: Surface,
}

impl std::fmt::Debug for CapturedFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapturedFrame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("capture_us", &self.capture_us)
            .field("frame_index", &self.frame_index)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CaptureStats {
    pub frames_delivered: u64,
    /// Frame bị bỏ vì phía tiêu thụ (encoder) chưa lấy kịp frame trước.
    pub frames_dropped: u64,
    /// Frame hệ thống báo "không có gì thay đổi" — bỏ qua, không tốn encode.
    pub frames_idle: u64,
}

/// Ô nối luồng callback của hệ điều hành với luồng encode.
///
/// Ô chỉ chứa **một** phần tử: frame mới tới mà encoder chưa lấy kịp frame cũ
/// thì frame cũ bị vứt và đếm vào `frames_dropped`. Đây là lựa chọn có chủ đích
/// cho điều khiển từ xa — hiển thị hình mới nhất quan trọng hơn hiển thị đủ mọi
/// hình. Xếp hàng thay vì vứt chỉ làm độ trễ dồn lên mãi.
///
/// Generic theo phần tử để test được mà không cần dựng một frame GPU thật.
pub struct FrameSlot<T> {
    state: Mutex<SlotState<T>>,
    ready: Condvar,
}

struct SlotState<T> {
    item: Option<T>,
    stats: CaptureStats,
    stopped: bool,
    error: Option<String>,
}

impl<T> Default for SlotState<T> {
    fn default() -> Self {
        Self {
            item: None,
            stats: CaptureStats::default(),
            stopped: false,
            error: None,
        }
    }
}

impl<T> FrameSlot<T> {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(SlotState::default()),
            ready: Condvar::new(),
        })
    }

    /// Đặt frame mới vào ô, đè lên frame chưa ai lấy nếu có.
    pub fn put(&self, item: T) {
        let mut state = self.lock();
        if state.item.is_some() {
            state.stats.frames_dropped += 1;
        }
        state.stats.frames_delivered += 1;
        state.item = Some(item);
        drop(state);
        self.ready.notify_one();
    }

    /// Ghi nhận một frame hệ thống báo "không có gì đổi".
    pub fn note_idle(&self) {
        self.lock().stats.frames_idle += 1;
    }

    /// Đóng ô. Mọi bên đang chờ được đánh thức ngay thay vì ngồi hết timeout.
    pub fn stop(&self, error: Option<String>) {
        let mut state = self.lock();
        state.stopped = true;
        if error.is_some() {
            state.error = error;
        }
        drop(state);
        self.ready.notify_all();
    }

    pub fn stats(&self) -> CaptureStats {
        self.lock().stats
    }

    /// Chờ tới khi có frame, ô đóng, hoặc hết `timeout`.
    pub fn take(&self, timeout: Duration) -> Result<T> {
        // Hạn chót tuyệt đối: `wait_timeout` có thể tỉnh dậy vô cớ, và cấp lại
        // nguyên `timeout` mỗi vòng thì hàm này chờ lâu hơn bên gọi cho phép.
        let deadline = Instant::now() + timeout;
        let mut state = self.lock();
        loop {
            if let Some(item) = state.item.take() {
                return Ok(item);
            }
            // Xét sau `item`: frame cuối cùng vẫn phải giao được dù luồng
            // capture đã dừng ngay sau đó.
            if state.stopped {
                return Err(match state.error.take() {
                    Some(message) => CaptureError::Platform(message),
                    None => CaptureError::Stopped,
                });
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(CaptureError::Timeout);
            };
            let (next, wait) = self
                .ready
                .wait_timeout(state, remaining)
                .expect("slot mutex bị poison");
            state = next;
            if wait.timed_out() && state.item.is_none() && !state.stopped {
                return Err(CaptureError::Timeout);
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SlotState<T>> {
        self.state.lock().expect("slot mutex bị poison")
    }
}

pub trait ScreenCapturer: Sized {
    fn list_displays() -> Result<Vec<DisplayInfo>>;
    fn start(config: CaptureConfig) -> Result<Self>;
    /// Chờ frame mới nhất. Luôn trả về frame *mới nhất*, không phải frame cũ
    /// trong hàng đợi — frame cũ đã hết giá trị với điều khiển thời gian thực.
    fn next_frame(&mut self, timeout: Duration) -> Result<CapturedFrame>;
    fn stats(&self) -> CaptureStats;
    fn stop(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_moi_de_len_frame_chua_ai_lay() {
        let slot = FrameSlot::new();
        slot.put(1u32);
        slot.put(2u32);

        // Lấy ra phải là frame *mới nhất*, không phải frame đầu hàng đợi.
        assert_eq!(slot.take(Duration::ZERO).expect("có frame"), 2);
        let stats = slot.stats();
        assert_eq!(stats.frames_delivered, 2);
        assert_eq!(stats.frames_dropped, 1);
    }

    #[test]
    fn o_rong_thi_bao_het_gio() {
        let slot = FrameSlot::<u32>::new();
        assert!(matches!(
            slot.take(Duration::from_millis(10)),
            Err(CaptureError::Timeout)
        ));
    }

    /// Luồng capture dừng giữa chừng thì frame cuối vẫn phải giao được — vứt nó
    /// đi là mất đúng khung hình cuối người dùng nhìn thấy.
    #[test]
    fn dung_luong_van_giao_not_frame_cuoi() {
        let slot = FrameSlot::new();
        slot.put(7u32);
        slot.stop(None);

        assert_eq!(slot.take(Duration::ZERO).expect("còn frame cuối"), 7);
        assert!(matches!(
            slot.take(Duration::ZERO),
            Err(CaptureError::Stopped)
        ));
    }

    #[test]
    fn dung_kem_loi_thi_bao_lai_loi_do() {
        let slot = FrameSlot::<u32>::new();
        slot.stop(Some("màn hình bị rút".into()));
        let Err(CaptureError::Platform(message)) = slot.take(Duration::ZERO) else {
            panic!("phải trả về lỗi của nền tảng");
        };
        assert_eq!(message, "màn hình bị rút");
    }

    /// Bên chờ phải được đánh thức ngay khi có frame, không phải ngồi hết
    /// timeout. Timeout dài hơn hẳn thời gian ngủ nên test không phụ thuộc tốc
    /// độ máy.
    #[test]
    fn cho_duoc_danh_thuc_khi_co_frame() {
        let slot = FrameSlot::new();
        let writer = Arc::clone(&slot);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            writer.put(42u32);
        });

        let started = Instant::now();
        assert_eq!(slot.take(Duration::from_secs(5)).expect("có frame"), 42);
        assert!(started.elapsed() < Duration::from_secs(1), "chờ quá lâu");
    }
}
