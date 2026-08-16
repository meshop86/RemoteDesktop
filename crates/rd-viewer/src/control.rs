//! Bật/tắt điều khiển và bơm sự kiện xuống máy host.
//!
//! Tách khỏi `main.rs` vì phần khoá an toàn ở đây đáng chú ý hơn phần hiển thị.
//! Hiện host và viewer là cùng một máy: sự kiện bơm ra rơi ngay vào cửa sổ này,
//! được bắt lại rồi bơm tiếp — một vòng lặp mà chính chuột và bàn phím cũng
//! không cắt được nữa. Nên mặc định là tắt, và muốn bật còn phải tự đặt biến
//! môi trường; khi phần mạng xong thì điều kiện này thay bằng "host ở máy khác".

use rd_input::{InputInjector as _, PlatformInjector, ScreenGeometry};
use rd_protocol::InputEvent;

/// Biến môi trường mở khoá bơm input khi host trùng viewer.
const LOCAL_OVERRIDE: &str = "RD_LOCAL_INPUT";

pub struct RemoteControl {
    /// Mở một lần rồi giữ: mở lại tốn một lần xin quyền hệ thống.
    injector: Option<PlatformInjector>,
    enabled: bool,
    /// Câu trạng thái cho HUD. Bật không được thì phải nói rõ vì sao, nếu không
    /// người dùng chỉ thấy phím tắt "không ăn".
    note: String,
    errors: u32,
}

impl Default for RemoteControl {
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteControl {
    pub fn new() -> Self {
        Self {
            injector: None,
            enabled: false,
            note: "tắt".into(),
            errors: 0,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn note(&self) -> &str {
        &self.note
    }

    pub fn errors(&self) -> u32 {
        self.errors
    }

    /// Đổi trạng thái. Trả về trạng thái sau khi đổi — bật có thể thất bại.
    pub fn set_enabled(&mut self, on: bool) -> bool {
        if !on {
            self.enabled = false;
            self.note = "tắt".into();
            tracing::info!("tắt điều khiển từ xa");
            return false;
        }

        if std::env::var_os(LOCAL_OVERRIDE).is_none() {
            self.note = format!("khoá — host trùng viewer, đặt {LOCAL_OVERRIDE}=1 để thử");
            tracing::warn!("{}", self.note);
            return false;
        }

        if self.injector.is_none() {
            let geometry = match host_geometry() {
                Ok(geometry) => geometry,
                Err(err) => {
                    self.note = err.to_string();
                    tracing::warn!(%err, "không đọc được kích thước màn hình host");
                    return false;
                }
            };
            match PlatformInjector::open(geometry) {
                Ok(injector) => self.injector = Some(injector),
                Err(err) => {
                    self.note = err.to_string();
                    tracing::warn!(%err, "không mở được kênh bơm input");
                    return false;
                }
            }
        }

        self.enabled = true;
        self.note = "bật".into();
        tracing::info!("bật điều khiển từ xa");
        true
    }

    /// Bơm một loạt sự kiện. Lỗi của từng sự kiện không cắt cả loạt: bỏ dở giữa
    /// chừng dễ để lại phím đang giữ mà không bao giờ được nhả.
    pub fn send(&mut self, events: &[InputEvent]) {
        let Some(injector) = self.injector.as_mut() else {
            return;
        };
        for event in events {
            if let Err(err) = injector.inject(event) {
                self.errors += 1;
                tracing::warn!(%err, ?event, "bơm sự kiện thất bại");
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn host_geometry() -> rd_input::Result<ScreenGeometry> {
    rd_input::macos::main_screen_geometry()
}

#[cfg(target_os = "windows")]
fn host_geometry() -> rd_input::Result<ScreenGeometry> {
    rd_input::windows::main_screen_geometry()
}
