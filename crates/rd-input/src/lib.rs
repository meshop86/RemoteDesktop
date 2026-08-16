//! Bơm sự kiện chuột/bàn phím vào máy host.
//!
//! Viewer gửi [`rd_protocol::InputEvent`] với toạ độ chuẩn hoá [0,1]; crate này
//! đổi sang toạ độ pixel của màn hình host rồi đẩy vào hệ điều hành.
//!
//! Hai điểm cần cẩn thận, đều là nguồn lỗi kinh điển của phần mềm điều khiển
//! từ xa:
//!
//! 1. **Phím kẹt.** Nếu viewer mất kết nối lúc đang giữ Cmd, host sẽ giữ Cmd
//!    mãi mãi. Vì vậy [`InputInjector`] tự nhớ những gì đang được giữ và
//!    [`InputEvent::ReleaseAll`] nhả sạch; `Drop` cũng làm việc đó.
//! 2. **Cờ modifier.** macOS không suy ra Shift/Cmd từ việc phím đó đang xuống —
//!    mỗi sự kiện phải tự mang cờ đi kèm, nếu không Cmd+C sẽ thành C. Windows
//!    thì ngược lại, tự theo dõi trạng thái bàn phím nên không cần cờ.

#[cfg(target_os = "macos")]
pub mod keymap_macos;
#[cfg(target_os = "macos")]
pub mod macos;

// Bảng scan code là dữ liệu thuần, không gọi API nào. Để không cfg-gate thì
// test của nó chạy được ngay trên máy dev macOS, thay vì chỉ khi build Windows.
pub mod keymap_windows;
#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "macos")]
pub use macos::CgInjector as PlatformInjector;
#[cfg(target_os = "windows")]
pub use windows::SendInputInjector as PlatformInjector;

use rd_protocol::{InputEvent, KeyCode, MouseButton};

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error(
        "hệ điều hành từ chối quyền điều khiển — cấp quyền trong System Settings > Privacy & Security > Accessibility rồi chạy lại"
    )]
    PermissionDenied,
    #[error("không tạo được sự kiện: {0}")]
    Platform(String),
    #[error("mã phím {0:?} không có trên nền tảng này")]
    UnsupportedKey(KeyCode),
}

pub type Result<T> = std::result::Result<T, InputError>;

/// Kích thước màn hình host, để đổi toạ độ chuẩn hoá thành pixel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenGeometry {
    pub width: f32,
    pub height: f32,
}

impl ScreenGeometry {
    /// Ép về trong màn hình rồi đổi sang pixel. Viewer có thể gửi toạ độ hơi
    /// lố ra ngoài khi chuột đi ra mép cửa sổ, không nên để nó thành toạ độ âm.
    pub fn to_pixels(self, x: f32, y: f32) -> (f64, f64) {
        let x = x.clamp(0.0, 1.0) * (self.width - 1.0).max(0.0);
        let y = y.clamp(0.0, 1.0) * (self.height - 1.0).max(0.0);
        (x as f64, y as f64)
    }
}

/// Những gì đang được giữ. Tách riêng khỏi phần phụ thuộc hệ điều hành để test
/// được logic chống phím kẹt mà không cần quyền hệ thống.
#[derive(Debug, Default, Clone)]
pub struct HeldState {
    keys: Vec<KeyCode>,
    buttons: Vec<MouseButton>,
}

impl HeldState {
    /// Ghi nhận một phím đổi trạng thái. Trả về `false` nếu sự kiện là thừa
    /// (nhấn phím đang nhấn, nhả phím đang nhả) — khi đó không nên bơm đi.
    ///
    /// Lọc trùng không phải để tiết kiệm: bàn phím thật tự lặp phím khi giữ, và
    /// nếu ta bơm lại sự kiện nhấn thì host nhận lặp hai lần.
    pub fn set_key(&mut self, code: KeyCode, pressed: bool) -> bool {
        let index = self.keys.iter().position(|held| *held == code);
        match (pressed, index) {
            (true, None) => {
                self.keys.push(code);
                true
            }
            (false, Some(index)) => {
                self.keys.remove(index);
                true
            }
            _ => false,
        }
    }

    pub fn set_button(&mut self, button: MouseButton, pressed: bool) -> bool {
        let index = self.buttons.iter().position(|held| *held == button);
        match (pressed, index) {
            (true, None) => {
                self.buttons.push(button);
                true
            }
            (false, Some(index)) => {
                self.buttons.remove(index);
                true
            }
            _ => false,
        }
    }

    pub fn keys(&self) -> &[KeyCode] {
        &self.keys
    }

    pub fn buttons(&self) -> &[MouseButton] {
        &self.buttons
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.buttons.is_empty()
    }

    /// Lấy ra mọi thứ đang giữ và xoá sổ, để bên gọi bơm sự kiện nhả.
    pub fn drain(&mut self) -> (Vec<KeyCode>, Vec<MouseButton>) {
        (
            std::mem::take(&mut self.keys),
            std::mem::take(&mut self.buttons),
        )
    }
}

/// Gom phần lẻ của cuộn lại giữa các sự kiện.
///
/// Có hệ điều hành chỉ nhận lượng cuộn là **số nguyên** (Windows đo theo "nấc",
/// mỗi nấc 120 đơn vị). Trackpad thì gửi rất nhiều bước nhỏ: làm tròn từng bước
/// một sẽ ra 0 mãi và trang đứng im. Giữ phần dư lại thì nhiều bước nhỏ cộng
/// dồn được thành một nấc.
#[derive(Debug, Default, Clone)]
pub struct WheelAccumulator {
    x: f32,
    y: f32,
}

impl WheelAccumulator {
    /// Nhận thêm một lượng cuộn (đã quy về đơn vị của hệ điều hành) và trả về
    /// phần nguyên gửi đi được. Phần lẻ ở lại cho lần sau.
    pub fn push(&mut self, delta_x: f32, delta_y: f32) -> (i32, i32) {
        // NaN/vô cực làm hỏng bộ tích luỹ vĩnh viễn: một lần dính là mọi lần
        // cuộn sau đều ra NaN. Bỏ qua thì tệ nhất chỉ mất một sự kiện.
        if !delta_x.is_finite() || !delta_y.is_finite() {
            return (0, 0);
        }
        self.x += delta_x;
        self.y += delta_y;
        let whole_x = self.x.trunc();
        let whole_y = self.y.trunc();
        self.x -= whole_x;
        self.y -= whole_y;
        (whole_x as i32, whole_y as i32)
    }
}

pub trait InputInjector {
    /// Mở kênh bơm sự kiện. Lỗi ở đây gần như luôn là thiếu quyền hệ thống.
    fn open(geometry: ScreenGeometry) -> Result<Self>
    where
        Self: Sized;

    /// Đổi kích thước màn hình host giữa chừng (người dùng cắm thêm màn hình,
    /// đổi phân giải). Toạ độ chuẩn hoá không đổi nên chỉ cần cập nhật ở đây.
    fn set_geometry(&mut self, geometry: ScreenGeometry);

    fn inject(&mut self, event: &InputEvent) -> Result<()>;

    /// Nhả mọi phím và nút đang giữ.
    fn release_all(&mut self) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loc_su_kien_trung_lap() {
        let mut held = HeldState::default();
        assert!(held.set_key(KeyCode::A, true));
        // Bàn phím thật lặp phím khi giữ — lần thứ hai phải bị chặn.
        assert!(!held.set_key(KeyCode::A, true));
        assert!(held.set_key(KeyCode::A, false));
        assert!(!held.set_key(KeyCode::A, false));
        assert!(held.is_empty());
    }

    #[test]
    fn drain_tra_ve_moi_thu_dang_giu() {
        let mut held = HeldState::default();
        held.set_key(KeyCode::MetaLeft, true);
        held.set_key(KeyCode::C, true);
        held.set_button(MouseButton::Left, true);

        let (keys, buttons) = held.drain();
        assert_eq!(keys, vec![KeyCode::MetaLeft, KeyCode::C]);
        assert_eq!(buttons, vec![MouseButton::Left]);
        assert!(held.is_empty());
    }

    /// Nhiều bước nhỏ dưới ngưỡng phải cộng dồn lại thành một nấc, không được
    /// làm tròn về 0 từng cái một — nếu không thì cuộn bằng trackpad đứng im.
    #[test]
    fn cuon_nho_cong_don_thanh_nac() {
        let mut wheel = WheelAccumulator::default();
        for _ in 0..3 {
            assert_eq!(wheel.push(0.0, 0.3), (0, 0));
        }
        // 0.3 * 4 = 1.2 → nhả ra 1, giữ lại 0.2.
        assert_eq!(wheel.push(0.0, 0.3), (0, 1));
        assert_eq!(wheel.push(0.0, 0.3), (0, 0));
    }

    /// Cuộn ngược lại cũng phải gom được, và phần dư không được lẫn giữa hai
    /// trục.
    #[test]
    fn hai_truc_cuon_doc_lap_va_co_dau() {
        let mut wheel = WheelAccumulator::default();
        assert_eq!(wheel.push(0.6, -0.6), (0, 0));
        assert_eq!(wheel.push(0.6, -0.6), (1, -1));

        // Một giá trị hỏng không được đầu độc phần dư đang giữ.
        let mut wheel = WheelAccumulator::default();
        wheel.push(0.0, 0.5);
        assert_eq!(wheel.push(0.0, f32::NAN), (0, 0));
        assert_eq!(wheel.push(0.0, 0.5), (0, 1));
    }

    #[test]
    fn toa_do_chuan_hoa_ra_pixel() {
        let geometry = ScreenGeometry {
            width: 1920.0,
            height: 1080.0,
        };
        assert_eq!(geometry.to_pixels(0.0, 0.0), (0.0, 0.0));
        assert_eq!(geometry.to_pixels(1.0, 1.0), (1919.0, 1079.0));
        // Chuột đi lố ra ngoài cửa sổ viewer không được thành toạ độ âm.
        assert_eq!(geometry.to_pixels(-0.5, 2.0), (0.0, 1079.0));
    }
}
